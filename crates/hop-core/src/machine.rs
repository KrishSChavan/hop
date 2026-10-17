//! The control hand-off state machine (docs/ARCHITECTURE.md, "Control
//! hand-off").
//!
//! [`Core::handle`] first fires any timer that is due at `now`, then applies
//! the event, then sends a heartbeat if one is due; only a layout change is
//! applied before the timers. Every exit from a session state is "fail open":
//! the controller restores its cursor and stops suppressing input, the
//! controlled side releases everything it injected.

use std::collections::{BTreeMap, BTreeSet};

use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, EntryPoint, InputEvent, KeyCode, MouseButton, Os,
    Point, RejectReason,
};

use crate::layout::{self, Exit, Layout, LayoutError, Travel};
use crate::motion::{MarkReceiver, MarkSender};
use crate::{CoreConfig, Crossing, Millis};

/// Whether the platform layer lets this device's physical input reach the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CaptureMode {
    /// Physical input reaches local apps; the core still sees it.
    Passive,
    /// Physical input is swallowed; only the core sees it.
    Suppress,
}

/// Something that happened, fed to [`Core::handle`].
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Time passed. The runtime should send one at least every 50 ms so
    /// timeouts and heartbeats happen on time.
    Tick,
    /// A peer finished its `Hello`. `displays` are in the peer's own
    /// coordinates; they replace the peer's displays in the layout. A second
    /// `PeerConnected` for a peer that is already connected means the old
    /// connection is gone, so any session with it ends first.
    PeerConnected {
        /// The peer's device id.
        peer: DeviceId,
        /// The peer's operating system.
        os: Os,
        /// The peer's displays, as reported in its `Hello`.
        displays: Vec<DisplayInfo>,
    },
    /// The connection to a peer closed.
    PeerDisconnected {
        /// The peer whose connection closed.
        peer: DeviceId,
    },
    /// A message from a peer. Messages from peers that are not connected are
    /// ignored, and so is `ClipboardText`, which the runtime handles itself.
    PeerMessage {
        /// The sender.
        from: DeviceId,
        /// The message.
        msg: CoreMessage,
    },
    /// Input seen by the capture layer.
    LocalInput {
        /// The event, in this device's logical units.
        event: InputEvent,
        /// Where the cursor was when the event happened. For pointer motion
        /// this is the position *before* the motion (an event tap reports
        /// the position after it): the core treats `cursor + (dx, dy)` as
        /// where the motion tries to go. Only read while idle.
        cursor: Point,
        /// False for Hop's own injected events, which the core ignores.
        physical: bool,
    },
    /// This device's layout changed (displays added, removed or moved). A
    /// connected peer that has no displays in the new layout keeps the ones
    /// it reported when it connected.
    LayoutChanged(Layout),
}

/// Something the runtime must do. [`Core::handle`] returns them in the order
/// they must happen.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send `msg` to `to`. `Motion` may go as an unreliable datagram; every
    /// other message must be reliable and ordered.
    Send {
        /// The receiving device.
        to: DeviceId,
        /// The message.
        msg: CoreMessage,
    },
    /// Start or stop swallowing physical input.
    SetCaptureMode(CaptureMode),
    /// Freeze and hide the local cursor while another device is driven.
    ParkCursor,
    /// Show the local cursor again at `at` (local coordinates).
    RestoreCursor {
        /// Where the cursor reappears.
        at: Point,
    },
    /// Move the local cursor to `to` while controlled; drag if injected
    /// buttons are held.
    MoveCursorTo {
        /// The new position, inside a local display.
        to: Point,
    },
    /// Inject `event` locally. Key codes are as `source_os`'s keyboard sent
    /// them; the runtime applies the key mapping.
    Inject {
        /// The event to inject.
        event: InputEvent,
        /// The OS whose keyboard produced the key codes.
        source_os: Os,
    },
    /// Release every key and button the runtime injected for a peer.
    ReleaseAllInjected,
    /// Tell the user interface.
    Notify(Notice),
}

/// News for the user interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// This device now controls `peer`.
    ControlStarted {
        /// The controlled device.
        peer: DeviceId,
    },
    /// `peer` now controls this device.
    ControlledBy {
        /// The controlling device.
        peer: DeviceId,
    },
    /// The session (or crossing attempt) with `peer` is over.
    SessionEnded {
        /// The other device of the session.
        peer: DeviceId,
        /// Why it ended.
        reason: EndReason,
    },
    /// The displays `peer` reported are unusable, so the cursor cannot cross
    /// to it until it reconnects with valid ones.
    PeerDisplaysRejected {
        /// The peer that reported them.
        peer: DeviceId,
        /// What was wrong.
        error: LayoutError,
    },
}

/// Why a session or crossing attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The peer refused the crossing.
    Rejected(RejectReason),
    /// The peer did not acknowledge the crossing within `enter_timeout_ms`.
    EnterTimeout,
    /// Both devices crossed at once and the peer, whose id is lower, kept
    /// the controller role. This device is now controlled by it.
    LostTieBreak,
    /// More input arrived while waiting for the peer than the buffer holds.
    BufferFull,
    /// The cursor left the controlled device through an edge.
    CursorLeft,
    /// Physical input on the controlled device took control back.
    TakenBack,
    /// The controller ended the session.
    Released,
    /// The emergency chord was pressed on this device.
    EmergencyChord,
    /// Nothing arrived from the peer for `peer_timeout_ms`.
    PeerTimeout,
    /// The connection to the peer closed or was replaced.
    Disconnected,
    /// The peer started a new session with this device, replacing this one.
    Superseded,
    /// The cursor was sent on toward a device this one cannot reach.
    UnreachableTarget,
}

/// A summary of what the core is doing, for user interfaces and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Local control.
    Idle,
    /// Crossed into `target`, waiting for its `EnterAck`.
    Entering {
        /// The device being entered.
        target: DeviceId,
        /// The session's sequence number.
        seq: u64,
    },
    /// This device's input drives `target`.
    Controlling {
        /// The controlled device.
        target: DeviceId,
        /// The session's sequence number.
        seq: u64,
    },
    /// `by` drives this device.
    Controlled {
        /// The controlling device.
        by: DeviceId,
        /// The session's sequence number.
        seq: u64,
    },
}

impl State {
    /// The capture mode that belongs to this state: `Suppress` exactly while
    /// entering or controlling.
    pub fn capture_mode(&self) -> CaptureMode {
        match self {
            State::Entering { .. } | State::Controlling { .. } => CaptureMode::Suppress,
            State::Idle | State::Controlled { .. } => CaptureMode::Passive,
        }
    }

