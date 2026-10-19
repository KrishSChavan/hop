//! The agent's core thread (docs/M2-DESIGN.md, "The agent"): it owns the
//! hand-off [`Core`] and the platform, feeds the core every event, and
//! carries out what the core decides.
//!
//! - **Events.** Captured input and network events arrive on separate
//!   bounded queues, so traffic from a peer can never crowd out local input.
//!   Capture's sink only queues the event and rings the core thread; when the
//!   input queue is full it drops the event and raises a flag, and the core
//!   thread then feeds `LocalStall`. The network side closes a connection
//!   whose queue is full, so no reliable message is ever dropped.
//! - **Ticks.** The core thread waits on its doorbell with a timeout, so a
//!   `Tick` runs every 10 ms even when nothing arrives, and ticks never queue
//!   up behind events.
//! - **The lease.** While the core needs input suppressed (entering or
//!   controlling), every tick first reads the capture mode: if the backend
//!   already went back to passive, the lease lapsed (this thread stalled for
//!   about a second) and the core gets `LocalStall`; otherwise the lease is
//!   renewed. Only this thread renews it, so a stalled core thread can never
//!   keep input suppressed. The same check runs before queued input is
//!   handled, because input captured after a lapse already reached this
//!   computer's own apps and must not be forwarded too.
//! - **Failures.** A platform call a session depends on that fails
//!   (suppressing, parking, moving the cursor, injecting) feeds `LocalStall`
//!   too, which ends any session failing open.
//! - **Quitting** ([`Agent::quit`]): input comes back first, then the
//!   network says goodbye (the caller shuts the link down afterwards). A
//!   panic on this thread unwinds through the [`Executor`], whose `Drop`
//!   gives input back the same way.
//! - **Updates for an app** ([`Agent::report_to`]): connections and session
//!   changes also go to an unbounded channel, with a `send` that never
//!   waits, so a slow app can never hold this thread up.

pub(crate) mod exec;
pub(crate) mod link;
pub(crate) mod start;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use hop_core::{CaptureMode, Core, CoreConfig, Event, Layout, Millis, State};
use hop_keymap::ModifierPolicy;
use hop_net::NetEvent;
use hop_platform::{CaptureMode as PlatformMode, CapturedEvent, Platform, PlatformError};
use hop_types::{DeviceId, DisplayInfo, Edge, Os};

use self::exec::Executor;
use self::link::Link;
use crate::service::{AgentUpdate, Session};
use crate::status;

/// How often the core gets a `Tick`.
pub(crate) const TICK: Duration = Duration::from_millis(10);

/// Captured events waiting for the core thread. A second of fast mouse
/// input fits; more means the core thread is stuck, and then input must come
/// back to the user rather than pile up.
pub(crate) const INPUT_QUEUE: usize = 1024;

/// Events handled from one queue before the other queue and the tick get a
/// turn.
const BATCH: usize = 256;

/// How often this computer's displays are checked for changes.
const DISPLAY_CHECK: Duration = Duration::from_secs(3);

/// Flags other threads raise for the core thread, and its doorbell.
///
/// Everything here is safe to use from any thread and never blocks, so the
/// capture sink, the network and the signal handler can all ring.
#[derive(Debug, Default)]
pub(crate) struct Signals {
    quit: AtomicBool,
    input_lost: AtomicBool,
    panicked: AtomicBool,
    core: OnceLock<Thread>,
}

impl Signals {
    /// Makes the calling thread the one [`Signals::ring`] wakes. Only the
    /// first call counts.
    pub(crate) fn attach(&self) {
        let _ = self.core.set(thread::current());
    }

    /// Wakes the core thread. Before [`Signals::attach`] this does nothing,
    /// which costs at most one tick: the core thread looks at its queues
    /// at every tick anyway.
    pub(crate) fn ring(&self) {
        if let Some(core) = self.core.get() {
            core.unpark();
        }
    }

    /// Asks the core thread to quit, failing open.
    pub(crate) fn request_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.ring();
    }

    /// Whether quitting was asked for.
    pub(crate) fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }

    /// Records that captured input was dropped (the input queue was full).
    pub(crate) fn input_lost(&self) {
        self.input_lost.store(true, Ordering::SeqCst);
    }

    /// Records that some thread panicked, so Hop exits with a failure.
    pub(crate) fn mark_panicked(&self) {
        self.panicked.store(true, Ordering::SeqCst);
    }

    /// Whether some thread panicked.
    pub(crate) fn panicked(&self) -> bool {
        self.panicked.load(Ordering::SeqCst)
    }
}

