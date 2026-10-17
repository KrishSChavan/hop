//! A deterministic simulation of two Hop devices: two cores, a simulated
//! network and a simulated OS per device (cursor, capture, injection).
//!
//! The default world is the user's desk: a Mac (one 1512x982 display at scale
//! 2.0) with a Windows PC (one 1536x864 display at scale 1.25) to its right.
//!
//! Every call into a core goes through [`Sim::handle`], which checks the
//! invariants of docs/ARCHITECTURE.md after it, so every scenario and property
//! test also checks them continuously. The simulation also knows which session
//! each message really belongs to (the cores only see a `seq`), so it checks
//! that every message delivered outside its session changes nothing, and it
//! measures a session's silence by the messages of that session alone. It
//! also tracks what the user physically holds and what each OS believes is
//! held, so it checks that no key or button is ever left stuck, that the
//! emergency chord's last key never reaches the peer, and that crossings and
//! exits respect held buttons.

// Each test binary uses a different subset of the helpers.
#![allow(dead_code)]

use std::collections::BTreeSet;

use hop_core::{Action, CaptureMode, Core, CoreConfig, Event, Layout, Millis, Notice, State};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, InputEvent, KeyCode, MouseButton, Os, Point, Rect,
    ScrollUnit,
};

pub const MAC: usize = 0;
pub const PC: usize = 1;

/// A key with no special meaning ("A").
pub const KEY_A: KeyCode = KeyCode::keyboard(0x04);
/// "C", as in Cmd+C.
pub const KEY_C: KeyCode = KeyCode::keyboard(0x06);

pub fn display(id: u32, x: f64, y: f64, w: f64, h: f64, scale: f64) -> DisplayInfo {
    DisplayInfo {
        id,
        frame: Rect::new(x, y, w, h),
        scale,
        primary: x == 0.0 && y == 0.0,
    }
}

pub fn mac_laptop() -> DisplayInfo {
    display(1, 0.0, 0.0, 1512.0, 982.0, 2.0)
}

pub fn pc_screen() -> DisplayInfo {
    display(1, 0.0, 0.0, 1536.0, 864.0, 1.25)
}

pub fn key(code: KeyCode, pressed: bool) -> InputEvent {
    InputEvent::Key {
        code,
        pressed,
        repeat: false,
    }
}

pub fn click(button: MouseButton, pressed: bool) -> InputEvent {
    InputEvent::Button { button, pressed }
}

/// How the world is set up.
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub mac_id: &'static str,
    pub pc_id: &'static str,
    pub mac_displays: Vec<DisplayInfo>,
    pub pc_displays: Vec<DisplayInfo>,
    pub core: CoreConfig,
    /// One-way latency, drawn uniformly from this range for every message.
    pub latency: (Millis, Millis),
    /// Probability that a `Motion` datagram is lost.
    pub drop_motion: f64,
    pub seed: u64,
    /// How often both cores get a `Tick`.
    pub tick_ms: Millis,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            mac_id: "mac",
            pc_id: "pc",
            mac_displays: vec![mac_laptop()],
            pc_displays: vec![pc_screen()],
            core: CoreConfig::default(),
            latency: (5, 5),
            drop_motion: 0.0,
            seed: 1,
            tick_ms: 10,
        }
    }
}

/// One injected event and where the cursor was when it happened.
#[derive(Clone, Debug, PartialEq)]
pub struct Injected {
    pub at: Millis,
    pub event: InputEvent,
    pub source_os: Os,
    pub cursor: Point,
}