    /// The other device of the current session, if any.
    pub fn peer(&self) -> Option<&DeviceId> {
        match self {
            State::Idle => None,
            State::Entering { target, .. } | State::Controlling { target, .. } => Some(target),
            State::Controlled { by, .. } => Some(by),
        }
    }
}

/// The hand-off state machine of one device.
///
/// Sans-IO: feed it [`Event`]s with the current time and carry out the
/// [`Action`]s it returns. It is deterministic: the same events at the same
/// times always give the same actions.
///
/// It follows docs/ARCHITECTURE.md ("Control hand-off"). Where the spec
/// leaves a case open, it decides like this:
/// - A device's `seq`s are odd toward a peer with a higher id and even
///   toward a lower one. A `seq` then names exactly one session of the pair,
///   so a `Heartbeat`, the one message that travels both ways, can never be
///   taken for one of the session with the roles reversed.
/// - An `Enter` from the device this one controls, or is controlled by,
///   replaces that session ([`EndReason::Superseded`]): the sender only
///   sends `Enter` while idle, so the old session is over on its side.
/// - Giving up on a timeout, the emergency chord, a full input buffer or a
///   lost tie-break still tells the peer (`Release` from the controller,
///   `TakeBack` from the controlled side), so a peer that missed the end
///   does not wait for its own timeout. A peer that never hears it times out
///   anyway.
/// - More input while entering than the buffer holds gives the crossing up
///   ([`EndReason::BufferFull`]). Nothing has been forwarded yet, while
///   dropping only some events could lose a key-up and leave a key stuck
///   down on the peer.
/// - When a crossing or session that left through an edge ends without the
///   cursor coming back, that edge stays disarmed until the cursor stops
///   pushing against it, so a busy or silent peer cannot cause a storm of
///   crossings.
/// - A motion that ends off screen is followed across this device's
///   displays, so it leaves through the edge of the last display it passes
///   (a diagonal past a laptop's corner onto the monitor above it), and a
///   cursor this core drives slides along the desktop's outer edge onto the
///   next display, as an OS moves its own cursor. The spec's "the display
///   that contains `pos`" is the same thing for one display, but with two it
///   left the driven cursor stuck where displays meet.
#[derive(Clone, Debug)]
pub struct Core {
    me: DeviceId,
    os: Os,
    config: CoreConfig,
    layout: Layout,
    /// Connected peers and the displays each reported (empty if refused).
    peers: BTreeMap<DeviceId, Vec<DisplayInfo>>,
    /// Keys physically held on this device, tracked in every state.
    held_keys: BTreeSet<KeyCode>,
    /// Mouse buttons physically held on this device, tracked in every state.
    held_buttons: BTreeSet<MouseButton>,
    /// The lowest `seq` the next `Enter` may use (see [`Core::new_seq`]).
    next_seq: u64,
    phase: Phase,
}

#[derive(Clone, Debug)]
enum Phase {
    Idle { push: Option<Push> },
    Entering(Entering),
    Controlling(Controlling),
    Controlled(Controlled),
}

/// Edge tracking while idle.
#[derive(Clone, Debug, PartialEq)]
enum Push {
    /// Pushing against an edge that leads to `to` since `since` (for
    /// `edge_delay_ms`).
    Pushing {
        display: u32,
        edge: Edge,
        to: DeviceId,
        since: Millis,
    },
    /// A session that left through this edge just ended without the cursor
    /// coming back. Crossing here again needs the cursor to stop pushing
    /// against the edge first, so a busy or silent peer, or the emergency
    /// chord, cannot turn a continued push into a storm of new crossings.
    Disarmed { display: u32, edge: Edge },
}

/// Where the cursor left this device, so it can be put back there.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Departure {
    point: Point,
    display: u32,
    edge: Edge,
}

/// The session peer, `seq`, and liveness bookkeeping.
#[derive(Clone, Debug)]
struct Link {
    peer: DeviceId,
    seq: u64,
    /// When the last message of this session arrived (or the session began).
    last_heard: Millis,
    /// When the last reliable message went to the peer. Unreliable `Motion`
    /// does not count: a lost datagram proves nothing to the peer, so it must
    /// not suppress the heartbeat.
    last_sent: Millis,
}

#[derive(Clone, Debug)]
struct Entering {
    link: Link,
    started: Millis,
    departure: Departure,
    buffer: Vec<InputEvent>,
}

#[derive(Clone, Debug)]
struct Controlling {
    link: Link,
    departure: Departure,
    marks: MarkSender,
}

#[derive(Clone, Debug)]
struct Controlled {
    link: Link,
    source_os: Os,
    /// The cursor position this core drives; authoritative (ADR-6).
    pos: Point,
    marks: MarkReceiver,
    /// Buttons injected for the controller and still down.
    buttons: BTreeSet<MouseButton>,
    /// Physical pointer travel on this device since the session began.
    local_travel: f64,
}

impl Phase {
    fn idle() -> Self {
        Phase::Idle { push: None }
    }

    fn link(&self) -> Option<&Link> {
        match self {
            Phase::Idle { .. } => None,
            Phase::Entering(e) => Some(&e.link),
            Phase::Controlling(c) => Some(&c.link),
            Phase::Controlled(c) => Some(&c.link),
        }
    }

    fn link_mut(&mut self) -> Option<&mut Link> {
        match self {
            Phase::Idle { .. } => None,
            Phase::Entering(e) => Some(&mut e.link),
            Phase::Controlling(c) => Some(&mut c.link),
            Phase::Controlled(c) => Some(&mut c.link),
        }
    }
}

/// The fields of an `Enter` received from a peer.
struct EnterRequest {
    seq: u64,
    entry: EntryPoint,
    held_keys: Vec<KeyCode>,
    source_os: Os,
}

/// A crossing that passed every rule except possibly the edge delay.
struct Candidate {
    exit: Exit,
    crossing: Crossing,
}

impl Core {
    /// A core for device `me` running `os`. Unusable config values are
    /// replaced by their defaults (see [`CoreConfig`]). `layout` should
    /// contain `me`; peers may have empty display lists until they connect.
    pub fn new(me: DeviceId, os: Os, config: CoreConfig, layout: Layout) -> Self {
        Self {
            me,
            os,
            config: config.sanitized(),
            layout,
            peers: BTreeMap::new(),
            held_keys: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            next_seq: 1,
            phase: Phase::idle(),
        }
    }

