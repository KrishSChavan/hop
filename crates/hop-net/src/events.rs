//! The queue of network events the agent's core thread reads.
//!
//! The queue is also the single record of what the core has been told:
//! for each peer it remembers which connection was announced last, and it
//! accepts messages only from that connection. That is how "nothing from a
//! replaced connection reaches the core" holds without any timing
//! assumption (docs/M2-DESIGN.md, "What the core sees").

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hop_proto::Negotiated;
use hop_types::{CoreMessage, DeviceId, DisplayInfo, Os};

use crate::CloseCode;

/// Something the network layer tells the agent. Feed them to the core in
/// the order they arrive:
///
/// | Event | Core event |
/// |---|---|
/// | [`NetEvent::PeerUp`] | `Event::PeerConnected { peer, os, displays }` |
/// | [`NetEvent::PeerDown`] | `Event::PeerDisconnected { peer }` |
/// | [`NetEvent::Message`] | `Event::PeerMessage { from, msg }` |
///
/// For each peer the events form clean runs: `PeerUp`, its messages, then
/// either `PeerDown` or another `PeerUp`. A second `PeerUp` without a
/// `PeerDown` between means the connection was replaced (a duplicate was
/// resolved); the core treats it as "the old connection is gone". Nothing
/// from a replaced or closed connection follows its replacement or its
/// `PeerDown`.
#[derive(Clone, Debug, PartialEq)]
pub enum NetEvent {
    /// A paired peer finished its `Hello` exchange on a connection that is
    /// now its current one.
    PeerUp(PeerUp),
    /// The peer's current connection closed. Any session with it must end.
    PeerDown {
        /// The peer.
        peer: DeviceId,
        /// Why, for the log.
        reason: DownReason,
    },
    /// A message from a peer's current connection. `ClipboardText` never
    /// appears in M2: it is not negotiated, and the network drops it.
    Message {
        /// The sender.
        from: DeviceId,
        /// The message.
        msg: CoreMessage,
    },
}

/// A peer that just connected.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerUp {
    /// The peer's id, as its certificate gives it (and its `Hello` had to
    /// match it).
    pub peer: DeviceId,
    /// The name the peer calls itself, cleaned for display: no control
    /// characters, at most 64 characters.
    pub name: String,
    /// The peer's Hop version, cleaned the same way.
    pub app_version: String,
    /// The peer's operating system.
    pub os: Os,
    /// The peer's displays, in its own coordinates, as its `Hello` gave
    /// them. The core validates them.
    pub displays: Vec<DisplayInfo>,
    /// The protocol version and capabilities both sides support.
    pub negotiated: Negotiated,
    /// The peer's address on this connection.
    pub remote: SocketAddr,
    /// Whether this device dialled the connection (rather than accepting
    /// it).
    pub dialled_by_us: bool,
}

/// Why a peer's connection went down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownReason {
    /// This device closed it, with this code.
    Local(CloseCode),
    /// The peer closed it, with an application code and its reason text
    /// (cleaned for display).
    Remote {
        /// The peer's close code; see [`CloseCode::from_code`].
        code: u64,
        /// The peer's reason, cleaned for display.
        reason: String,
    },
    /// Nothing was heard from the peer within the idle timeout.
    TimedOut,
    /// The connection failed below Hop (a transport error or a reset).
    Transport(String),
}

impl fmt::Display for DownReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownReason::Local(code) => write!(f, "closed here: {code}"),
            DownReason::Remote { code, reason } => {
                match CloseCode::from_code(*code) {
                    Some(known) => write!(f, "closed by the peer: {known}")?,
                    None => write!(f, "closed by the peer: code {code}")?,
                }
                if !reason.is_empty() {
                    write!(f, " ({reason})")?;
                }
                Ok(())
            }
            DownReason::TimedOut => f.write_str("timed out"),
            DownReason::Transport(detail) => write!(f, "connection failed: {detail}"),
        }
    }
}

/// What happened to a message offered to the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    /// Queued for the core.
    Queued,
    /// Not from the peer's current connection: dropped. The connection
    /// was replaced or closed, so its reader should stop.
    NotCurrent,
    /// The queue is full. A reliable message must not be dropped, so the
    /// connection must close with `OVERLOADED`.
    Full,
    /// The agent stopped listening (shutdown, or it dropped
    /// [`NetEvents`]).
    Closed,
}

/// The receiving end of the network events, for the agent's core thread.
///
/// All methods are cheap and safe to call from any thread; they take a
/// lock that the network layer holds only for a few instructions. To wait
/// on this queue and the input queue at once, give
/// [`NetConfig::on_event`](crate::NetConfig::on_event) a callback that wakes
/// the core thread, and call [`NetEvents::try_recv`] after each wake-up.
///
/// Dropping it tells the network layer that nobody is listening: new
/// connections are then refused and messages are dropped.
pub struct NetEvents {
    queue: Arc<EventQueue>,
}

