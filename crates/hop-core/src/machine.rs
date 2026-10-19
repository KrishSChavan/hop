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
use crate::{CoreConfig, Crossing, Millis, SharedEdge};

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
    /// The runtime can no longer vouch for this device's input: its
    /// suppression lease lapsed (Hop stalled, and capture already went back
    /// to passive by itself), captured input was dropped, or a platform call
    /// a session depends on (suppressing, parking, injecting) failed.
    ///
    /// A cursor hidden by [`Action::ConcealCursor`] is shown again first
    /// ([`Action::RevealCursor`]). The core ends any session, failing open,
    /// with [`EndReason::Stalled`]:
    /// - while entering or controlling, as for the emergency chord: it sends
    ///   `Release`, restores the cursor at the exit point, sets capture to
    ///   passive, and disarms the edge the cursor left through;
    /// - while controlled, it sends `TakeBack` and emits
    ///   [`Action::ReleaseAllInjected`].
    ///
    /// Then, in every state, it stops vouching for the keys it believed
    /// held, since it may have missed their key-ups:
    /// - They are never pressed for a peer (in the next `Enter`) or pressed
    ///   again for local apps when the cursor comes home, so a missed key-up
    ///   cannot leave a modifier stuck down. A key the user keeps holding
    ///   through the stall therefore does not follow the cursor until it is
    ///   pressed again (lost, never stuck).
    /// - They still get a local key-up at the next crossing, and still count
    ///   toward the emergency chord, where counting a key as held when it
    ///   is not does no harm.
    /// - A modifier pressed again for local apps when the cursor came home
    ///   is released at once, as its key-up may be the one that was missed.
    ///
    /// Any news of a key (down or up) settles its state again. Mouse buttons
    /// are kept as they were: a cursor that crossed while a button was still
    /// down could leave it stuck down here, so a missed button-up only
    /// blocks crossings until the next click. With no session, no key
    /// believed held and no hidden cursor, the event changes nothing, so
    /// feeding it twice in a row is harmless.
    LocalStall,
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
    /// Send `to` a tiny unreliable packet so its Wi-Fi radio is awake when
    /// the cursor crosses. Emitted while idle, for physical pointer motion
    /// that stays on this device's displays and heads for a part of an edge
    /// that leads to the connected peer `to` (one of
    /// [`Core::edges_toward`]): the motion moves toward that edge (it has a
    /// component along the edge's outward normal) and ends on the edge's
    /// display, level with the part (within its span along the edge) and at
    /// most `nudge_distance` from the edge line. At most one per event for
    /// each peer, and none to a peer nudged less than `nudge_interval_ms`
    /// before. Best effort: a lost nudge changes nothing.
    Nudge {
        /// The device the cursor is heading for.
        to: DeviceId,
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
    /// Hide the local cursor where it is. Emitted only when a session in
    /// which this device was controlled ends because the cursor left
    /// through an edge ([`EndReason::CursorLeft`]): after `Leave` is sent
    /// and [`Action::ReleaseAllInjected`], before the
    /// [`Notice::SessionEnded`], so the cursor the controller drove here
    /// does not stay visible at that edge. No other ending hides it.
    /// Nothing is frozen, moved or suppressed, and the platform shows the
    /// cursor again by itself when this device's own mouse is used.
    ConcealCursor,
    /// Show a cursor hidden by [`Action::ConcealCursor`] again. Emitted only
    /// while it is hidden, as the first action of the list: on physical
    /// pointer input here (motion, a button press, scrolling; keys and
    /// button releases do not count), when a peer enters this device again
    /// (before [`Action::MoveCursorTo`]), and on [`Event::LocalStall`]. The
    /// cursor is never hidden outside Idle.
    RevealCursor,
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
    /// Release every key and button the runtime injected. Emitted on every
    /// exit from Controlled, and also when this device becomes controlled,
    /// so nothing an earlier session left pressed carries over into the new
    /// one.
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
    /// This device could no longer vouch for its own input (a lapsed
    /// suppression lease, dropped input or a failed platform call; see
    /// [`Event::LocalStall`]), so it ended the session. The peer sees the
    /// session end as [`EndReason::Released`] or [`EndReason::TakenBack`].
    Stalled,
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
/// - [`Event::LocalStall`] (docs/M2-DESIGN.md) ends any session the way the
///   emergency chord or a take-back does. The design says the core then
///   forgets the keys and buttons it believed held. Keys are forgotten only
///   where a wrong guess would press one: they are never pressed for a peer
///   or again for local apps, but they still get a local key-up at the next
///   crossing and still count toward the chord, so a key that really is
///   still down cannot get stuck here and the chord keeps working. Buttons
///   are kept, so the cursor never crosses while one may still be down.
/// - When a session in which this device was controlled ends because the
///   cursor left through an edge, the cursor the controller drove here is
///   hidden where it is ([`Action::ConcealCursor`], docs/M4-M5-DESIGN.md),
///   so it does not stay visible at that edge. No other ending hides it: a
///   take-back, a release, a timeout, a disconnect, a replaced session or a
///   stall is not the cursor visibly leaving, and failing open favours a
///   visible cursor. It is shown again ([`Action::RevealCursor`], first in
///   its list) on physical pointer input here (motion, a button press,
///   scrolling; never a key, since typing needs no pointer), when a peer
///   enters again, and on [`Event::LocalStall`]. Both ways out of Idle, a
///   crossing and an accepted `Enter`, pass one of those, so the cursor is
///   hidden only while idle. A dropped connection leaves it hidden: the
///   user is at the other computer.
/// - While idle, physical pointer motion that stays on this device's
///   displays nudges each connected peer it heads for ([`Action::Nudge`],
///   docs/M4-M5-DESIGN.md), so a dozing Wi-Fi radio is awake by the time the
///   cursor crosses: the motion moves toward a part of an edge that leads to
///   that peer ([`Core::edges_toward`], whole edges minus the corner dead
///   zones) and ends level with it, at most `nudge_distance` from the edge.
///   A peer gets at most one nudge per event and one every
///   `nudge_interval_ms`; a `nudge_distance` of 0 turns nudges off. Motion
///   that would leave the screen does not nudge: it pushes against an edge,
///   where the crossing rules decide.
#[derive(Clone, Debug)]
pub struct Core {
    me: DeviceId,
    os: Os,
    config: CoreConfig,
    layout: Layout,
    /// Connected peers and the displays each reported (empty if refused).
    peers: BTreeMap<DeviceId, Vec<DisplayInfo>>,
    /// Keys physically held on this device, tracked in every state. A stall
    /// moves them all to `maybe_held`.
    held_keys: BTreeSet<KeyCode>,
    /// Keys that were held when the runtime last reported a stall
    /// ([`Event::LocalStall`]) and have not gone down or up since. Their
    /// key-up may have been missed, so they are no longer in `held_keys`:
    /// they are never pressed for a peer or again for local apps. They still
    /// get a key-up at a crossing and still count toward the emergency
    /// chord, where treating a key as held when it is not does no harm.
    maybe_held: BTreeSet<KeyCode>,
    /// Mouse buttons physically held on this device, tracked in every state.
    held_buttons: BTreeSet<MouseButton>,
    /// Modifiers pressed again for local apps when the cursor came home
    /// (rule 7) whose physical key is still down. The injector remembers
    /// them, so when the physical key comes up the core also releases the
    /// injected copy; otherwise the injector would keep the modifier down
    /// and add it to the next session's keys (Cmd+Q instead of Q).
    repressed: BTreeSet<KeyCode>,
    /// Whether the cursor a session left at this device's edge is hidden
    /// ([`Action::ConcealCursor`]). Only ever true while idle.
    cursor_concealed: bool,
    /// The parts of this device's edges that lead to each connected peer
    /// ([`Core::edges_toward`]), for nudges. Recomputed whenever the layout
    /// or the connected peers change; peers no edge leads to are left out.
    shared_edges: BTreeMap<DeviceId, Vec<SharedEdge>>,
    /// When each peer was last nudged ([`Action::Nudge`]). Kept across a
    /// reconnect, so a peer is never nudged more often than
    /// `nudge_interval_ms`; one entry per peer, so it stays small.
    last_nudge: BTreeMap<DeviceId, Millis>,
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
            maybe_held: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            repressed: BTreeSet::new(),
            cursor_concealed: false,
            shared_edges: BTreeMap::new(),
            last_nudge: BTreeMap::new(),
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
            Event::LocalStall => self.on_local_stall(now, out),
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

    /// The parts of this device's display edges from which the cursor
    /// crosses to `peer`, in this device's own coordinates, for drop zones:
    /// [`Layout::edges_toward`] with this core's adjacency tolerance and
    /// corner dead zone, in the layout as it stands. Empty while `peer` is
    /// not connected, since the cursor never crosses to it then: ask after
    /// the core has handled the peer's [`Event::PeerConnected`], and again
    /// after each [`Event::LayoutChanged`].
    pub fn edges_toward(&self, peer: &DeviceId) -> Vec<SharedEdge> {
        if !self.peers.contains_key(peer) {
            return Vec::new();
        }
        self.layout.edges_toward(
            &self.me,
            peer,
            self.config.adjacency_tolerance,
            self.config.corner_dead_zone,
        )
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
        self.refresh_shared_edges();
    }

    fn on_peer_disconnected(&mut self, now: Millis, peer: &DeviceId, out: &mut Vec<Action>) {
        self.peers.remove(peer);
        self.end_session_with(now, peer, EndReason::Disconnected, out);
        self.reset_push();
        self.refresh_shared_edges();
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
        self.refresh_shared_edges();
    }

    /// Recomputes [`Core::shared_edges`] for the layout and the connected
    /// peers as they stand.
    fn refresh_shared_edges(&mut self) {
        self.shared_edges = self
            .peers
            .keys()
            .map(|peer| (peer.clone(), self.edges_toward(peer)))
            .filter(|(_, edges)| !edges.is_empty())
            .collect();
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
        // The cursor the last session left hidden at the edge is about to
        // move, and it must be seen where it goes.
        self.reveal_cursor(out);
        // Start the session with nothing injected: a modifier pressed again
        // on an earlier return must not leak into the peer's keys.
        out.push(Action::ReleaseAllInjected);
        self.repressed.clear();
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
                self.repressed.insert(code);
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
        // This device's own mouse shows a cursor the last session left
        // hidden, before anything else happens (a crossing parks it). Keys
        // do not: typing needs no pointer.
        if matches!(
            event,
            InputEvent::PointerMotion { .. }
                | InputEvent::Button { pressed: true, .. }
                | InputEvent::Scroll { .. }
        ) {
            self.reveal_cursor(out);
        }
        match event {
            InputEvent::Key { code, pressed, .. } => {
                // Any news of a key settles whether it is down.
                self.maybe_held.remove(&code);
                if pressed {
                    self.held_keys.insert(code);
                } else {
                    self.held_keys.remove(&code);
                    if self.repressed.remove(&code) {
                        // The physical key-up already reached local apps;
                        // this one clears the injector's copy of the press.
                        out.push(Action::Inject {
                            event: InputEvent::Key {
                                code,
                                pressed: false,
                                repeat: false,
                            },
                            source_os: self.os,
                        });
                    }
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

    /// Idle: watch for a crossing (the crossing rules of the spec), and
    /// nudge the peers the cursor heads for.
    fn idle_input(&mut self, now: Millis, event: InputEvent, cursor: Point, out: &mut Vec<Action>) {
        let InputEvent::PointerMotion { dx, dy } = event else {
            return;
        };
        let displays = self
            .layout
            .device(&self.me)
            .map_or(&[][..], |d| d.displays());
        let exit = match layout::travel(displays, cursor, dx, dy) {
            Some(Travel::Exits(exit)) => exit,
            travel => {
                // Not pushing against an edge (any more).
                self.reset_push();
                if let Some(Travel::Inside(to)) = travel {
                    self.nudge_peers(now, to, dx, dy, out);
                }
                return;
            }
        };
        let Phase::Idle { push } = &mut self.phase else {
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

    /// Idle motion by `(dx, dy)` that ends on screen at `to`: nudges each
    /// connected peer with an edge that the motion heads for and ends within
    /// `nudge_distance` of, unless that peer was nudged less than
    /// `nudge_interval_ms` ago. Peers come in id order, each at most once.
    fn nudge_peers(&mut self, now: Millis, to: Point, dx: f64, dy: f64, out: &mut Vec<Action>) {
        let reach = self.config.nudge_distance;
        // 0 turns nudges off; it would still match a cursor right on a
        // left or top edge line.
        if reach <= 0.0 {
            return;
        }
        let interval = self.config.nudge_interval_ms;
        for (peer, edges) in &self.shared_edges {
            if !edges.iter().any(|e| e.is_approached(to, dx, dy, reach)) {
                continue;
            }
            let due = self
                .last_nudge
                .get(peer)
                .is_none_or(|&last| now.saturating_sub(last) >= interval);
            if due {
                self.last_nudge.insert(peer.clone(), now);
                out.push(Action::Nudge { to: peer.clone() });
            }
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
        // Local apps must never see a key stuck down while we are away. That
        // includes keys that may still be down after a stall: a key-up for a
        // key that is already up is harmless.
        for &code in self.held_keys.union(&self.maybe_held) {
            out.push(Action::Inject {
                event: InputEvent::Key {
                    code,
                    pressed: false,
                    repeat: false,
                },
                source_os: self.os,
            });
        }
        // Those key-ups also cleared the injector's copy of any modifier
        // pressed again on an earlier return.
        self.repressed.clear();
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
        let chord = &self.config.emergency_chord;
        // Keys that may still be down after a stall count too: the chord
        // only ever ends a session, so counting a key wrongly is harmless,
        // while ignoring one could leave the user without a way out.
        let completed = if self.maybe_held.is_empty() {
            chord.is_completed_by(&event, &self.held_keys)
        } else {
            let held = self.held_keys.union(&self.maybe_held).copied().collect();
            chord.is_completed_by(&event, &held)
        };
        if completed {
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

    // ----- stalls -------------------------------------------------------

    /// [`Event::LocalStall`]: show a hidden cursor, end any session, failing
    /// open, then stop vouching for the keys believed held.
    fn on_local_stall(&mut self, now: Millis, out: &mut Vec<Action>) {
        // Fail open: Hop can no longer vouch for this device, so it does not
        // leave a cursor hidden, even though the platform would show it at
        // the next mouse event anyway.
        self.reveal_cursor(out);
        match std::mem::replace(&mut self.phase, Phase::idle()) {
            // As for the emergency chord. The Release also frees a peer that
            // accepted an Enter we never heard the answer to.
            Phase::Entering(Entering {
                link, departure, ..
            })
            | Phase::Controlling(Controlling {
                link, departure, ..
            }) => self.finish_as_controller(now, link, departure, EndReason::Stalled, true, out),
            Phase::Controlled(c) => {
                let msg = CoreMessage::TakeBack { seq: c.link.seq };
                self.finish_as_controlled(c.link, EndReason::Stalled, Some(msg), out);
            }
            // No session to end. Edge tracking stays as it is, so an edge
            // disarmed by an earlier ending stays disarmed.
            idle @ Phase::Idle { .. } => self.phase = idle,
        }
        self.stop_vouching_for_keys(out);
    }

    /// Moves every key believed held to `maybe_held` (see
    /// [`Event::LocalStall`]). Buttons are kept on purpose.
    ///
    /// Plain forgetting would not do: a key that really is still down would
    /// get no key-up at the next crossing (rule 1), capture would then
    /// swallow its physical key-up, and it would stay stuck down here while
    /// the cursor is away.
    fn stop_vouching_for_keys(&mut self, out: &mut Vec<Action>) {
        // The injector holds a modifier pressed again on the way home until
        // the core sees its physical key-up, and that may be what was
        // missed. Release it now, even if the user may still be holding it.
        for code in std::mem::take(&mut self.repressed) {
            out.push(Action::Inject {
                event: InputEvent::Key {
                    code,
                    pressed: false,
                    repeat: false,
                },
                source_os: self.os,
            });
        }
        let mut held = std::mem::take(&mut self.held_keys);
        self.maybe_held.append(&mut held);
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
    /// controller, release everything injected, hide the cursor if it left
    /// through an edge, notify.
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
        // The cursor moved on to another device, so the one the controller
        // drove here would only stay visible at this edge. Every other
        // ending leaves it visible (fail open).
        if reason == EndReason::CursorLeft {
            out.push(Action::ConcealCursor);
            self.cursor_concealed = true;
        }
        out.push(Action::Notify(Notice::SessionEnded {
            peer: link.peer,
            reason,
        }));
        self.phase = Phase::idle();
    }

    /// Shows the cursor again if [`Action::ConcealCursor`] hid it.
    fn reveal_cursor(&mut self, out: &mut Vec<Action>) {
        if std::mem::take(&mut self.cursor_concealed) {
            out.push(Action::RevealCursor);
        }
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
    use hop_types::{MotionMark, Rect, ScrollUnit};

    fn motion(dx: f64, dy: f64) -> InputEvent {
        InputEvent::PointerMotion { dx, dy }
    }

    /// Physical input on this device, with the cursor at `cursor`.
    fn local(event: InputEvent, cursor: Point) -> Event {
        Event::LocalInput {
            event,
            cursor,
            physical: true,
        }
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

    /// The exact actions of `LocalStall` in each role, as the agent carries
    /// them out.
    #[test]
    fn a_stall_ends_the_session_with_exactly_these_actions() {
        let (mut core, pc, _) = controlling_the_middle_of_three();
        assert_eq!(
            core.handle(30, Event::LocalStall),
            vec![
                Action::Send {
                    to: pc.clone(),
                    msg: CoreMessage::Release { seq: 1 }
                },
                Action::RestoreCursor {
                    at: Point::new(1511.0, 500.0)
                },
                Action::SetCaptureMode(CaptureMode::Passive),
                Action::Notify(Notice::SessionEnded {
                    peer: pc.clone(),
                    reason: EndReason::Stalled
                }),
            ]
        );
        assert_eq!(core.state(), State::Idle);
        assert_eq!(core.handle(40, Event::LocalStall), vec![], "nothing left");

        // Controlled by the PC, in its session 2.
        let entry = EntryPoint {
            display_id: 1,
            edge: Edge::Right,
            position: Point::new(1511.0, 300.0),
        };
        let enter = CoreMessage::Enter {
            seq: 2,
            entry,
            held_keys: vec![],
            source_os: Os::Windows,
        };
        core.handle(
            50,
            Event::PeerMessage {
                from: pc.clone(),
                msg: enter,
            },
        );
        assert!(matches!(core.state(), State::Controlled { seq: 2, .. }));
        assert_eq!(
            core.handle(60, Event::LocalStall),
            vec![
                Action::Send {
                    to: pc.clone(),
                    msg: CoreMessage::TakeBack { seq: 2 }
                },
                Action::ReleaseAllInjected,
                Action::Notify(Notice::SessionEnded {
                    peer: pc,
                    reason: EndReason::Stalled
                }),
            ]
        );
        assert_eq!(core.state(), State::Idle);
    }

    #[test]
    fn edges_lead_to_a_peer_only_while_it_is_connected() {
        let (mac, pc) = (DeviceId::new("mac"), DeviceId::new("pc"));
        let display = |w: f64, h: f64| DisplayInfo {
            id: 1,
            frame: Rect::new(0.0, 0.0, w, h),
            scale: 1.0,
            primary: true,
        };
        let laptop = display(1512.0, 982.0);
        let layout = Layout::two_devices(
            mac.clone(),
            vec![laptop.clone()],
            pc.clone(),
            Edge::Right,
            vec![],
        )
        .unwrap();
        let config = CoreConfig {
            corner_dead_zone: 100.0,
            ..CoreConfig::default()
        };
        let mut core = Core::new(mac, Os::MacOs, config, layout);
        assert_eq!(core.edges_toward(&pc), []);
        core.handle(
            0,
            Event::PeerConnected {
                peer: pc.clone(),
                os: Os::Windows,
                displays: vec![display(1536.0, 864.0)],
            },
        );
        // The whole of the Mac's right edge leads to the PC, also above and
        // below the shorter PC (beside y 59 to 923); the core's own corner
        // dead zone trims it.
        assert_eq!(
            core.edges_toward(&pc),
            [SharedEdge {
                display: 1,
                frame: laptop.frame,
                edge: Edge::Right,
                start: 100.0,
                end: 882.0,
            }]
        );
        core.handle(10, Event::PeerDisconnected { peer: pc.clone() });
        assert_eq!(core.edges_toward(&pc), []);
    }

    /// On the user's own laptops (the PC on the Mac's right), the cursor
    /// pushed against the Mac's right edge crosses to the PC exactly where
    /// that edge leads to the PC: drop zones cover all of it and no more.
    #[test]
    fn the_cursor_crosses_exactly_where_the_edges_lead() {
        let (mac, pc) = (DeviceId::new("mac"), DeviceId::new("pc"));
        let display = |w: f64, h: f64| DisplayInfo {
            id: 1,
            frame: Rect::new(0.0, 0.0, w, h),
            scale: 1.0,
            primary: true,
        };
        let connected = || {
            let layout = Layout::two_devices(
                mac.clone(),
                vec![display(1512.0, 982.0)],
                pc.clone(),
                Edge::Right,
                vec![],
            )
            .unwrap();
            let mut core = Core::new(mac.clone(), Os::MacOs, CoreConfig::default(), layout);
            core.handle(
                0,
                Event::PeerConnected {
                    peer: pc.clone(),
                    os: Os::Windows,
                    displays: vec![display(1536.0, 864.0)],
                },
            );
            core
        };
        let edges = connected().edges_toward(&pc);
        assert_eq!(
            edges,
            [SharedEdge {
                display: 1,
                frame: Rect::new(0.0, 0.0, 1512.0, 982.0),
                edge: Edge::Right,
                start: 10.0,
                end: 972.0,
            }]
        );
        for y in (0..982).map(|y| f64::from(y) + 0.5) {
            let mut core = connected();
            core.handle(
                10,
                Event::LocalInput {
                    event: motion(20.0, 0.0),
                    cursor: Point::new(1505.0, y),
                    physical: true,
                },
            );
            let crossed = matches!(core.state(), State::Entering { .. });
            let leads = edges.iter().any(|e| e.start <= y && y <= e.end);
            assert_eq!(crossed, leads, "at y = {y}");
        }
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

    // ----- the cursor left behind (docs/M4-M5-DESIGN.md, section 2) -----

    /// A display with its top-left corner at `(x, y)` of its device's own
    /// coordinates.
    fn screen(id: u32, x: f64, y: f64, width: f64, height: f64) -> DisplayInfo {
        DisplayInfo {
            id,
            frame: Rect::new(x, y, width, height),
            scale: 1.0,
            primary: x == 0.0 && y == 0.0,
        }
    }

    /// The user's desk from the PC's side, with `config`: the PC's 1536x864
    /// screen with the Mac's 1512x982 laptop to its left, connected.
    /// Returns the core and the Mac's id.
    fn the_pc_beside_the_mac_with(config: CoreConfig) -> (Core, DeviceId) {
        let (pc, mac) = (DeviceId::new("pc"), DeviceId::new("mac"));
        let layout = Layout::two_devices(
            pc.clone(),
            vec![screen(1, 0.0, 0.0, 1536.0, 864.0)],
            mac.clone(),
            Edge::Left,
            vec![],
        )
        .unwrap();
        let mut core = Core::new(pc, Os::Windows, config, layout);
        core.handle(
            0,
            Event::PeerConnected {
                peer: mac.clone(),
                os: Os::MacOs,
                displays: vec![screen(1, 0.0, 0.0, 1512.0, 982.0)],
            },
        );
        (core, mac)
    }

    /// The Mac's `Enter` for its session `seq`, onto the middle of the PC's
    /// left edge.
    fn enter_from_the_mac(seq: u64) -> CoreMessage {
        CoreMessage::Enter {
            seq,
            entry: EntryPoint {
                display_id: 1,
                edge: Edge::Left,
                position: Point::new(1.0, 432.0),
            },
            held_keys: vec![],
            source_os: Os::MacOs,
        }
    }

    /// The PC after the Mac drove its cursor in (session 1, at t=10) and
    /// back out through the PC's left edge (at t=20). Returns the core, the
    /// Mac's id and the actions of that last motion.
    fn the_pc_after_the_cursor_left() -> (Core, DeviceId, Vec<Action>) {
        let (mut core, mac) = the_pc_beside_the_mac_with(CoreConfig::default());
        let from_mac = |msg| Event::PeerMessage {
            from: mac.clone(),
            msg,
        };
        core.handle(10, from_mac(enter_from_the_mac(1)));
        assert!(matches!(core.state(), State::Controlled { seq: 1, .. }));
        // 200 units to the left of a cursor 1 unit inside the edge.
        let mark = MotionMark {
            n: 1,
            total_dx: -200.0,
            total_dy: 0.0,
        };
        let out = core.handle(20, from_mac(CoreMessage::Motion { seq: 1, mark }));
        assert_eq!(core.state(), State::Idle);
        (core, mac, out)
    }

    /// The controlled side's exact actions when the cursor leaves it
    /// through an edge, as the agent carries them out: the Mac hears first,
    /// nothing injected stays down, and the cursor is hidden before the
    /// session is reported over.
    #[test]
    fn the_cursor_left_behind_is_hidden_with_exactly_these_actions() {
        let (_, mac, out) = the_pc_after_the_cursor_left();
        let entry = EntryPoint {
            display_id: 1,
            edge: Edge::Right,
            position: Point::new(1511.0, 491.0),
        };
        assert_eq!(
            out,
            vec![
                Action::Send {
                    to: mac.clone(),
                    msg: CoreMessage::Leave {
                        seq: 1,
                        to: mac.clone(),
                        entry
                    }
                },
                Action::ReleaseAllInjected,
                Action::ConcealCursor,
                Action::Notify(Notice::SessionEnded {
                    peer: mac,
                    reason: EndReason::CursorLeft
                }),
            ]
        );
    }

    #[test]
    fn a_peer_entering_again_shows_the_hidden_cursor_before_moving_it() {
        let (mut core, mac, _) = the_pc_after_the_cursor_left();
        let from_mac = |msg| Event::PeerMessage {
            from: mac.clone(),
            msg,
        };
        // Typing on the PC needs no pointer: the cursor stays hidden.
        for pressed in [true, false] {
            let shift = InputEvent::Key {
                code: KeyCode::LEFT_SHIFT,
                pressed,
                repeat: false,
            };
            assert_eq!(core.handle(30, local(shift, Point::new(1.0, 432.0))), []);
        }
        assert_eq!(
            core.handle(40, from_mac(enter_from_the_mac(3))),
            vec![
                Action::RevealCursor,
                Action::ReleaseAllInjected,
                Action::MoveCursorTo {
                    to: Point::new(1.0, 432.0)
                },
                Action::Send {
                    to: mac.clone(),
                    msg: CoreMessage::EnterAck { seq: 3 }
                },
                Action::Notify(Notice::ControlledBy { peer: mac.clone() }),
            ]
        );
        // A release hides nothing, so a stall afterwards has nothing to
        // show again.
        assert_eq!(
            core.handle(50, from_mac(CoreMessage::Release { seq: 3 })),
            vec![
                Action::ReleaseAllInjected,
                Action::Notify(Notice::SessionEnded {
                    peer: mac,
                    reason: EndReason::Released
                }),
            ]
        );
        assert_eq!(core.handle(60, Event::LocalStall), []);
    }

    #[test]
    fn pointer_input_here_shows_the_hidden_cursor_first() {
        let at = Point::new(1.0, 432.0);
        let press = InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        };
        let scroll = InputEvent::Scroll {
            dx: 0.0,
            dy: 1.0,
            unit: ScrollUnit::Line,
        };
        // To the right, away from the edge that leads to the Mac, so the
        // motion nudges nothing.
        for event in [motion(5.0, 0.0), press, scroll] {
            let (mut core, _, _) = the_pc_after_the_cursor_left();
            assert_eq!(
                core.handle(30, local(event, at)),
                [Action::RevealCursor],
                "{event:?}"
            );
            assert_eq!(core.handle(40, local(event, at)), [], "only once");
        }
        // Neither does a button release (the button may have gone down
        // before the cursor came here), nor input with unusable numbers.
        let (mut core, mac, _) = the_pc_after_the_cursor_left();
        let release = InputEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        };
        assert_eq!(core.handle(30, local(release, at)), []);
        assert_eq!(core.handle(30, local(motion(f64::NAN, 0.0), at)), []);
        // A motion that crosses at once shows the cursor before parking it.
        assert_eq!(
            core.handle(40, local(motion(-50.0, 0.0), Point::new(5.0, 432.0))),
            vec![
                Action::RevealCursor,
                Action::SetCaptureMode(CaptureMode::Suppress),
                Action::ParkCursor,
                Action::Send {
                    to: mac,
                    msg: CoreMessage::Enter {
                        seq: 2,
                        entry: EntryPoint {
                            display_id: 1,
                            edge: Edge::Right,
                            position: Point::new(1511.0, 491.0),
                        },
                        held_keys: vec![],
                        source_os: Os::Windows,
                    }
                },
            ]
        );
    }

    #[test]
    fn a_stall_shows_the_hidden_cursor() {
        let (mut core, _, _) = the_pc_after_the_cursor_left();
        assert_eq!(core.handle(30, Event::LocalStall), [Action::RevealCursor]);
        assert_eq!(core.handle(40, Event::LocalStall), [], "nothing left");
    }

    /// Only the cursor leaving through an edge hides it. Every other end of
    /// a session this device was controlled in leaves the cursor shown, so
    /// there is nothing to show again afterwards either.
    #[test]
    fn no_other_ending_hides_the_cursor() {
        let mac = DeviceId::new("mac");
        let at = Point::new(1.0, 432.0);
        let from_mac = |msg| Event::PeerMessage {
            from: mac.clone(),
            msg,
        };
        let press = InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
            repeat: false,
        };
        let scroll = InputEvent::Scroll {
            dx: 0.0,
            dy: 1.0,
            unit: ScrollUnit::Line,
        };
        let endings = [
            (
                EndReason::Released,
                20,
                from_mac(CoreMessage::Release { seq: 1 }),
            ),
            (EndReason::TakenBack, 20, local(press, at)),
            // Nothing heard since the session began at t=10.
            (EndReason::PeerTimeout, 1510, Event::Tick),
            (
                EndReason::Disconnected,
                20,
                Event::PeerDisconnected { peer: mac.clone() },
            ),
            (EndReason::Superseded, 20, from_mac(enter_from_the_mac(3))),
            (EndReason::Stalled, 20, Event::LocalStall),
        ];
        for (reason, now, event) in endings {
            let (mut core, _) = the_pc_beside_the_mac_with(CoreConfig::default());
            core.handle(10, from_mac(enter_from_the_mac(1)));
            let out = core.handle(now, event);
            let ended = Action::Notify(Notice::SessionEnded {
                peer: mac.clone(),
                reason,
            });
            assert!(out.contains(&ended), "{reason:?}: {out:?}");
            assert!(!out.contains(&Action::ConcealCursor), "{reason:?}");
            let out = core.handle(now + 10, local(scroll, at));
            assert!(!out.contains(&Action::RevealCursor), "{reason:?}");
        }
    }

    // ----- nudges (docs/M4-M5-DESIGN.md, section 1) ----------------------

    /// A Mac with one 1000x1000 display, the PC both to its right and above
    /// it (one display each way), and "zed" to its left, all connected.
    /// Returns the core and the ids of the PC and zed.
    fn the_mac_between_two_peers() -> (Core, DeviceId, DeviceId) {
        let (mac, pc, zed) = (
            DeviceId::new("mac"),
            DeviceId::new("pc"),
            DeviceId::new("zed"),
        );
        let square = |id, x, y| screen(id, x, y, 1000.0, 1000.0);
        let layout = Layout::new([
            crate::DeviceLayout::new(mac.clone(), Point::new(0.0, 0.0), vec![square(1, 0.0, 0.0)]),
            crate::DeviceLayout::new(pc.clone(), Point::new(1000.0, 0.0), vec![]),
            crate::DeviceLayout::new(zed.clone(), Point::new(-1000.0, 0.0), vec![]),
        ])
        .unwrap();
        let mut core = Core::new(mac, Os::MacOs, CoreConfig::default(), layout);
        let peers = [
            (&pc, vec![square(1, 0.0, 0.0), square(2, -1000.0, -1000.0)]),
            (&zed, vec![square(1, 0.0, 0.0)]),
        ];
        for (peer, displays) in peers {
            core.handle(
                0,
                Event::PeerConnected {
                    peer: peer.clone(),
                    os: Os::Windows,
                    displays,
                },
            );
        }
        (core, pc, zed)
    }

    /// Physical motion by `(dx, dy)` from `(x, y)`, at `now`.
    fn move_by(core: &mut Core, now: Millis, (x, y): (f64, f64), dx: f64, dy: f64) -> Vec<Action> {
        core.handle(now, local(motion(dx, dy), Point::new(x, y)))
    }

    /// The part of an edge of the Mac's display that leads to a peer: all
    /// of it but the default corner dead zones.
    fn mac_edge(edge: Edge) -> SharedEdge {
        SharedEdge {
            display: 1,
            frame: Rect::new(0.0, 0.0, 1000.0, 1000.0),
            edge,
            start: 10.0,
            end: 990.0,
        }
    }

    #[test]
    fn each_peer_the_pointer_heads_for_is_nudged_at_most_every_interval() {
        let (mut core, pc, zed) = the_mac_between_two_peers();
        assert_eq!(
            core.edges_toward(&pc),
            [mac_edge(Edge::Right), mac_edge(Edge::Top)]
        );
        assert_eq!(core.edges_toward(&zed), [mac_edge(Edge::Left)]);
        let nudge = |to: &DeviceId| Action::Nudge { to: to.clone() };
        let core = &mut core;

        // Toward the right edge, ending 200 units from it.
        assert_eq!(move_by(core, 100, (500.0, 500.0), 300.0, 0.0), [nudge(&pc)]);
        // Not again for 200 ms.
        assert_eq!(move_by(core, 150, (800.0, 500.0), 100.0, 0.0), []);
        assert_eq!(move_by(core, 299, (900.0, 500.0), 10.0, 0.0), []);
        assert_eq!(move_by(core, 300, (910.0, 500.0), 10.0, 0.0), [nudge(&pc)]);
        // Away from the edge, along it, or toward it from too far: never.
        assert_eq!(move_by(core, 1000, (920.0, 500.0), -50.0, 0.0), []);
        assert_eq!(move_by(core, 1000, (870.0, 500.0), 0.0, 50.0), []);
        assert_eq!(move_by(core, 1000, (100.0, 500.0), 599.0, 0.0), []);
        // Level with a corner dead zone, beside no part that leads anywhere.
        assert_eq!(move_by(core, 1000, (900.0, 5.0), 10.0, 0.0), []);
        // Into the top-right corner, near both edges that lead to the PC:
        // one nudge.
        assert_eq!(
            move_by(core, 2000, (800.0, 200.0), 50.0, -50.0),
            [nudge(&pc)]
        );
        // Into the top-left corner, near an edge of each peer: one each, in
        // id order.
        assert_eq!(
            move_by(core, 3000, (200.0, 200.0), -50.0, -50.0),
            [nudge(&pc), nudge(&zed)]
        );
        // Each peer has its own interval.
        assert_eq!(move_by(core, 3100, (150.0, 150.0), -10.0, 0.0), []);
        assert_eq!(
            move_by(core, 3200, (140.0, 150.0), -10.0, 0.0),
            [nudge(&zed)]
        );
    }

    /// The edges are looked up again whenever a peer comes or goes or the
    /// layout changes.
    #[test]
    fn nudges_follow_the_peers_and_the_layout() {
        let (mut core, pc, zed) = the_mac_between_two_peers();
        // Gone: no more nudges for zed.
        assert_eq!(
            core.handle(10, Event::PeerDisconnected { peer: zed.clone() }),
            []
        );
        assert_eq!(move_by(&mut core, 20, (200.0, 500.0), -50.0, 0.0), []);
        // The PC reconnects with only the display on the right: the top edge
        // leads nowhere now.
        core.handle(
            30,
            Event::PeerConnected {
                peer: pc.clone(),
                os: Os::Windows,
                displays: vec![screen(1, 0.0, 0.0, 1000.0, 1000.0)],
            },
        );
        assert_eq!(core.edges_toward(&pc), [mac_edge(Edge::Right)]);
        assert_eq!(move_by(&mut core, 40, (500.0, 100.0), 0.0, -10.0), []);
        assert_eq!(
            move_by(&mut core, 50, (900.0, 500.0), 10.0, 0.0),
            [Action::Nudge { to: pc.clone() }]
        );
        // The agent moves the PC far away: nothing leads to it any more.
        let far = Layout::new([
            crate::DeviceLayout::new(
                DeviceId::new("mac"),
                Point::new(0.0, 0.0),
                vec![screen(1, 0.0, 0.0, 1000.0, 1000.0)],
            ),
            crate::DeviceLayout::new(pc.clone(), Point::new(5000.0, 5000.0), vec![]),
        ])
        .unwrap();
        core.handle(60, Event::LayoutChanged(far));
        assert_eq!(core.edges_toward(&pc), []);
        assert_eq!(move_by(&mut core, 500, (900.0, 500.0), 10.0, 0.0), []);
    }

    /// A cursor right on a left or top edge line is 0 units from it, but a
    /// nudge distance of 0 still turns nudges off.
    #[test]
    fn a_nudge_distance_of_0_turns_nudges_off() {
        let toward_the_mac = |config| {
            let (mut core, mac) = the_pc_beside_the_mac_with(config);
            let out = move_by(&mut core, 10, (5.0, 432.0), -5.0, 0.0);
            assert_eq!(core.state(), State::Idle, "still on the PC's screen");
            (out, mac)
        };
        let (out, mac) = toward_the_mac(CoreConfig::default());
        assert_eq!(out, [Action::Nudge { to: mac }]);
        let off = CoreConfig {
            nudge_distance: 0.0,
            ..CoreConfig::default()
        };
        assert_eq!(toward_the_mac(off).0, []);
    }
}