    /// Applies `event` at time `now` and returns what the runtime must do.
    pub fn handle(&mut self, now: Millis, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            // New displays are in place from this moment, so a timer firing
            // now must already see them: it may put the cursor back where a
            // display was just unplugged.
            Event::LayoutChanged(layout) => {
                self.on_layout_changed(layout);
                self.check_timers(now, &mut out);
            }
            event => {
                self.check_timers(now, &mut out);
                self.apply(now, event, &mut out);
            }
        }
        self.send_heartbeat_if_due(now, &mut out);
        out
    }

    fn apply(&mut self, now: Millis, event: Event, out: &mut Vec<Action>) {
        match event {
            Event::Tick => {}
            Event::PeerConnected { peer, displays, .. } => {
                self.on_peer_connected(now, peer, displays, out);
            }
            Event::PeerDisconnected { peer } => self.on_peer_disconnected(now, &peer, out),
            Event::PeerMessage { from, msg } => self.on_message(now, from, msg, out),
            Event::LocalInput {
                event,
                cursor,
                physical,
            } => {
                if physical {
                    self.on_local_input(now, event, cursor, out);
                }
            }
            Event::LayoutChanged(layout) => self.on_layout_changed(layout),
        }
    }

    /// What the core is doing right now.
    pub fn state(&self) -> State {
        match &self.phase {
            Phase::Idle { .. } => State::Idle,
            Phase::Entering(e) => State::Entering {
                target: e.link.peer.clone(),
                seq: e.link.seq,
            },
            Phase::Controlling(c) => State::Controlling {
                target: c.link.peer.clone(),
                seq: c.link.seq,
            },
            Phase::Controlled(c) => State::Controlled {
                by: c.link.peer.clone(),
                seq: c.link.seq,
            },
        }
    }

    /// The layout in use, including the displays connected peers reported.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    // ----- timers -------------------------------------------------------

    fn check_timers(&mut self, now: Millis, out: &mut Vec<Action>) {
        let Some(link) = self.phase.link() else {
            return;
        };
        let silent = now.saturating_sub(link.last_heard) >= self.config.peer_timeout_ms;
        let enter_expired = matches!(&self.phase, Phase::Entering(e)
            if now.saturating_sub(e.started) >= self.config.enter_timeout_ms);
        if !(silent || enter_expired) {
            return;
        }
        let reason = if enter_expired {
            EndReason::EnterTimeout
        } else {
            EndReason::PeerTimeout
        };
        // The peer may be alive but unreachable in one direction; telling it
        // the session is over (best effort) stops it waiting for its own
        // timeout.
        match std::mem::replace(&mut self.phase, Phase::idle()) {
            Phase::Idle { .. } => {}
            Phase::Entering(Entering {
                link, departure, ..
            })
            | Phase::Controlling(Controlling {
                link, departure, ..
            }) => self.finish_as_controller(now, link, departure, reason, true, out),
            Phase::Controlled(c) => {
                let msg = CoreMessage::TakeBack { seq: c.link.seq };
                self.finish_as_controlled(c.link, reason, Some(msg), out);
            }
        }
    }

    fn send_heartbeat_if_due(&mut self, now: Millis, out: &mut Vec<Action>) {
        let interval = self.config.heartbeat_interval_ms;
        if let Some(link) = self.phase.link_mut()
            && now.saturating_sub(link.last_sent) >= interval
        {
            let seq = link.seq;
            send_on(link, now, CoreMessage::Heartbeat { seq }, out);
        }
    }

    // ----- connections and layout ---------------------------------------

    fn on_peer_connected(
        &mut self,
        now: Millis,
        peer: DeviceId,
        displays: Vec<DisplayInfo>,
        out: &mut Vec<Action>,
    ) {
        if peer == self.me {
            return;
        }
        if self.peers.contains_key(&peer) {
            self.end_session_with(now, &peer, EndReason::Disconnected, out);
        }
        let accepted = match self.layout.set_displays(&peer, displays.clone()) {
            Ok(()) => displays,
            Err(error) => {
                // The peer stays connected but cannot be crossed into. An
                // empty list always validates, so this only fails for a peer
                // that is not in the layout at all, which is fine.
                let _ = self.layout.set_displays(&peer, Vec::new());
                out.push(Action::Notify(Notice::PeerDisplaysRejected {
                    peer: peer.clone(),
                    error,
                }));
                Vec::new()
            }
        };
        self.peers.insert(peer, accepted);
        self.reset_push();
    }

    fn on_peer_disconnected(&mut self, now: Millis, peer: &DeviceId, out: &mut Vec<Action>) {
        self.peers.remove(peer);
        self.end_session_with(now, peer, EndReason::Disconnected, out);
        self.reset_push();
    }

    fn on_layout_changed(&mut self, layout: Layout) {
        self.layout = layout;
        for (id, displays) in &self.peers {
            let missing = self
                .layout
                .device(id)
                .is_some_and(|d| d.displays().is_empty());
            if missing && !displays.is_empty() {
                // These displays were validated when the peer connected.
                let _ = self.layout.set_displays(id, displays.clone());
            }
        }
        // Display ids and edges may mean something else now.
        self.reset_push();
    }

    /// Ends the current session if it is with `peer`; no message is sent
    /// because the connection is gone.
    fn end_session_with(
        &mut self,
        now: Millis,
        peer: &DeviceId,
        reason: EndReason,
        out: &mut Vec<Action>,
    ) {
        if !self.phase.link().is_some_and(|l| l.peer == *peer) {
            return;
        }
        match std::mem::replace(&mut self.phase, Phase::idle()) {
            Phase::Idle { .. } => {}
            Phase::Entering(Entering {
                link, departure, ..
            })
            | Phase::Controlling(Controlling {
                link, departure, ..
            }) => self.finish_as_controller(now, link, departure, reason, false, out),
            Phase::Controlled(c) => self.finish_as_controlled(c.link, reason, None, out),
        }
    }

    fn reset_push(&mut self) {
        if let Phase::Idle { push } = &mut self.phase {
            *push = None;
        }
    }

    // ----- messages -----------------------------------------------------

    fn on_message(&mut self, now: Millis, from: DeviceId, msg: CoreMessage, out: &mut Vec<Action>) {
        if from == self.me || !self.peers.contains_key(&from) {
            return;
        }
        match msg {
            CoreMessage::Enter {
                seq,
                entry,
                held_keys,
                source_os,
            } => {
                let request = EnterRequest {
                    seq,
                    entry,
                    held_keys,
                    source_os,
                };
                self.on_enter(now, from, &request, out);
            }
            CoreMessage::ClipboardText { .. } => {}
            msg => self.on_session_message(now, &from, msg, out),
        }
    }

    // Rule 3 of the spec; the cases it leaves open are decided as below.
    fn on_enter(
        &mut self,
        now: Millis,
        from: DeviceId,
        request: &EnterRequest,
        out: &mut Vec<Action>,
    ) {
        let seq = request.seq;
        let busy = |out: &mut Vec<Action>, to: DeviceId| {
            out.push(Action::Send {
                to,
                msg: CoreMessage::EnterReject {
                    seq,
                    reason: RejectReason::Busy,
                },
            });
        };
        match &mut self.phase {
            Phase::Idle { .. } => self.accept(now, from, request, out),
            // Both crossed at once: the lower id keeps the controller role.
            Phase::Entering(e) if e.link.peer == from => {
                if self.me < from {
                    busy(out, from);
                } else if let Phase::Entering(e) = std::mem::replace(&mut self.phase, Phase::idle())
                {
                    // The winner normally rejects our Enter, and then this
                    // Release is stale there. But if the winner gave up its
                    // own crossing before our Enter arrived, it accepted it,
                    // and the Release ends that session nobody drives.
                    self.finish_as_controller(
                        now,
                        e.link,
                        e.departure,
                        EndReason::LostTieBreak,
                        true,
                        out,
                    );
                    self.accept(now, from, request, out);
                }
            }
            // `from` only sends Enter while idle, so it already ended the
            // session we think we have with it. Fail open on our side, then
            // treat the Enter as if we were idle.
            Phase::Controlling(c) if c.link.peer == from => {
                if let Phase::Controlling(c) = std::mem::replace(&mut self.phase, Phase::idle()) {
                    self.finish_as_controller(
                        now,
                        c.link,
                        c.departure,
                        EndReason::Superseded,
                        false,
                        out,
                    );
                    self.accept(now, from, request, out);
                }
            }
            Phase::Controlled(c) if c.link.peer == from => {
                if c.link.seq == seq {
                    // A duplicate of the Enter that started this session.
                    c.link.last_heard = now;
                } else if let Phase::Controlled(c) =
                    std::mem::replace(&mut self.phase, Phase::idle())
                {
                    self.finish_as_controlled(c.link, EndReason::Superseded, None, out);
                    self.accept(now, from, request, out);
                }
            }
            // Busy with another device (entering, controlling or controlled).
            _ => busy(out, from),
        }
    }

    /// Becomes controlled by `from` (rule 3, "while Idle").
    fn accept(
        &mut self,
        now: Millis,
        from: DeviceId,
        request: &EnterRequest,
        out: &mut Vec<Action>,
    ) {
        let EnterRequest {
            seq,
            entry,
            ref held_keys,
            source_os,
        } = *request;
        let displays = self
            .layout
            .device(&self.me)
            .map_or(&[][..], |d| d.displays());
        let Some(pos) = layout::place_point(displays, Some(entry.display_id), entry.position)
        else {
            // No local display to put a cursor on: refuse rather than inject
            // blind.
            out.push(Action::Send {
                to: from,
                msg: CoreMessage::EnterReject {
                    seq,
                    reason: RejectReason::Unavailable,
                },
            });
            return;
        };
        out.push(Action::MoveCursorTo { to: pos });
        // Only modifiers are pressed: they are all `held_keys` should carry,
        // and pressing arbitrary keys for a peer would be unsafe.
        let modifiers: BTreeSet<KeyCode> = held_keys
            .iter()
            .copied()
            .filter(|k| k.is_modifier())
            .collect();
        for code in modifiers {
            out.push(Action::Inject {
                event: InputEvent::Key {
                    code,
                    pressed: true,
                    repeat: false,
                },
                source_os,
            });
        }
        let mut link = Link {
            peer: from.clone(),
            seq,
            last_heard: now,
            last_sent: now,
        };
        send_on(&mut link, now, CoreMessage::EnterAck { seq }, out);
        out.push(Action::Notify(Notice::ControlledBy { peer: from }));
        self.phase = Phase::Controlled(Controlled {
            link,
            source_os,
            pos,
            marks: MarkReceiver::default(),
            buttons: BTreeSet::new(),
            local_travel: 0.0,
        });
    }

    /// Every message except `Enter` and `ClipboardText`. Only messages of the
    /// current session, travelling in the direction that fits our role, have
    /// any effect. A message going the other way belongs to a session with
    /// the roles reversed, which by definition is not the current one.
    fn on_session_message(
        &mut self,
        now: Millis,
        from: &DeviceId,
        msg: CoreMessage,
        out: &mut Vec<Action>,
    ) {
        let (Some(seq), Some(link)) = (msg.seq(), self.phase.link()) else {
            return;
        };
        if link.peer != *from || link.seq != seq {
            return;
        }
        match &self.phase {
            Phase::Idle { .. } => {}
            Phase::Entering(_) | Phase::Controlling(_) => self.on_reply(now, msg, out),
            Phase::Controlled(_) => self.on_command(now, msg, out),
        }
    }

    /// A current-session message from the device we control or are entering.
    fn on_reply(&mut self, now: Millis, msg: CoreMessage, out: &mut Vec<Action>) {
        match msg {
            CoreMessage::Heartbeat { .. } => self.heard(now),
            CoreMessage::EnterAck { .. } => {
                self.heard(now);
                self.start_controlling(now, out);
            }
            CoreMessage::EnterReject { reason, .. } => {
                self.end_as_controller(now, EndReason::Rejected(reason), false, out);
            }
            CoreMessage::TakeBack { .. } => {
                self.end_as_controller(now, EndReason::TakenBack, false, out);
            }
            CoreMessage::Leave { to, entry, .. } => self.on_leave(now, to, entry, out),
            _ => {}
        }
    }

    /// A current-session message from the device controlling us.
    fn on_command(&mut self, now: Millis, msg: CoreMessage, out: &mut Vec<Action>) {
        match msg {
            CoreMessage::Heartbeat { .. } => self.heard(now),
            CoreMessage::Release { .. } => {
                if let Phase::Controlled(c) = std::mem::replace(&mut self.phase, Phase::idle()) {
                    self.finish_as_controlled(c.link, EndReason::Released, None, out);
                }
            }
            CoreMessage::Motion { mark, .. } => {
                self.heard(now);
                self.catch_up(now, mark, out);
            }
            CoreMessage::Input { mark, events, .. } => {
                self.heard(now);
                // Catch up first so a click lands where the controller saw it,
                // even if motion datagrams were lost. If the catch-up carried
                // the cursor off this device, the events are dropped with the
                // session.
                if self.catch_up(now, mark, out) {
                    for event in events {
                        self.inject_for_peer(event, out);
                    }
                }
            }
            _ => {}
        }
    }

    fn heard(&mut self, now: Millis) {
        if let Some(link) = self.phase.link_mut() {
            link.last_heard = now;
        }
    }

    /// Entering → Controlling on `EnterAck`: flush the buffered input.
    fn start_controlling(&mut self, now: Millis, out: &mut Vec<Action>) {
        match std::mem::replace(&mut self.phase, Phase::idle()) {
            Phase::Entering(e) => {
                let mut c = Controlling {
                    link: e.link,
                    departure: e.departure,
                    marks: MarkSender::default(),
                };
                out.push(Action::Notify(Notice::ControlStarted {
                    peer: c.link.peer.clone(),
                }));
                for event in e.buffer {
                    forward(&mut c, now, event, out);
                }
                self.phase = Phase::Controlling(c);
            }
            // A duplicate ack while already controlling changes nothing.
            other => self.phase = other,
        }
    }

    /// Rule 7: the cursor left the device we control.
    fn on_leave(&mut self, now: Millis, to: DeviceId, entry: EntryPoint, out: &mut Vec<Action>) {
        let (link, departure) = match std::mem::replace(&mut self.phase, Phase::idle()) {
            Phase::Entering(e) => (e.link, e.departure),
            Phase::Controlling(c) => (c.link, c.departure),
            other => {
                self.phase = other;
                return;
            }
        };
        if to == self.me {
            let displays = self
                .layout
                .device(&self.me)
                .map_or(&[][..], |d| d.displays());
            let at = layout::place_point(displays, Some(entry.display_id), entry.position)
                .unwrap_or(departure.point);
            out.push(Action::RestoreCursor { at });
            out.push(Action::SetCaptureMode(CaptureMode::Passive));
            // Shortcuts held across the edge keep working here.
            for &code in self.held_keys.iter().filter(|k| k.is_modifier()) {
                out.push(Action::Inject {
                    event: InputEvent::Key {
                        code,
                        pressed: true,
                        repeat: false,
                    },
                    source_os: self.os,
                });
            }
            out.push(Action::Notify(Notice::SessionEnded {
                peer: link.peer,
                reason: EndReason::CursorLeft,
            }));
        } else if to != link.peer && self.peers.contains_key(&to) {
            // On to a third device; capture stays suppressed and the cursor
            // parked.
            out.push(Action::Notify(Notice::SessionEnded {
                peer: link.peer,
                reason: EndReason::CursorLeft,
            }));
            self.enter(now, to, entry, departure, out);
        } else {
            self.finish_as_controller(
                now,
                link,
                departure,
                EndReason::UnreachableTarget,
                false,
                out,
            );
        }
    }

    // ----- local input --------------------------------------------------

    fn on_local_input(
        &mut self,
        now: Millis,
        event: InputEvent,
        cursor: Point,
        out: &mut Vec<Action>,
    ) {
        if !is_usable(&event) {
            return;
        }
        match event {
            InputEvent::Key { code, pressed, .. } => {
                if pressed {
                    self.held_keys.insert(code);
                } else {
                    self.held_keys.remove(&code);
                }
            }
            InputEvent::Button { button, pressed } => {
                if pressed {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
            }
            InputEvent::PointerMotion { .. } | InputEvent::Scroll { .. } => {}
        }
        match &self.phase {
            Phase::Idle { .. } => self.idle_input(now, event, cursor, out),
            Phase::Entering(_) | Phase::Controlling(_) => self.controller_input(now, event, out),
            Phase::Controlled(_) => self.controlled_input(event, out),
        }
    }

    /// Idle: watch for a crossing (the crossing rules of the spec).
    fn idle_input(&mut self, now: Millis, event: InputEvent, cursor: Point, out: &mut Vec<Action>) {
        let InputEvent::PointerMotion { dx, dy } = event else {
            return;
        };
        let displays = self
            .layout
            .device(&self.me)
            .map_or(&[][..], |d| d.displays());
        let exit = match layout::travel(displays, cursor, dx, dy) {
            Some(Travel::Exits(exit)) => Some(exit),
            _ => None,
        };
        let Phase::Idle { push } = &mut self.phase else {
            return;
        };
        let Some(exit) = exit else {
            *push = None;
            return;
        };
        if let Some(Push::Disarmed { display, edge }) = push {
            if *display == exit.display && *edge == exit.edge {
                return;
            }
            *push = None;
        }
        let Some(candidate) = self.candidate(exit) else {
            self.reset_push();
            return;
        };
        let since = match &self.phase {
            Phase::Idle {
                push:
                    Some(Push::Pushing {
                        display,
                        edge,
                        to,
                        since,
                    }),
            } if *display == exit.display && *edge == exit.edge && *to == candidate.crossing.to => {
                *since
            }
            _ => now,
        };
        if now.saturating_sub(since) >= self.config.edge_delay_ms {
            self.cross(now, candidate, out);
        } else {
            self.phase = Phase::Idle {
                push: Some(Push::Pushing {
                    display: exit.display,
                    edge: exit.edge,
                    to: candidate.crossing.to,
                    since,
                }),
            };
        }
    }

    /// The crossing rules other than the edge delay.
    fn candidate(&self, exit: Exit) -> Option<Candidate> {
        if !self.held_buttons.is_empty() || exit.in_corner_dead_zone(self.config.corner_dead_zone) {
            return None;
        }
        let crossing = self.layout.exit_target(
            &self.me,
            exit.display,
            exit.edge,
            exit.at,
            self.config.adjacency_tolerance,
        )?;
        self.peers
            .contains_key(&crossing.to)
            .then_some(Candidate { exit, crossing })
    }

    /// Rule 1: Idle → Entering.
    fn cross(&mut self, now: Millis, candidate: Candidate, out: &mut Vec<Action>) {
        out.push(Action::SetCaptureMode(CaptureMode::Suppress));
        out.push(Action::ParkCursor);
        // Local apps must never see a key stuck down while we are away.
        for &code in &self.held_keys {
            out.push(Action::Inject {
                event: InputEvent::Key {
                    code,
                    pressed: false,
                    repeat: false,
                },
                source_os: self.os,
            });
        }
        let departure = Departure {
            point: candidate.exit.inside_point(),
            display: candidate.exit.display,
            edge: candidate.exit.edge,
        };
        let Crossing { to, entry } = candidate.crossing;
        self.enter(now, to, entry, departure, out);
    }

    /// A new, increasing `seq` for a session this device controls with
    /// `target`: odd when this device has the lower id of the two, even
    /// otherwise.
    ///
    /// Both devices of a pair count independently, so without the split a
    /// session each of them controls could carry the same `seq`. `Heartbeat`
    /// travels both ways with nothing but the `seq`, so after lost messages
    /// two devices that each believe the other controls them would keep each
    /// other alive forever. With the split, a `seq` names exactly one session
    /// of the pair and the stray heartbeats are ignored as stale.
    fn new_seq(&mut self, target: &DeviceId) -> u64 {
        let odd = self.me < *target;
        let seq = if (self.next_seq % 2 == 1) == odd {
            self.next_seq
        } else {
            self.next_seq.saturating_add(1)
        };
        self.next_seq = seq.saturating_add(1);
        seq
    }

    /// Sends `Enter` to `to` and waits for its answer.
    fn enter(
        &mut self,
        now: Millis,
        to: DeviceId,
        entry: EntryPoint,
        departure: Departure,
        out: &mut Vec<Action>,
    ) {
        let seq = self.new_seq(&to);
        let held_keys = self
            .held_keys
            .iter()
            .copied()
            .filter(|k| k.is_modifier())
            .collect();
        let mut link = Link {
            peer: to,
            seq,
            last_heard: now,
            last_sent: now,
        };
        let msg = CoreMessage::Enter {
            seq,
            entry,
            held_keys,
            source_os: self.os,
        };
        send_on(&mut link, now, msg, out);
        self.phase = Phase::Entering(Entering {
            link,
            started: now,
            departure,
            buffer: Vec::new(),
        });
    }

    /// Entering or Controlling: buffer or forward, and watch for the chord.
    fn controller_input(&mut self, now: Millis, event: InputEvent, out: &mut Vec<Action>) {
        if self
            .config
            .emergency_chord
            .is_completed_by(&event, &self.held_keys)
        {
            // The chord's last key is not forwarded.
            self.end_as_controller(now, EndReason::EmergencyChord, true, out);
            return;
        }
        let limit = self.config.enter_buffer_limit;
        match &mut self.phase {
            Phase::Entering(e) => {
                if !buffer_push(&mut e.buffer, event, limit) {
                    self.end_as_controller(now, EndReason::BufferFull, true, out);
                }
            }
            Phase::Controlling(c) => forward(c, now, event, out),
            Phase::Idle { .. } | Phase::Controlled(_) => {}
        }
    }

    /// Controlled: physical input here takes control back (rule 8). The
    /// input itself already reached local apps (capture is passive).
    /// Physical motion is not added to the driven position; the next motion
    /// step puts the cursor back where the controller left it, which is at
    /// most `takeback_threshold` away.
    fn controlled_input(&mut self, event: InputEvent, out: &mut Vec<Action>) {
        let threshold = self.config.takeback_threshold;
        let Phase::Controlled(c) = &mut self.phase else {
            return;
        };
        let take_back = match event {
            InputEvent::Key { pressed: true, .. } | InputEvent::Button { pressed: true, .. } => {
                true
            }
            InputEvent::PointerMotion { dx, dy } => {
                c.local_travel += dx.hypot(dy);
                c.local_travel >= threshold
            }
            _ => false,
        };
        if take_back && let Phase::Controlled(c) = std::mem::replace(&mut self.phase, Phase::idle())
        {
            let msg = CoreMessage::TakeBack { seq: c.link.seq };
            self.finish_as_controlled(c.link, EndReason::TakenBack, Some(msg), out);
        }
    }

    // ----- controlled: motion and injection -----------------------------

    /// Applies motion up to `mark`. Returns false if the session ended
    /// because the cursor left this device.
    fn catch_up(
        &mut self,
        now: Millis,
        mark: hop_types::MotionMark,
        out: &mut Vec<Action>,
    ) -> bool {
        let Phase::Controlled(c) = &mut self.phase else {
            return false;
        };
        match c.marks.catch_up(mark) {
            Some((dx, dy)) => self.motion_step(now, dx, dy, out),
            None => true,
        }
    }

    /// Rule 5: one motion step on the controlled side.
    fn motion_step(&mut self, now: Millis, dx: f64, dy: f64, out: &mut Vec<Action>) -> bool {
        let speed = self.config.pointer_speed;
        let Phase::Controlled(c) = &mut self.phase else {
            return false;
        };
        let displays = self
            .layout
            .device(&self.me)
            .map_or(&[][..], |d| d.displays());
        let Some(travel) = layout::travel(displays, c.pos, dx * speed, dy * speed) else {
            return true;
        };
        let exit = match travel {
            Travel::Inside(to) => {
                c.pos = to;
                out.push(Action::MoveCursorTo { to });
                return true;
            }
            Travel::Exits(exit) => exit,
        };
        let leave =
            if c.buttons.is_empty() && !exit.in_corner_dead_zone(self.config.corner_dead_zone) {
                self.layout
                    .exit_target(
                        &self.me,
                        exit.display,
                        exit.edge,
                        exit.at,
                        self.config.adjacency_tolerance,
                    )
                    .filter(|x| x.to == c.link.peer || self.peers.contains_key(&x.to))
            } else {
                None
            };
        let Some(Crossing { to, entry }) = leave else {
            c.pos = exit.clamped;
            out.push(Action::MoveCursorTo { to: exit.clamped });
            return true;
        };
        if let Phase::Controlled(c) = std::mem::replace(&mut self.phase, Phase::idle()) {
            let msg = CoreMessage::Leave {
                seq: c.link.seq,
                to,
                entry,
            };
            let mut link = c.link;
            // Leave is reliable, so it counts for the heartbeat bookkeeping
            // even though the link is about to be dropped.
            link.last_sent = now;
            self.finish_as_controlled(link, EndReason::CursorLeft, Some(msg), out);
        }
        false
    }

    /// Rule 6: buttons, keys and scroll from the controller.
    fn inject_for_peer(&mut self, event: InputEvent, out: &mut Vec<Action>) {
        let Phase::Controlled(c) = &mut self.phase else {
            return;
        };
        match event {
            // Motion only ever travels as marks.
            InputEvent::PointerMotion { .. } => return,
            InputEvent::Scroll { .. } if !is_usable(&event) => return,
            InputEvent::Button { button, pressed } => {
                if pressed {
                    c.buttons.insert(button);
                } else {
                    c.buttons.remove(&button);
                }
            }
            InputEvent::Key { .. } | InputEvent::Scroll { .. } => {}
        }
        out.push(Action::Inject {
            event,
            source_os: c.source_os,
        });
    }

    // ----- leaving a session --------------------------------------------

    /// Ends Entering or Controlling, if that is the current phase.
    fn end_as_controller(
        &mut self,
        now: Millis,
        reason: EndReason,
        send_release: bool,
        out: &mut Vec<Action>,
    ) {
        match std::mem::replace(&mut self.phase, Phase::idle()) {
            Phase::Entering(Entering {
                link, departure, ..
            })
            | Phase::Controlling(Controlling {
                link, departure, ..
            }) => self.finish_as_controller(now, link, departure, reason, send_release, out),
            other => self.phase = other,
        }
    }

    /// The controller's way back to Idle: restore the cursor where it left,
    /// stop suppressing input, notify.
    fn finish_as_controller(
        &mut self,
        now: Millis,
        mut link: Link,
        departure: Departure,
        reason: EndReason,
        send_release: bool,
        out: &mut Vec<Action>,
    ) {
        if send_release {
            let seq = link.seq;
            send_on(&mut link, now, CoreMessage::Release { seq }, out);
        }
        let displays = self
            .layout
            .device(&self.me)
            .map_or(&[][..], |d| d.displays());
        let at = layout::place_point(displays, Some(departure.display), departure.point)
            .unwrap_or(departure.point);
        out.push(Action::RestoreCursor { at });
        out.push(Action::SetCaptureMode(CaptureMode::Passive));
        out.push(Action::Notify(Notice::SessionEnded {
            peer: link.peer,
            reason,
        }));
        let disarm = !matches!(reason, EndReason::LostTieBreak | EndReason::Superseded);
        self.phase = Phase::Idle {
            push: disarm.then_some(Push::Disarmed {
                display: departure.display,
                edge: departure.edge,
            }),
        };
    }

    /// The controlled side's way back to Idle: optionally tell the
    /// controller, release everything injected, notify.
    fn finish_as_controlled(
        &mut self,
        link: Link,
        reason: EndReason,
        msg: Option<CoreMessage>,
        out: &mut Vec<Action>,
    ) {
        if let Some(msg) = msg {
            out.push(Action::Send {
                to: link.peer.clone(),
                msg,
            });
        }
        out.push(Action::ReleaseAllInjected);
        out.push(Action::Notify(Notice::SessionEnded {
            peer: link.peer,
            reason,
        }));
        self.phase = Phase::idle();
    }
}