impl NetEvents {
    pub(crate) fn new(queue: Arc<EventQueue>) -> NetEvents {
        NetEvents { queue }
    }

    /// The next event, if one is waiting.
    pub fn try_recv(&self) -> Option<NetEvent> {
        self.queue.lock().pop()
    }

    /// The next event, waiting up to `timeout` for one. `None` when the
    /// time ran out or the network layer shut down.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<NetEvent> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.queue.lock();
        loop {
            if let Some(event) = state.pop() {
                return Some(event);
            }
            if state.closed {
                return None;
            }
            let wait = match deadline {
                Some(deadline) => deadline.checked_duration_since(Instant::now())?,
                // A timeout too large for the clock: wait in long steps.
                None => Duration::from_secs(3600),
            };
            if wait.is_zero() {
                return None;
            }
            state = self
                .queue
                .ready
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// How many events are waiting.
    pub fn len(&self) -> usize {
        self.queue.lock().events.len()
    }

    /// Whether no events are waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for NetEvents {
    fn drop(&mut self) {
        self.queue.close();
    }
}

impl fmt::Debug for NetEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetEvents")
            .field("waiting", &self.len())
            .finish()
    }
}

/// The callback the agent gives to be told that events are waiting.
pub(crate) type Notify = Arc<dyn Fn() + Send + Sync>;

/// The shared queue behind [`NetEvents`].
pub(crate) struct EventQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    notify: Option<Notify>,
}

struct QueueState {
    events: VecDeque<NetEvent>,
    /// `Message` events waiting; at most `capacity`.
    messages: usize,
    /// `PeerUp` and `PeerDown` events waiting. A connection is announced
    /// only while fewer than `max_states` are waiting, and each announced
    /// connection adds at most one `PeerDown`, so these stay bounded too.
    states: usize,
    capacity: usize,
    max_states: usize,
    /// The connection last announced for each peer, while the core believes
    /// it is up.
    current: HashMap<DeviceId, u64>,
    closed: bool,
}

impl QueueState {
    fn pop(&mut self) -> Option<NetEvent> {
        let event = self.events.pop_front()?;
        match event {
            NetEvent::Message { .. } => self.messages = self.messages.saturating_sub(1),
            NetEvent::PeerUp(_) | NetEvent::PeerDown { .. } => {
                self.states = self.states.saturating_sub(1);
            }
        }
        Some(event)
    }
}