/// One simulated computer.
#[derive(Debug)]
pub struct Device {
    pub id: DeviceId,
    pub os: Os,
    pub core: Core,
    pub displays: Vec<DisplayInfo>,
    pub cursor: Point,
    pub parked: bool,
    pub capture: CaptureMode,
    pub injected: Vec<Injected>,
    /// Keys and buttons injected for the peer and not released yet.
    pub remote_keys: BTreeSet<KeyCode>,
    pub remote_buttons: BTreeSet<MouseButton>,
    /// Keys and buttons the user is physically holding on this device.
    pub physical_keys: BTreeSet<KeyCode>,
    pub physical_buttons: BTreeSet<MouseButton>,
    /// Keys and buttons this device's own OS believes are down from local
    /// sources: physical input that capture let through, plus the core's
    /// local injections (key-ups at a crossing, key-downs on the way back).
    pub os_keys: BTreeSet<KeyCode>,
    pub os_buttons: BTreeSet<MouseButton>,
    pub release_all: usize,
    pub notices: Vec<Notice>,
    pub sent: Vec<(Millis, CoreMessage)>,
    /// Every action, for determinism checks.
    pub log: Vec<(Millis, Action)>,
}

impl Device {
    /// The injected events with their source OS, without timing.
    pub fn injected_events(&self) -> Vec<(InputEvent, Os)> {
        self.injected
            .iter()
            .map(|i| (i.event, i.source_os))
            .collect()
    }

    /// Messages of one kind this device sent, e.g. `|m| matches!(m, Enter {..})`.
    pub fn sent_matching(&self, f: impl Fn(&CoreMessage) -> bool) -> Vec<CoreMessage> {
        self.sent
            .iter()
            .filter(|(_, m)| f(m))
            .map(|(_, m)| m.clone())
            .collect()
    }

    fn on_screen(&self, p: Point) -> bool {
        self.displays.iter().any(|d| d.frame.contains(p))
    }
}

#[derive(Clone, Debug)]
struct Packet {
    deliver_at: Millis,
    order: u64,
    to: usize,
    msg: CoreMessage,
    /// The session the message belongs to (see `message_session`).
    session: Option<SessionKey>,
}

/// A link between the two devices: reliable messages arrive in order, motion
/// datagrams can be reordered or lost.
#[derive(Debug)]
pub struct Network {
    pub latency: (Millis, Millis),
    pub drop_motion: f64,
    /// Messages to `blackhole[i]` are silently lost (a peer that never
    /// answers, or a one-way outage). Harsher than a real connection, which
    /// never loses a reliable message without closing.
    pub blackhole: [bool; 2],
    /// Messages to `stall[i]` wait until [`Sim::heal`], as on a QUIC
    /// connection whose Wi-Fi drops out for a while: reliable messages are
    /// retransmitted and arrive late but in order, datagrams are lost.
    pub stall: [bool; 2],
    /// Whether a blackhole has ever swallowed a reliable message.
    pub dropped_reliable: bool,
    pub connected: bool,
    in_flight: Vec<Packet>,
    /// Reliable messages held back by a stall, in the order they were sent.
    stalled: Vec<Packet>,
    next_order: u64,
    last_reliable: [Millis; 2],
    rng: u64,
    pub lost_motion: usize,
}