/// Sends `msg` to the session peer, keeping the heartbeat bookkeeping.
fn send_on(link: &mut Link, now: Millis, msg: CoreMessage, out: &mut Vec<Action>) {
    if !matches!(msg, CoreMessage::Motion { .. }) {
        link.last_sent = now;
    }
    out.push(Action::Send {
        to: link.peer.clone(),
        msg,
    });
}

/// Rule 4: motion goes out as the new cumulative mark; everything else as
/// `Input` carrying the current mark.
fn forward(c: &mut Controlling, now: Millis, event: InputEvent, out: &mut Vec<Action>) {
    let seq = c.link.seq;
    let msg = match event {
        InputEvent::PointerMotion { dx, dy } => CoreMessage::Motion {
            seq,
            mark: c.marks.push(dx, dy),
        },
        other => CoreMessage::Input {
            seq,
            mark: c.marks.current(),
            events: vec![other],
        },
    };
    send_on(&mut c.link, now, msg, out);
}

/// Buffers `event` while entering. Consecutive motion (and scroll of the same
/// unit) is merged, so a fast mouse cannot fill the buffer while waiting for
/// the ack. Returns false when the buffer is full.
fn buffer_push(buffer: &mut Vec<InputEvent>, event: InputEvent, limit: usize) -> bool {
    match (buffer.last_mut(), event) {
        (
            Some(InputEvent::PointerMotion { dx, dy }),
            InputEvent::PointerMotion {
                dx: more_x,
                dy: more_y,
            },
        ) => {
            *dx += more_x;
            *dy += more_y;
            return true;
        }
        (
            Some(InputEvent::Scroll { dx, dy, unit }),
            InputEvent::Scroll {
                dx: more_x,
                dy: more_y,
                unit: more_unit,
            },
        ) if *unit == more_unit => {
            *dx += more_x;
            *dy += more_y;
            return true;
        }
        _ => {}
    }
    if buffer.len() >= limit {
        return false;
    }
    buffer.push(event);
    true
}