impl EventQueue {
    /// A queue holding at most `capacity` messages and `max_states`
    /// connection events. `notify` runs after events are queued.
    pub(crate) fn new(capacity: usize, max_states: usize, notify: Option<Notify>) -> EventQueue {
        EventQueue {
            state: Mutex::new(QueueState {
                events: VecDeque::new(),
                messages: 0,
                states: 0,
                capacity: capacity.max(1),
                max_states: max_states.max(1),
                current: HashMap::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            notify,
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // A panic is fatal for the whole agent, so a poisoned lock only
        // happens while the process is going down; keep going with the data.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Announces connection `conn` as `up.peer`'s current one. Returns
    /// false (and changes nothing) if too many connection events are
    /// waiting or nobody listens; the caller then closes the connection.
    pub(crate) fn announce(&self, conn: u64, up: PeerUp) -> bool {
        let mut state = self.lock();
        if state.closed || state.states >= state.max_states {
            return false;
        }
        state.current.insert(up.peer.clone(), conn);
        state.states += 1;
        state.events.push_back(NetEvent::PeerUp(up));
        true
    }

    /// Queues a message from connection `conn`. A reliable message that
    /// does not fit returns [`Offer::Full`]; an unreliable one (`Motion`)
    /// that does not fit is dropped and also returns `Full`, which the
    /// caller ignores for motion.
    pub(crate) fn offer(&self, from: &DeviceId, conn: u64, msg: CoreMessage) -> Offer {
        let mut state = self.lock();
        if state.closed {
            return Offer::Closed;
        }
        if state.current.get(from) != Some(&conn) {
            return Offer::NotCurrent;
        }
        if state.messages >= state.capacity {
            return Offer::Full;
        }
        state.messages += 1;
        state.events.push_back(NetEvent::Message {
            from: from.clone(),
            msg,
        });
        Offer::Queued
    }

    /// Reports that connection `conn` to `peer` closed. Queues `PeerDown`
    /// only if `conn` is the connection the core was last told about, and
    /// returns whether it did.
    pub(crate) fn disconnect(&self, peer: &DeviceId, conn: u64, reason: DownReason) -> bool {
        let mut state = self.lock();
        if state.closed || state.current.get(peer) != Some(&conn) {
            return false;
        }
        state.current.remove(peer);
        // Never refused: the core must always hear that a peer it believes
        // connected is gone. The count stays bounded (see `states`).
        state.states += 1;
        state.events.push_back(NetEvent::PeerDown {
            peer: peer.clone(),
            reason,
        });
        true
    }

    /// Stops accepting events (shutdown, or the agent dropped
    /// [`NetEvents`]) and wakes any waiting reader.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    /// Wakes the agent after events were queued. Called with no Hop lock
    /// held, so the agent's callback may take its own locks freely.
    pub(crate) fn wake(&self) {
        self.ready.notify_all();
        if let Some(notify) = &self.notify {
            notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hop_proto::PROTO_MAJOR;

    use super::*;

    fn up(peer: &str) -> PeerUp {
        PeerUp {
            peer: DeviceId::new(peer),
            name: "peer".to_owned(),
            app_version: "0.1.0".to_owned(),
            os: Os::Windows,
            displays: Vec::new(),
            negotiated: Negotiated {
                proto_major: PROTO_MAJOR,
                proto_minor: 0,
                capabilities: Vec::new(),
            },
            remote: SocketAddr::from(([127, 0, 0, 1], 1)),
            dialled_by_us: true,
        }
    }

    fn heartbeat(seq: u64) -> CoreMessage {
        CoreMessage::Heartbeat { seq }
    }

    #[test]
    fn only_the_current_connection_is_heard() {
        let queue = Arc::new(EventQueue::new(16, 16, None));
        let events = NetEvents::new(queue.clone());
        let peer = DeviceId::new("p");
        assert_eq!(queue.offer(&peer, 1, heartbeat(0)), Offer::NotCurrent);
        assert!(queue.announce(1, up("p")));
        assert_eq!(queue.offer(&peer, 1, heartbeat(1)), Offer::Queued);
        // Connection 2 replaces 1: from then on 1 is not heard, and its
        // close is not reported.
        assert!(queue.announce(2, up("p")));
        assert_eq!(queue.offer(&peer, 1, heartbeat(2)), Offer::NotCurrent);
        assert!(!queue.disconnect(&peer, 1, DownReason::TimedOut));
        assert_eq!(queue.offer(&peer, 2, heartbeat(3)), Offer::Queued);
        assert!(queue.disconnect(&peer, 2, DownReason::TimedOut));
        assert_eq!(queue.offer(&peer, 2, heartbeat(4)), Offer::NotCurrent);

        let kinds: Vec<String> = std::iter::from_fn(|| events.try_recv())
            .map(|event| match event {
                NetEvent::PeerUp(_) => "up".to_owned(),
                NetEvent::PeerDown { .. } => "down".to_owned(),
                NetEvent::Message { msg, .. } => format!("{:?}", msg.seq()),
            })
            .collect();
        assert_eq!(kinds, ["up", "Some(1)", "up", "Some(3)", "down"]);
    }

    #[test]
    fn a_full_queue_refuses_messages_but_never_peer_down() {
        let queue = Arc::new(EventQueue::new(2, 1, None));
        let events = NetEvents::new(queue.clone());
        let peer = DeviceId::new("p");
        assert!(queue.announce(1, up("p")));
        assert!(
            !queue.announce(2, up("q")),
            "too many connection events waiting"
        );
        assert_eq!(queue.offer(&peer, 1, heartbeat(1)), Offer::Queued);
        assert_eq!(queue.offer(&peer, 1, heartbeat(2)), Offer::Queued);
        assert_eq!(queue.offer(&peer, 1, heartbeat(3)), Offer::Full);
        assert!(queue.disconnect(&peer, 1, DownReason::Local(CloseCode::Overloaded)));
        assert_eq!(events.len(), 4);
        // The PeerUp and PeerDown both count against the limit until read.
        assert!(matches!(events.try_recv(), Some(NetEvent::PeerUp(_))));
        assert!(!queue.announce(3, up("q")));
        while events.try_recv().is_some() {}
        assert!(queue.announce(3, up("q")));
    }

    #[test]
    fn recv_timeout_waits_and_wakes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let queue = Arc::new(EventQueue::new(
            4,
            4,
            Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            })),
        ));
        let events = NetEvents::new(queue.clone());
        assert_eq!(events.recv_timeout(Duration::from_millis(20)), None);
        let producer = std::thread::spawn({
            let queue = queue.clone();
            move || {
                std::thread::sleep(Duration::from_millis(30));
                assert!(queue.announce(1, up("p")));
                queue.wake();
            }
        });
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(10)),
            Some(NetEvent::PeerUp(_))
        ));
        producer.join().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        queue.close();
        assert_eq!(events.recv_timeout(Duration::from_secs(10)), None);
    }

    #[test]
    fn dropping_the_receiver_closes_the_queue() {
        let queue = Arc::new(EventQueue::new(4, 4, None));
        drop(NetEvents::new(queue.clone()));
        assert!(!queue.announce(1, up("p")));
        assert_eq!(
            queue.offer(&DeviceId::new("p"), 1, heartbeat(1)),
            Offer::Closed
        );
    }
}