impl Network {
    fn random(&mut self) -> u64 {
        // xorshift64*: deterministic and good enough for test jitter.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f64 {
        (self.random() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn send(&mut self, now: Millis, to: usize, msg: CoreMessage, session: Option<SessionKey>) {
        if !self.connected {
            return;
        }
        let unreliable = matches!(msg, CoreMessage::Motion { .. });
        if self.blackhole[to] {
            self.dropped_reliable |= !unreliable;
            return;
        }
        if self.stall[to] {
            if unreliable {
                self.lost_motion += 1;
            } else {
                self.stalled.push(Packet {
                    deliver_at: now,
                    order: self.next_order,
                    to,
                    msg,
                    session,
                });
                self.next_order += 1;
            }
            return;
        }
        let (lo, hi) = self.latency;
        let latency = lo + self.random() % (hi.saturating_sub(lo) + 1);
        if unreliable && self.unit() < self.drop_motion {
            self.lost_motion += 1;
            return;
        }
        let mut deliver_at = now + latency;
        if !unreliable {
            deliver_at = deliver_at.max(self.last_reliable[to]);
            self.last_reliable[to] = deliver_at;
        }
        self.in_flight.push(Packet {
            deliver_at,
            order: self.next_order,
            to,
            msg,
            session,
        });
        self.next_order += 1;
    }

    fn next_due(&self) -> Option<Millis> {
        self.in_flight.iter().map(|p| p.deliver_at).min()
    }

    fn pop_due(&mut self, now: Millis) -> Option<Packet> {
        let index = self
            .in_flight
            .iter()
            .enumerate()
            .filter(|(_, p)| p.deliver_at <= now)
            .min_by_key(|(_, p)| (p.deliver_at, p.order))
            .map(|(i, _)| i)?;
        Some(self.in_flight.remove(index))
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

/// The whole world.
#[derive(Debug)]
pub struct Sim {
    pub now: Millis,
    pub dev: [Device; 2],
    pub net: Network,
    pub config: SimConfig,
    next_tick: Millis,
    /// When each device's current session began.
    session_start: [Millis; 2],
    /// When a message of its current session last reached each device.
    last_heard: [Millis; 2],
}

/// A session as the spec identifies it: the controller's id and the `seq` it
/// picked.
type SessionKey = (DeviceId, u64);

/// The session a device is in. The peer's id would not do: "controlled by the
/// PC in its session 2" and "controlling the PC in my own session 2" are
/// different sessions.
fn session_key(me: &DeviceId, state: &State) -> Option<SessionKey> {
    match state {
        State::Idle => None,
        State::Entering { seq, .. } | State::Controlling { seq, .. } => Some((me.clone(), *seq)),
        State::Controlled { by, seq } => Some((by.clone(), *seq)),
    }
}

/// The session `msg` from `from` to `to` really belongs to, judged by the
/// simulation rather than by the cores. Every message travels one way only
/// except `Heartbeat`, for which `sender_controls` tells whether its sender is
/// the session's controller.
fn message_session(
    msg: &CoreMessage,
    from: &DeviceId,
    to: &DeviceId,
    sender_controls: bool,
) -> Option<SessionKey> {
    let seq = msg.seq()?;
    let controller = match msg {
        CoreMessage::Enter { .. }
        | CoreMessage::Input { .. }
        | CoreMessage::Motion { .. }
        | CoreMessage::Release { .. } => from,
        CoreMessage::EnterAck { .. }
        | CoreMessage::EnterReject { .. }
        | CoreMessage::Leave { .. }
        | CoreMessage::TakeBack { .. } => to,
        CoreMessage::Heartbeat { .. } if sender_controls => from,
        CoreMessage::Heartbeat { .. } => to,
        CoreMessage::ClipboardText { .. } => return None,
    };
    Some((controller.clone(), seq))
}

impl Sim {
    /// The default desk, connected at time 0.
    pub fn mac_and_pc() -> Sim {
        Sim::new(SimConfig::default())
    }

    /// The default desk with a different core config on both devices.
    pub fn with_core(core: CoreConfig) -> Sim {
        Sim::new(SimConfig {
            core,
            ..SimConfig::default()
        })
    }

    pub fn new(config: SimConfig) -> Sim {
        let make = |me: &str, os: Os, mine: &[DisplayInfo], peer: &str, side: Edge| {
            // The peer's displays are unknown until it connects, as in the
            // real agent.
            let layout = Layout::two_devices(
                DeviceId::new(me),
                mine.to_vec(),
                DeviceId::new(peer),
                side,
                vec![],
            )
            .expect("valid test layout");
            let core = Core::new(DeviceId::new(me), os, config.core.clone(), layout);
            let start = mine
                .iter()
                .find(|d| d.primary)
                .unwrap_or(&mine[0])
                .frame
                .center();
            Device {
                id: DeviceId::new(me),
                os,
                core,
                displays: mine.to_vec(),
                cursor: start,
                parked: false,
                capture: CaptureMode::Passive,
                injected: Vec::new(),
                remote_keys: BTreeSet::new(),
                remote_buttons: BTreeSet::new(),
                physical_keys: BTreeSet::new(),
                physical_buttons: BTreeSet::new(),
                os_keys: BTreeSet::new(),
                os_buttons: BTreeSet::new(),
                release_all: 0,
                notices: Vec::new(),
                sent: Vec::new(),
                log: Vec::new(),
            }
        };
        let mac = make(
            config.mac_id,
            Os::MacOs,
            &config.mac_displays,
            config.pc_id,
            Edge::Right,
        );
        let pc = make(
            config.pc_id,
            Os::Windows,
            &config.pc_displays,
            config.mac_id,
            Edge::Left,
        );
        let mut sim = Sim {
            now: 0,
            dev: [mac, pc],
            net: Network {
                latency: config.latency,
                drop_motion: config.drop_motion,
                blackhole: [false, false],
                stall: [false, false],
                dropped_reliable: false,
                connected: true,
                in_flight: Vec::new(),
                stalled: Vec::new(),
                next_order: 0,
                last_reliable: [0, 0],
                rng: config.seed.max(1),
                lost_motion: 0,
            },
            next_tick: config.tick_ms,
            config,
            session_start: [0, 0],
            last_heard: [0, 0],
        };
        sim.connect_both();
        sim
    }

    fn connect_both(&mut self) {
        for d in [MAC, PC] {
            let peer = &self.dev[1 - d];
            let event = Event::PeerConnected {
                peer: peer.id.clone(),
                os: peer.os,
                displays: peer.displays.clone(),
            };
            self.handle(d, event);
        }
    }

    pub fn id(&self, d: usize) -> DeviceId {
        self.dev[d].id.clone()
    }

    pub fn state(&self, d: usize) -> State {
        self.dev[d].core.state()
    }

    pub fn cursor(&self, d: usize) -> Point {
        self.dev[d].cursor
    }

    // ----- physical input -----------------------------------------------

    /// Physical pointer motion on device `d`. With passive capture the OS
    /// moves the cursor (clamped to its displays) before the core sees the
    /// event, which carries the position from before the move.
    pub fn motion(&mut self, d: usize, dx: f64, dy: f64) {
        let dev = &mut self.dev[d];
        let cursor = dev.cursor;
        if dev.capture == CaptureMode::Passive {
            dev.cursor = os_move(&dev.displays, cursor, dx, dy);
        }
        self.physical(d, InputEvent::PointerMotion { dx, dy }, cursor);
    }

    pub fn key(&mut self, d: usize, code: KeyCode, pressed: bool) {
        let cursor = self.dev[d].cursor;
        self.physical(d, key(code, pressed), cursor);
    }

    pub fn tap(&mut self, d: usize, code: KeyCode) {
        self.key(d, code, true);
        self.key(d, code, false);
    }

    /// The default emergency chord on device `d`: Left Control, Left Alt and
    /// Left Shift go down, then Escape; then all four come up again.
    pub fn chord(&mut self, d: usize) {
        let keys = [
            KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::LEFT_SHIFT,
            KeyCode::ESCAPE,
        ];
        for code in keys {
            self.key(d, code, true);
        }
        for code in keys.into_iter().rev() {
            self.key(d, code, false);
        }
    }

    pub fn button(&mut self, d: usize, button: MouseButton, pressed: bool) {
        let cursor = self.dev[d].cursor;
        self.physical(d, click(button, pressed), cursor);
    }

    pub fn scroll(&mut self, d: usize, dy: f64) {
        let cursor = self.dev[d].cursor;
        let event = InputEvent::Scroll {
            dx: 0.0,
            dy,
            unit: ScrollUnit::Line,
        };
        self.physical(d, event, cursor);
    }

    /// One physical event on device `d`. The sim records what the user is
    /// holding and, if capture lets the event through, what the local OS
    /// sees; then the core gets it.
    fn physical(&mut self, d: usize, event: InputEvent, cursor: Point) {
        let dev = &mut self.dev[d];
        let passes = dev.capture == CaptureMode::Passive;
        match event {
            InputEvent::Key { code, pressed, .. } => {
                set_held(&mut dev.physical_keys, code, pressed);
                if passes {
                    set_held(&mut dev.os_keys, code, pressed);
                }
            }
            InputEvent::Button { button, pressed } => {
                set_held(&mut dev.physical_buttons, button, pressed);
                if passes {
                    set_held(&mut dev.os_buttons, button, pressed);
                }
            }
            InputEvent::PointerMotion { .. } | InputEvent::Scroll { .. } => {}
        }
        let chord = self
            .config
            .core
            .emergency_chord
            .is_completed_by(&event, &dev.physical_keys);
        let before = dev.core.state();
        let actions = self.handle(
            d,
            Event::LocalInput {
                event,
                cursor,
                physical: true,
            },
        );
        // Rule 9: the chord ends a crossing or session at once, tells the
        // peer, and its last key never reaches the peer.
        if let (true, State::Entering { seq, .. } | State::Controlling { seq, .. }) =
            (chord, &before)
        {
            let at = self.now;
            assert_eq!(self.state(d), State::Idle, "chord at t={at}");
            assert!(
                actions.iter().any(|a| matches!(a,
                    Action::Send { msg: CoreMessage::Release { seq: s }, .. } if s == seq)),
                "no Release for the chord at t={at}: {actions:?}"
            );
            assert!(
                !actions.iter().any(|a| matches!(a, Action::Send {
                    msg: CoreMessage::Input { events, .. }, ..
                } if events.contains(&event))),
                "the chord's {event:?} was forwarded at t={at}: {actions:?}"
            );
        }
    }

    // ----- time and network ---------------------------------------------

    /// Lets `ms` milliseconds pass: messages arrive when due and both cores
    /// get a `Tick` every `tick_ms`. Deliveries due at a tick go first.
    pub fn advance(&mut self, ms: Millis) {
        let target = self.now + ms;
        loop {
            if let Some(t) = self.net.next_due()
                && t <= target
                && t <= self.next_tick
            {
                self.now = self.now.max(t);
                if let Some(packet) = self.net.pop_due(self.now) {
                    self.deliver_as(packet.to, packet.msg, packet.session);
                }
                continue;
            }
            if self.next_tick <= target {
                self.now = self.now.max(self.next_tick);
                self.next_tick += self.config.tick_ms;
                self.handle(MAC, Event::Tick);
                self.handle(PC, Event::Tick);
                continue;
            }
            break;
        }
        self.now = target;
    }

    /// Long enough for everything in flight (and its answers) to arrive.
    pub fn settle(&mut self) {
        let worst = self.config.latency.1;
        self.advance(4 * worst + 2 * self.config.tick_ms);
    }

    /// Hands `msg` to device `to` right now, as if from its peer. A made-up
    /// `Heartbeat` is taken to come from the peer in its role in the
    /// receiver's current session, so it belongs to that session exactly when
    /// its `seq` matches.
    pub fn deliver(&mut self, to: usize, msg: CoreMessage) -> Vec<Action> {
        let (me, peer) = (self.id(to), self.id(1 - to));
        let sender_controls = match session_key(&me, &self.state(to)) {
            Some((controller, _)) => controller == peer,
            None => true,
        };
        let session = message_session(&msg, &peer, &me, sender_controls);
        self.deliver_as(to, msg, session)
    }

    /// Hands `msg`, which belongs to `session`, to device `to` right now.
    ///
    /// "Messages from stale sessions change nothing" is checked here on
    /// every delivery: a message of any other session than the receiver's
    /// current one, `Enter` aside, must act exactly like a plain `Tick` at
    /// the same moment, on the actions and on the whole core state.
    fn deliver_as(
        &mut self,
        to: usize,
        msg: CoreMessage,
        session: Option<SessionKey>,
    ) -> Vec<Action> {
        let me = self.id(to);
        let current = session.is_some() && session == session_key(&me, &self.state(to));
        let stale = session.is_some() && !current && !matches!(msg, CoreMessage::Enter { .. });
        let twin = stale.then(|| {
            let mut twin = self.dev[to].core.clone();
            let actions = twin.handle(self.now, Event::Tick);
            (twin, actions)
        });
        let from = self.id(1 - to);
        let actions = self.handle(
            to,
            Event::PeerMessage {
                from,
                msg: msg.clone(),
            },
        );
        if let Some((twin, expected)) = twin {
            let at = self.now;
            assert_eq!(actions, expected, "stale {msg:?} acted on {me} at t={at}");
            assert_eq!(
                format!("{:?}", self.dev[to].core),
                format!("{twin:?}"),
                "stale {msg:?} changed {me} at t={at}"
            );
        }
        if current {
            self.last_heard[to] = self.now;
        }
        actions
    }

    /// The connection drops: both sides hear about it, nothing in flight
    /// arrives.
    pub fn disconnect(&mut self) {
        self.net.connected = false;
        self.net.in_flight.clear();
        self.net.stalled.clear();
        for d in [MAC, PC] {
            let peer = self.id(1 - d);
            self.handle(d, Event::PeerDisconnected { peer });
        }
    }

    /// Ends every outage. Messages a blackhole swallowed stay lost; the ones
    /// a stall held back go out now, in their original order.
    pub fn heal(&mut self) {
        self.net.blackhole = [false, false];
        self.net.stall = [false, false];
        let now = self.now;
        for packet in std::mem::take(&mut self.net.stalled) {
            self.net.send(now, packet.to, packet.msg, packet.session);
        }
    }

    /// Whether messages to some device are being lost or held back.
    pub fn outage(&self) -> bool {
        self.net.blackhole.contains(&true) || self.net.stall.contains(&true)
    }

    pub fn reconnect(&mut self) {
        if self.net.connected {
            return;
        }
        self.net.connected = true;
        self.connect_both();
    }

    /// Device `d`'s displays change, say a monitor is plugged in or out. The
    /// OS moves a cursor left off screen onto the first display, and the
    /// agent hands the core a layout rebuilt from the config, without the
    /// peer's displays. The peer is not told, so it keeps an outdated view.
    pub fn set_displays(&mut self, d: usize, displays: Vec<DisplayInfo>) {
        let side = if d == MAC { Edge::Right } else { Edge::Left };
        let layout =
            Layout::two_devices(self.id(d), displays.clone(), self.id(1 - d), side, vec![])
                .expect("valid test layout");
        let dev = &mut self.dev[d];
        dev.displays = displays;
        if !dev.on_screen(dev.cursor) {
            dev.cursor = dev.displays[0].frame.clamp(dev.cursor, 1.0);
        }
        self.handle(d, Event::LayoutChanged(layout));
    }

    // ----- the core boundary ----------------------------------------------

    /// Feeds one event to device `d`'s core, carries out the actions and
    /// checks the invariants.
    pub fn handle(&mut self, d: usize, event: Event) -> Vec<Action> {
        let before = self.dev[d].core.state();
        let actions = self.dev[d].core.handle(self.now, event);
        let after = self.dev[d].core.state();
        self.apply(d, &actions, &after);
        let me = &self.dev[d].id;
        let session = session_key(me, &after);
        if session.is_some() && session != session_key(me, &before) {
            self.session_start[d] = self.now;
        }
        self.check(d, &before, &after, &actions);
        self.check_remote_keys();
        actions
    }

    /// Carries out `actions` of device `d`, whose core is now in `after`.
    fn apply(&mut self, d: usize, actions: &[Action], after: &State) {
        let now = self.now;
        // A core sends heartbeats last, from the state it ends up in.
        let controls = matches!(after, State::Entering { .. } | State::Controlling { .. });
        let controlled = matches!(after, State::Controlled { .. });
        for action in actions {
            self.dev[d].log.push((now, action.clone()));
            match action {
                Action::Send { to, msg } => {
                    assert_eq!(*to, self.dev[1 - d].id, "only the peer exists");
                    let dev = &self.dev[d];
                    match msg {
                        // Crossing rule: no mouse button physically held.
                        CoreMessage::Enter { .. } => assert!(
                            dev.physical_buttons.is_empty(),
                            "{} crossed with {:?} held at t={now}",
                            dev.id,
                            dev.physical_buttons
                        ),
                        // Rule 5: no exit while an injected button is held.
                        CoreMessage::Leave { .. } => assert!(
                            dev.remote_buttons.is_empty(),
                            "{} let the cursor leave with {:?} held at t={now}",
                            dev.id,
                            dev.remote_buttons
                        ),
                        _ => {}
                    }
                    self.dev[d].sent.push((now, msg.clone()));
                    let session = message_session(msg, &self.dev[d].id, to, controls);
                    self.net.send(now, 1 - d, msg.clone(), session);
                }
                Action::SetCaptureMode(mode) => self.dev[d].capture = *mode,
                Action::ParkCursor => self.dev[d].parked = true,
                Action::RestoreCursor { at } => {
                    let dev = &mut self.dev[d];
                    dev.cursor = *at;
                    dev.parked = false;
                }
                Action::MoveCursorTo { to } => {
                    assert!(controlled, "MoveCursorTo while {after:?} at t={now}");
                    self.dev[d].cursor = *to;
                }
                Action::Inject { event, source_os } => {
                    let dev = &mut self.dev[d];
                    if *source_os == dev.os {
                        // The core's own keys for local apps: never buttons.
                        let InputEvent::Key { code, pressed, .. } = *event else {
                            panic!("local injection of {event:?} at t={now}");
                        };
                        set_held(&mut dev.os_keys, code, pressed);
                    } else {
                        assert!(controlled, "injected {event:?} while {after:?} at t={now}");
                        match *event {
                            InputEvent::Key { code, pressed, .. } => {
                                set_held(&mut dev.remote_keys, code, pressed);
                            }
                            InputEvent::Button { button, pressed } => {
                                set_held(&mut dev.remote_buttons, button, pressed);
                            }
                            _ => {}
                        }
                    }
                    let cursor = dev.cursor;
                    dev.injected.push(Injected {
                        at: now,
                        event: *event,
                        source_os: *source_os,
                        cursor,
                    });
                }
                Action::ReleaseAllInjected => {
                    let dev = &mut self.dev[d];
                    dev.release_all += 1;
                    dev.remote_keys.clear();
                    dev.remote_buttons.clear();
                }
                Action::Notify(notice) => self.dev[d].notices.push(notice.clone()),
            }
        }
    }

    /// The invariants of docs/ARCHITECTURE.md, plus a few about the
    /// simulated OS that follow from them.
    fn check(&self, d: usize, before: &State, after: &State, actions: &[Action]) {
        let dev = &self.dev[d];
        let ctx = || {
            format!(
                "device {} at t={} after {:?}: {actions:#?}",
                dev.id, self.now, before
            )
        };

        // Capture is Suppress exactly in Entering and Controlling.
        assert_eq!(dev.capture, after.capture_mode(), "capture mode, {}", ctx());
        // The cursor is parked exactly then too, and otherwise on screen.
        let away = matches!(after, State::Entering { .. } | State::Controlling { .. });
        assert_eq!(dev.parked, away, "parked cursor, {}", ctx());
        if !dev.parked {
            assert!(
                dev.on_screen(dev.cursor),
                "cursor {:?} off screen, {}",
                dev.cursor,
                ctx()
            );
        }

        // Two devices never control each other at the same time.
        let (mac, pc) = (self.state(MAC), self.state(PC));
        let mutual = matches!(&mac, State::Controlling { target, .. } if *target == self.dev[PC].id)
            && matches!(&pc, State::Controlling { target, .. } if *target == self.dev[MAC].id);
        assert!(!mutual, "mutual control: {mac:?} / {pc:?}, {}", ctx());

        // Every exit from Controlled emits ReleaseAllInjected ...
        if matches!(before, State::Controlled { .. }) && before != after {
            assert!(
                actions.contains(&Action::ReleaseAllInjected),
                "left Controlled without releasing, {}",
                ctx()
            );
        }
        // Local apps never see a key or button down that the user is not
        // holding: no stuck keys on this device.
        assert!(
            dev.os_keys.is_subset(&dev.physical_keys)
                && dev.os_buttons.is_subset(&dev.physical_buttons),
            "local OS holds keys {:?} / buttons {:?}, the user {:?} / {:?}, {}",
            dev.os_keys,
            dev.os_buttons,
            dev.physical_keys,
            dev.physical_buttons,
            ctx()
        );

        // ... after the last key or button it injected for the peer.
        if !matches!(after, State::Controlled { .. }) {
            assert!(
                dev.remote_keys.is_empty() && dev.remote_buttons.is_empty(),
                "keys {:?} / buttons {:?} left down, {}",
                dev.remote_keys,
                dev.remote_buttons,
                ctx()
            );
        }

        // No session state survives peer_timeout_ms without a message of
        // that very session, and no crossing waits longer than
        // enter_timeout_ms.
        if *after != State::Idle {
            let quiet_since = self.session_start[d].max(self.last_heard[d]);
            assert!(
                self.now - quiet_since < self.config.core.peer_timeout_ms,
                "session survived {} ms of silence, {}",
                self.now - quiet_since,
                ctx()
            );
        }
        if matches!(after, State::Entering { .. }) {
            assert!(
                self.now - self.session_start[d] < self.config.core.enter_timeout_ms,
                "still entering after the enter timeout, {}",
                ctx()
            );
        }
    }

    /// No stuck keys on the far side: once everything sent has arrived, the
    /// controlled device holds down exactly the buttons the user holds on
    /// the controller, every modifier the user holds there, and no key the
    /// user is not holding. (Only modifiers travel with `Enter`, so another
    /// key already down at the crossing is not pressed on the far side.)
    /// Skipped after a blackhole swallowed a reliable message, which a real
    /// connection never does without closing.
    fn check_remote_keys(&self) {
        if self.net.dropped_reliable || self.net.in_flight() > 0 || !self.net.stalled.is_empty() {
            return;
        }
        let (c, d) = match (self.state(MAC), self.state(PC)) {
            (State::Controlling { seq: a, .. }, State::Controlled { seq: b, .. }) if a == b => {
                (MAC, PC)
            }
            (State::Controlled { seq: b, .. }, State::Controlling { seq: a, .. }) if a == b => {
                (PC, MAC)
            }
            _ => return,
        };
        let (controller, controlled) = (&self.dev[c], &self.dev[d]);
        let modifiers: BTreeSet<KeyCode> = controller
            .physical_keys
            .iter()
            .copied()
            .filter(|k| k.is_modifier())
            .collect();
        assert!(
            controlled.remote_keys.is_subset(&controller.physical_keys)
                && modifiers.is_subset(&controlled.remote_keys)
                && controlled.remote_buttons == controller.physical_buttons,
            "at t={}, {} holds keys {:?} / buttons {:?} for {}, whose user holds {:?} / {:?}",
            self.now,
            controlled.id,
            controlled.remote_keys,
            controlled.remote_buttons,
            controller.id,
            controller.physical_keys,
            controller.physical_buttons
        );
    }
}

fn set_held<T: Ord>(held: &mut BTreeSet<T>, item: T, pressed: bool) {
    if pressed {
        held.insert(item);
    } else {
        held.remove(&item);
    }
}

/// How an OS moves its cursor: freely across its displays. A motion that
/// would leave every display is pulled back inside the desktop's bounding
/// box, so the cursor slides along the outer edge onto the next display; if
/// that point is on no display either, the cursor stops at the edge of the
/// display it was on.
pub fn os_move(displays: &[DisplayInfo], from: Point, dx: f64, dy: f64) -> Point {
    let to = Point::new(from.x + dx, from.y + dy);
    if displays.iter().any(|d| d.frame.contains(to)) {
        return to;
    }
    let desktop = displays
        .iter()
        .skip(1)
        .fold(displays[0].frame, |acc, d| acc.union(&d.frame));
    let pulled = desktop.clamp(to, 1.0);
    if displays.iter().any(|d| d.frame.contains(pulled)) {
        return pulled;
    }
    let home = displays
        .iter()
        .find(|d| d.frame.contains(from))
        .unwrap_or(&displays[0]);
    home.frame.clamp(to, 1.0)
}