/// Non-finite motion or scroll is dropped before it can poison a mark or a
/// cursor position.
fn is_usable(event: &InputEvent) -> bool {
    match *event {
        InputEvent::PointerMotion { dx, dy } | InputEvent::Scroll { dx, dy, .. } => {
            dx.is_finite() && dy.is_finite()
        }
        InputEvent::Button { .. } | InputEvent::Key { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_types::{Rect, ScrollUnit};

    fn motion(dx: f64, dy: f64) -> InputEvent {
        InputEvent::PointerMotion { dx, dy }
    }

    #[test]
    fn buffer_merges_motion_and_scroll() {
        let mut buf = Vec::new();
        assert!(buffer_push(&mut buf, motion(1.0, 2.0), 2));
        assert!(buffer_push(&mut buf, motion(3.0, 4.0), 2));
        let scroll = |dy| InputEvent::Scroll {
            dx: 0.0,
            dy,
            unit: ScrollUnit::Line,
        };
        assert!(buffer_push(&mut buf, scroll(1.0), 2));
        assert!(buffer_push(&mut buf, scroll(2.0), 2));
        assert_eq!(buf, vec![motion(4.0, 6.0), scroll(3.0)]);
        let click = InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        };
        assert!(!buffer_push(&mut buf, click, 2));
    }

    #[test]
    fn unusable_events_are_recognized() {
        assert!(is_usable(&motion(1.0, 2.0)));
        assert!(!is_usable(&motion(f64::NAN, 0.0)));
        assert!(!is_usable(&InputEvent::Scroll {
            dx: 0.0,
            dy: f64::INFINITY,
            unit: ScrollUnit::Pixel
        }));
    }

    #[test]
    fn state_summary_capture_modes() {
        let id = DeviceId::new("x");
        assert_eq!(State::Idle.capture_mode(), CaptureMode::Passive);
        let entering = State::Entering {
            target: id.clone(),
            seq: 1,
        };
        assert_eq!(entering.capture_mode(), CaptureMode::Suppress);
        assert_eq!(entering.peer(), Some(&id));
        let controlled = State::Controlled {
            by: id.clone(),
            seq: 1,
        };
        assert_eq!(controlled.capture_mode(), CaptureMode::Passive);
        assert_eq!(State::Idle.peer(), None);
    }

    /// A Mac core with a PC to its right and a third device, "zed", to the
    /// PC's right, all connected, after the Mac crossed into the PC (session
    /// 1). Returns the core and the ids of the PC and zed.
    fn controlling_the_middle_of_three() -> (Core, DeviceId, DeviceId) {
        let display = |w: f64, h: f64| DisplayInfo {
            id: 1,
            frame: Rect::new(0.0, 0.0, w, h),
            scale: 1.0,
            primary: true,
        };
        let (mac, pc, zed) = (
            DeviceId::new("mac"),
            DeviceId::new("pc"),
            DeviceId::new("zed"),
        );
        let layout = Layout::new([
            crate::DeviceLayout::new(
                mac.clone(),
                Point::new(0.0, 0.0),
                vec![display(1512.0, 982.0)],
            ),
            crate::DeviceLayout::new(
                pc.clone(),
                Point::new(1512.0, 0.0),
                vec![display(1536.0, 982.0)],
            ),
            crate::DeviceLayout::new(
                zed.clone(),
                Point::new(3048.0, 0.0),
                vec![display(800.0, 982.0)],
            ),
        ])
        .unwrap();
        let mut core = Core::new(mac.clone(), Os::MacOs, CoreConfig::default(), layout);
        for (peer, width) in [(&pc, 1536.0), (&zed, 800.0)] {
            core.handle(
                0,
                Event::PeerConnected {
                    peer: peer.clone(),
                    os: Os::Windows,
                    displays: vec![display(width, 982.0)],
                },
            );
        }
        let out = core.handle(
            10,
            Event::LocalInput {
                event: motion(50.0, 0.0),
                cursor: Point::new(1500.0, 500.0),
                physical: true,
            },
        );
        assert!(out.contains(&Action::SetCaptureMode(CaptureMode::Suppress)));
        core.handle(
            20,
            Event::PeerMessage {
                from: pc.clone(),
                msg: CoreMessage::EnterAck { seq: 1 },
            },
        );
        assert_eq!(
            core.state(),
            State::Controlling {
                target: pc.clone(),
                seq: 1
            }
        );
        (core, pc, zed)
    }

    /// Rule 7 for more than two devices.
    #[test]
    fn leave_toward_a_third_device_enters_it() {
        let (mut core, pc, third) = controlling_the_middle_of_three();
        let msg = |msg| Event::PeerMessage {
            from: pc.clone(),
            msg,
        };
        let entry = EntryPoint {
            display_id: 1,
            edge: Edge::Left,
            position: Point::new(1.0, 300.0),
        };
        let out = core.handle(
            30,
            msg(CoreMessage::Leave {
                seq: 1,
                to: third.clone(),
                entry,
            }),
        );
        assert_eq!(
            out,
            vec![
                Action::Notify(Notice::SessionEnded {
                    peer: pc.clone(),
                    reason: EndReason::CursorLeft
                }),
                Action::Send {
                    to: third.clone(),
                    msg: CoreMessage::Enter {
                        seq: 3,
                        entry,
                        held_keys: vec![],
                        source_os: Os::MacOs
                    }
                },
            ]
        );
        assert_eq!(
            core.state(),
            State::Entering {
                target: third.clone(),
                seq: 3
            }
        );
        let out = core.handle(
            40,
            Event::PeerMessage {
                from: third.clone(),
                msg: CoreMessage::EnterAck { seq: 3 },
            },
        );
        assert_eq!(
            out,
            vec![Action::Notify(Notice::ControlStarted {
                peer: third.clone()
            })]
        );
        // A Leave toward a device we are not connected to fails open.
        let out = core.handle(
            50,
            Event::PeerMessage {
                from: third.clone(),
                msg: CoreMessage::Leave {
                    seq: 3,
                    to: DeviceId::new("nobody"),
                    entry,
                },
            },
        );
        assert!(out.contains(&Action::SetCaptureMode(CaptureMode::Passive)));
        assert_eq!(core.state(), State::Idle);
    }

    /// Rule 3: an Enter from a device other than the session peer is
    /// refused as busy, and the session carries on.
    #[test]
    fn a_third_device_is_told_busy() {
        let (mut core, pc, zed) = controlling_the_middle_of_three();
        let enter = |seq| Event::PeerMessage {
            from: zed.clone(),
            msg: CoreMessage::Enter {
                seq,
                entry: EntryPoint {
                    display_id: 1,
                    edge: Edge::Right,
                    position: Point::new(1511.0, 300.0),
                },
                held_keys: vec![KeyCode::LEFT_SHIFT],
                source_os: Os::Windows,
            },
        };
        let busy = |seq| Action::Send {
            to: zed.clone(),
            msg: CoreMessage::EnterReject {
                seq,
                reason: RejectReason::Busy,
            },
        };
        assert_eq!(core.handle(30, enter(2)), vec![busy(2)]);
        assert_eq!(
            core.state(),
            State::Controlling {
                target: pc.clone(),
                seq: 1
            }
        );
        // The same while the PC controls the Mac.
        core.handle(
            40,
            Event::PeerMessage {
                from: pc.clone(),
                msg: CoreMessage::Enter {
                    seq: 2,
                    entry: EntryPoint {
                        display_id: 1,
                        edge: Edge::Right,
                        position: Point::new(1511.0, 300.0),
                    },
                    held_keys: vec![],
                    source_os: Os::Windows,
                },
            },
        );
        assert_eq!(
            core.state(),
            State::Controlled {
                by: pc.clone(),
                seq: 2
            }
        );
        assert_eq!(core.handle(50, enter(4)), vec![busy(4)]);
        assert_eq!(
            core.state(),
            State::Controlled {
                by: pc.clone(),
                seq: 2
            }
        );
    }

    #[test]
    fn seqs_are_split_by_parity_within_each_pair() {
        let me = DeviceId::new("mac");
        let mut core = Core::new(me, Os::MacOs, CoreConfig::default(), Layout::default());
        // Odd toward a peer with a higher id, even toward a lower one, and
        // always increasing.
        let (higher, lower) = (DeviceId::new("pc"), DeviceId::new("aa"));
        let seqs: Vec<u64> = [&higher, &higher, &lower, &higher, &lower, &lower]
            .into_iter()
            .map(|target| core.new_seq(target))
            .collect();
        assert_eq!(seqs, vec![1, 3, 4, 5, 6, 8]);
    }
}