/// The paired computer and where it sits.
#[derive(Clone, Debug)]
pub(crate) struct PeerPlacement {
    /// Its device id.
    pub(crate) id: DeviceId,
    /// Its name, for status lines until it says its own.
    pub(crate) name: String,
    /// The edge of this computer's screens that leads to it.
    pub(crate) side: Edge,
}

/// What the core thread runs with.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// This device's id.
    pub(crate) me: DeviceId,
    /// This device's OS, as the core and the key mapping see it. Normally
    /// `Os::current()`; tests of the Cmd/Ctrl swap on one machine change it.
    pub(crate) os: Os,
    /// The hand-off settings.
    pub(crate) core: CoreConfig,
    /// The Cmd/Ctrl swap for injected keys.
    pub(crate) policy: ModifierPolicy,
    /// The paired computer.
    pub(crate) peer: PeerPlacement,
}

/// Why the core thread could not start.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    /// The platform could not list this computer's displays.
    #[error("could not read this computer's displays")]
    Displays(#[source] PlatformError),
    /// Capture could not start (on a Mac, usually the Accessibility
    /// permission).
    #[error("could not start watching this computer's keyboard and mouse")]
    Capture(#[source] PlatformError),
}

/// The core thread's state: the core, the executor and the input queue.
pub(crate) struct Agent<L: Link> {
    core: Core,
    exec: Executor<L>,
    input: Receiver<CapturedEvent>,
    signals: Arc<Signals>,
    started: Instant,
    me: DeviceId,
    peer: PeerPlacement,
    displays: Vec<DisplayInfo>,
    next_display_check: Instant,
    told_local_network: bool,
    updates: Option<Updates>,
}

/// Where the core thread tells an app what is happening.
struct Updates {
    tx: Sender<AgentUpdate>,
    /// The session kind last reported, so only changes go out.
    session: Session,
}

impl Updates {
    /// Never waits: the channel is unbounded. A gone app is no error; the
    /// agent runs on until it is asked to quit.
    fn send(&self, update: AgentUpdate) {
        let _ = self.tx.send(update);
    }

    /// Reports `now` if it is not the kind reported last.
    fn session(&mut self, now: Session) {
        if now != self.session {
            self.session = now;
            self.send(AgentUpdate::Session(now));
        }
    }
}

/// The session kind an app shows for `state`. Entering counts as
/// controlling: input is already held back from this computer's apps.
fn session_of(state: &State) -> Session {
    match state {
        State::Idle => Session::Idle,
        State::Entering { .. } | State::Controlling { .. } => Session::Controlling,
        State::Controlled { .. } => Session::Controlled,
    }
}

impl<L: Link> Agent<L> {
    /// Builds the core from `settings` and this computer's displays, and
    /// starts capture. Call it on the core thread: from here on, every
    /// platform call happens on the thread that owns the agent.
    ///
    /// # Errors
    ///
    /// [`AgentError`] if the displays cannot be read or capture cannot
    /// start. Nothing has been suppressed or injected then.
    pub(crate) fn start(
        mut platform: Platform,
        link: L,
        settings: Settings,
        signals: Arc<Signals>,
    ) -> Result<Agent<L>, AgentError> {
        let displays = platform.displays.displays().map_err(AgentError::Displays)?;
        if displays.is_empty() {
            status!("Hop found no screens on this computer, so the cursor cannot cross yet.");
        }
        let layout = build_layout(&settings.me, &displays, &settings.peer);
        let core = Core::new(settings.me.clone(), settings.os, settings.core, layout);
        let (tx, rx) = mpsc::sync_channel(INPUT_QUEUE);
        let sink_signals = signals.clone();
        platform
            .capture
            .start(Box::new(move |event| {
                // Never blocks: the capture thread must hand events off and
                // return at once.
                if tx.try_send(event).is_err() {
                    sink_signals.input_lost();
                }
                sink_signals.ring();
            }))
            .map_err(AgentError::Capture)?;
        let mut exec = Executor::new(platform, link, settings.os, settings.policy);
        exec.set_peer_name(settings.peer.id.clone(), settings.peer.name.clone());
        Ok(Agent {
            core,
            exec,
            input: rx,
            signals,
            started: Instant::now(),
            me: settings.me,
            peer: settings.peer,
            displays,
            next_display_check: Instant::now() + DISPLAY_CHECK,
            told_local_network: false,
            updates: None,
        })
    }

    /// From now on, also tells `updates` about connections
    /// ([`AgentUpdate::Connected`], [`AgentUpdate::Disconnected`]) and
    /// session changes ([`AgentUpdate::Session`], only when the kind
    /// changes). Sending never waits.
    pub(crate) fn report_to(&mut self, updates: Sender<AgentUpdate>) {
        let session = session_of(&self.core.state());
        self.updates = Some(Updates {
            tx: updates,
            session,
        });
    }

    /// Runs until [`Signals::request_quit`]. Call [`Agent::quit`] next.
    pub(crate) fn run(&mut self) {
        self.signals.attach();
        let mut next_tick = Instant::now();
        let mut busy = false;
        loop {
            if !busy {
                let wait = next_tick.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    // Woken early by the doorbell; spurious wake-ups only
                    // cost a look at the queues.
                    thread::park_timeout(wait);
                }
            }
            if self.signals.quit_requested() {
                return;
            }
            busy = self.poll();
            let now = Instant::now();
            if now >= next_tick {
                self.tick();
                // From now, not from the missed deadline: ticks never queue.
                next_tick = now + TICK;
            }
        }
    }

    /// Handles what is waiting: captured input (and lost input), then
    /// network events. Returns true if a queue still had more than one
    /// batch, so the caller should come back without waiting.
    pub(crate) fn poll(&mut self) -> bool {
        let mut busy = false;
        // Checked before any queued input is handled, not only at the next
        // tick: once the lease has lapsed, the backend lets physical input
        // through to this computer's own apps, so input queued since then
        // was already typed here and must not be forwarded to the peer as
        // well. Input from before the lapse is lost with the session, which
        // is safe (the peer releases what it injected).
        self.end_session_if_lease_lapsed();
        if self.signals.input_lost.swap(false, Ordering::SeqCst) {
            // What was queued before the loss goes to the core first; then
            // the core hears that some input after it never arrived. That
            // order ends a session and stops the core vouching for any key
            // whose key-up may be among the lost events, so it can never be
            // pressed for the peer in the next crossing.
            for _ in 0..2 * INPUT_QUEUE {
                let Ok(captured) = self.input.try_recv() else {
                    break;
                };
                self.on_input(captured);
            }
            tracing::warn!(
                "captured input was dropped because Hop fell behind; giving the keyboard and \
                 mouse back"
            );
            self.feed(Event::LocalStall);
        } else {
            for handled in 1..=BATCH {
                let Ok(captured) = self.input.try_recv() else {
                    break;
                };
                self.on_input(captured);
                busy |= handled == BATCH;
            }
        }
        for handled in 1..=BATCH {
            let Some(event) = self.exec.link.try_recv() else {
                break;
            };
            self.on_net_event(event);
            busy |= handled == BATCH;
        }
        busy
    }

    /// One tick: timers, the lease, and the slower checks.
    pub(crate) fn tick(&mut self) {
        self.feed(Event::Tick);
        self.renew_lease();
        let now = Instant::now();
        if now >= self.next_display_check {
            self.next_display_check = now + DISPLAY_CHECK;
            self.check_displays();
            self.check_local_network();
        }
    }

    /// Quits failing open, in the design's order: input comes back first
    /// (a session ends the way a stall ends it: `Release` or `TakeBack`
    /// queued for the peer, the cursor restored where it left, capture
    /// passive, injected keys released), then capture stops. The caller
    /// then shuts the network down, which delivers the goodbye.
    pub(crate) fn quit(&mut self) {
        self.exec.begin_quit();
        if self.core.state() != State::Idle {
            self.feed(Event::LocalStall);
        }
        self.exec.fail_open();
        self.exec.platform.capture.stop();
    }

    /// What the core is doing.
    #[cfg(test)]
    pub(crate) fn state(&self) -> State {
        self.core.state()
    }

    fn millis(&self) -> Millis {
        Millis::try_from(self.started.elapsed().as_millis()).unwrap_or(Millis::MAX)
    }

    /// Hands `event` to the core and carries out its actions. If a platform
    /// call the session depends on failed, the core hears `LocalStall`
    /// right away; a failure while carrying out that stall's own actions is
    /// only logged, so failing open can never loop.
    fn feed(&mut self, event: Event) {
        let now = self.millis();
        let actions = self.core.handle(now, event);
        if self.exec.execute(actions).stalled {
            let actions = self.core.handle(now, Event::LocalStall);
            let _ = self.exec.execute(actions);
        }
        // Every change of state goes through here, so this sees them all.
        if let Some(updates) = &mut self.updates {
            updates.session(session_of(&self.core.state()));
        }
    }

    /// Tells the app, if one listens.
    fn report(&self, update: AgentUpdate) {
        if let Some(updates) = &self.updates {
            updates.send(update);
        }
    }

    fn on_input(&mut self, captured: CapturedEvent) {
        self.exec.last_cursor = captured.cursor;
        self.feed(Event::LocalInput {
            event: captured.event,
            cursor: captured.cursor,
            physical: captured.physical,
        });
    }

    fn on_net_event(&mut self, event: NetEvent) {
        match event {
            NetEvent::PeerUp(up) => {
                tracing::info!(
                    peer = %up.peer,
                    name = %up.name,
                    os = %up.os,
                    version = %up.app_version,
                    address = %up.remote,
                    dialled_by_us = up.dialled_by_us,
                    "connected"
                );
                status!("Connected to {} ({}).", up.name, up.os);
                self.report(AgentUpdate::Connected {
                    name: up.name.clone(),
                    os: up.os,
                });
                self.exec.set_peer_name(up.peer.clone(), up.name.clone());
                self.feed(Event::PeerConnected {
                    peer: up.peer,
                    os: up.os,
                    displays: up.displays,
                });
            }
            NetEvent::PeerDown { peer, reason } => {
                tracing::info!(peer = %peer, reason = %reason, "disconnected");
                if !self.exec.is_quitting() {
                    status!(
                        "Lost the connection to {} ({reason}). Hop keeps trying to reconnect.",
                        self.exec.name_of(&peer)
                    );
                }
                // Before the core hears it, so a session it ends is
                // reported after the connection.
                self.report(AgentUpdate::Disconnected);
                self.feed(Event::PeerDisconnected { peer });
            }
            NetEvent::Message { from, msg } => self.feed(Event::PeerMessage { from, msg }),
            // Files sent or received: the agent's own, from work package T5
            // on. The core never sees files.
            NetEvent::Transfer(_) => {}
        }
    }

    fn renew_lease(&mut self) {
        if self.core.state().capture_mode() != CaptureMode::Suppress
            || self.end_session_if_lease_lapsed()
        {
            return;
        }
        if let Err(error) = self.exec.platform.capture.set_mode(PlatformMode::Suppress) {
            tracing::warn!(error = %error, "could not renew input suppression");
            self.feed(Event::LocalStall);
        }
    }

    /// While the core needs input suppressed, a capture that reads Passive
    /// means the lease lapsed (this thread stalled for about a second) and
    /// the backend already gave input back: the core gets `LocalStall`
    /// rather than a renewal. Returns whether that happened.
    fn end_session_if_lease_lapsed(&mut self) -> bool {
        if self.core.state().capture_mode() != CaptureMode::Suppress
            || self.exec.platform.capture.mode() != PlatformMode::Passive
        {
            return false;
        }
        tracing::warn!(
            "the input lease lapsed: Hop stalled for about a second, so the keyboard and mouse \
             came back by themselves"
        );
        self.feed(Event::LocalStall);
        true
    }

    /// Picks up added, removed or moved displays. The peer learns about
    /// them when it next connects; until then it places the cursor by the
    /// old list, and the core moves an entry point that misses into the
    /// nearest display.
    fn check_displays(&mut self) {
        match self.exec.platform.displays.displays() {
            Ok(displays) if displays != self.displays => {
                tracing::info!(count = displays.len(), "this computer's displays changed");
                let layout = build_layout(&self.me, &displays, &self.peer);
                self.displays = displays.clone();
                self.feed(Event::LayoutChanged(layout));
                self.exec.link.set_displays(displays);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(error = %error, "could not check this computer's displays");
            }
        }
    }

    fn check_local_network(&mut self) {
        if !self.told_local_network && self.exec.link.local_network_denied() {
            self.told_local_network = true;
            status!(
                "macOS is blocking Hop from the local network. Turn Hop on in System Settings > \
                 Privacy & Security > Local Network, then quit Hop and start it again."
            );
        }
    }
}

/// The v1 layout: this computer, and the peer flush against `peer.side`.
/// The peer's displays arrive with its `Hello`.
fn build_layout(me: &DeviceId, displays: &[DisplayInfo], peer: &PeerPlacement) -> Layout {
    let place = |mine: Vec<DisplayInfo>| {
        Layout::two_devices(me.clone(), mine, peer.id.clone(), peer.side, Vec::new())
    };
    match place(displays.to_vec()) {
        Ok(layout) => layout,
        Err(error) => {
            // Without usable displays the cursor cannot cross, and the core
            // turns away a peer that tries to enter: safe, and visible.
            tracing::error!(error = %error, "this computer's displays are unusable");
            status!(
                "This computer's screens cannot be used ({error}), so the cursor cannot cross."
            );
            place(Vec::new()).unwrap_or_default()
        }
    }
}
