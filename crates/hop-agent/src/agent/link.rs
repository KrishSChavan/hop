//! The agent's view of the network: what the core thread needs from
//! `hop-net`, behind a small trait so the core loop can be tested with a
//! fake network.

use std::path::PathBuf;

use hop_net::{NetEvent, NetEvents, NetHandle, SendError};
use hop_types::{CoreMessage, DeviceId, DisplayInfo};

/// What the core thread does with the network. Every method returns at
/// once: the core thread must never wait for the network.
pub(crate) trait Link {
    /// Sends `msg` to `to`. A reliable message is never dropped: when it
    /// cannot be queued the connection closes and `PeerDown` follows, so
    /// errors only need logging.
    fn send(&self, to: &DeviceId, msg: CoreMessage) -> Result<(), SendError>;
    /// Sends the files at `paths` to `to` and returns the transfer's id at
    /// once; the outcome arrives later as `NetEvent::Transfer`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the agent sends dropped files from work package T5 on"
        )
    )]
    fn send_files(&self, to: &DeviceId, paths: Vec<PathBuf>) -> Result<u64, SendError>;
    /// Wakes `to`'s Wi-Fi radio with a tiny datagram, best effort.
    fn nudge(&self, to: &DeviceId);
    /// The next network event, if one is waiting.
    fn try_recv(&self) -> Option<NetEvent>;
    /// The displays to announce on connections made from now on.
    fn set_displays(&self, displays: Vec<DisplayInfo>);
    /// Whether macOS turned out to be blocking Hop from the local network.
    fn local_network_denied(&self) -> bool;
}

/// The real network: `hop-net`'s handle and event queue.
pub(crate) struct NetLink {
    handle: NetHandle,
    events: NetEvents,
}

impl NetLink {
    /// Wraps the two halves `hop_net::start` returned.
    pub(crate) fn new(handle: NetHandle, events: NetEvents) -> NetLink {
        NetLink { handle, events }
    }
}

impl Link for NetLink {
    fn send(&self, to: &DeviceId, msg: CoreMessage) -> Result<(), SendError> {
        self.handle.send(to, msg)
    }

    fn send_files(&self, to: &DeviceId, paths: Vec<PathBuf>) -> Result<u64, SendError> {
        self.handle.send_files(to, paths)
    }

    fn nudge(&self, to: &DeviceId) {
        self.handle.nudge(to);
    }

    fn try_recv(&self) -> Option<NetEvent> {
        self.events.try_recv()
    }

    fn set_displays(&self, displays: Vec<DisplayInfo>) {
        self.handle.set_displays(displays);
    }

    fn local_network_denied(&self) -> bool {
        self.handle.local_network_denied()
    }
}
