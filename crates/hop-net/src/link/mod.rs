//! The session link: one QUIC endpoint that listens for and dials paired
//! peers, keeps one connection per peer, and carries `Hello`,
//! `CoreMessage`s, nudges and file transfers (docs/M2-DESIGN.md,
//! "Transport"; docs/M4-M5-DESIGN.md).

mod conn;
mod dial;
mod files;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use hop_proto::encode_frame;
use hop_proto::{
    Capability, Hello, KIND_NUDGE, Message, Negotiated, ProtoError, caps, encode_datagram,
};
use hop_types::{CoreMessage, DeviceId, DisplayInfo, Os};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::Instant;

use crate::discovery::Discovery;
use crate::events::{EventQueue, NetEvents, Notify};
use crate::sanitize::{MAX_DISPLAY_CHARS, display_text};
use crate::tls::{self, Accept, PinSet, VerifierStats};
use crate::transport::TransportParams;
use crate::{CloseCode, Fingerprint, Identity, ratelimit};

pub(crate) use conn::Link;
use files::{ConnFiles, Inbox};

/// A nudge: the kind alone (`Message::Nudge` as a datagram).
static NUDGE: [u8; 2] = KIND_NUDGE.to_be_bytes();

/// A paired peer the link may talk to, as pairing saved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedPeer {
    /// The peer's id. It must equal the first 32 characters of
    /// `fingerprint`; a peer where they differ is skipped with a warning.
    pub id: DeviceId,
    /// The peer's pinned fingerprint. Only a device holding this exact key
    /// can connect as this peer.
    pub fingerprint: Fingerprint,
    /// The peer's name, for logs.
    pub name: String,
    /// A fixed address to dial (an IPv4 address or host name, with an
    /// optional `:port`; the default port is this device's own). It is
    /// always tried, in addition to what mDNS finds, which also works on
    /// networks that block multicast.
    pub address: Option<String>,
}

/// Timeouts and delays of the link. The defaults are the design's values;
/// tests shorten them.
#[derive(Clone, Debug)]
pub struct Timing {
    /// A TLS handshake must finish within this (5 s).
    pub handshake_timeout: Duration,
    /// After the handshake, the control stream must open and the peer's
    /// `Hello` arrive within this (3 s).
    pub hello_timeout: Duration,
    /// QUIC keep-alive interval (1 s).
    pub keep_alive: Duration,
    /// Silence after which a connection is dead (5 s).
    pub idle_timeout: Duration,
    /// How long the device with the higher id waits before dialling a peer
    /// it is not connected to (1 s). The lower id dials at once.
    pub higher_id_delay: Duration,
    /// First reconnect delay (250 ms).
    pub backoff_min: Duration,
    /// Longest reconnect delay (5 s).
    pub backoff_max: Duration,
    /// A connection must stay up this long before the backoff starts again
    /// from `backoff_min` (10 s), so a peer that accepts and drops at once
    /// cannot cause a dial storm.
    pub stable_after: Duration,
    /// Retry interval after the peer turned out to be incompatible (60 s).
    pub incompatible_retry: Duration,
    /// How long a listener holds an incoming connection while its own dial
    /// to the same peer, which would win the duplicate rule, is still in
    /// progress (1 s). This keeps the core from hearing about a connection
    /// that is about to be replaced.
    pub duplicate_grace: Duration,
    /// [`NetHandle::shutdown`] waits at most this long for queued messages
    /// to be delivered (500 ms).
    pub shutdown_wait: Duration,
    /// Host name lookups for configured addresses give up after this (2 s).
    pub resolve_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            handshake_timeout: Duration::from_secs(5),
            hello_timeout: Duration::from_secs(3),
            keep_alive: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(5),
            higher_id_delay: Duration::from_secs(1),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(5),
            stable_after: Duration::from_secs(10),
            incompatible_retry: Duration::from_secs(60),
            duplicate_grace: Duration::from_secs(1),
            shutdown_wait: Duration::from_millis(500),
            resolve_timeout: Duration::from_secs(2),
        }
    }
}

/// Bounds on everything a peer or a stranger can make the link hold.
#[derive(Clone, Debug)]
pub struct Limits {
    /// TLS handshakes in progress at once; more are refused (4).
    pub max_handshakes: usize,
    /// Connection attempts quinn queues before Hop sees them (16).
    pub max_incoming: usize,
    /// Messages waiting for the core (1024). When full, a reliable message
    /// closes its connection with `OVERLOADED`; a `Motion` is dropped.
    pub event_capacity: usize,
    /// Connection events (`PeerUp`, `PeerDown`) waiting for the core (32).
    /// While this many wait, new connections are refused.
    pub max_pending_connection_events: usize,
    /// Frames waiting to be written on one connection (256). When full,
    /// [`NetHandle::send`] closes the connection with `OVERLOADED` rather
    /// than drop a message.
    pub send_capacity: usize,
    /// The most files one received transfer may hold (1,000, the protocol's
    /// own limit). An offer of more is refused.
    pub max_transfer_files: usize,
    /// The largest file this device receives, in bytes (4 GiB).
    pub max_file_bytes: u64,
    /// The most bytes one received transfer may hold in all (16 GiB).
    pub max_transfer_bytes: u64,
    /// Received transfers that may wait while another one runs (4); an
    /// offer beyond those is refused as busy.
    pub max_waiting_transfers: usize,
    /// How long a sender waits for the answer to its offer before it fails
    /// with [`TransferFailure::NoAnswer`](crate::TransferFailure::NoAnswer)
    /// (2 minutes). Longer than the wait for progress: the receiver
    /// answers once its Received folder is ready, and on a Mac the first
    /// use of the Downloads folder waits until the user allows it.
    pub answer_timeout: Duration,
    /// How long a transfer may go without progress, and how long a sender
    /// waits for the outcome once the data is sent, before it fails (30 s).
    /// A receiver gives up on an accepted offer whose data does not start
    /// within this, too.
    pub transfer_timeout: Duration,
    /// The fastest this device sends file data on one connection, in bytes
    /// per second (12 MiB/s; zero means no limit), so a large transfer
    /// does not fill the Wi-Fi queue in front of the input.
    pub file_rate: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_handshakes: 4,
            max_incoming: 16,
            event_capacity: 1024,
            max_pending_connection_events: 32,
            send_capacity: 256,
            max_transfer_files: hop_proto::MAX_OFFER_FILES,
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            max_transfer_bytes: 16 * 1024 * 1024 * 1024,
            max_waiting_transfers: 4,
            answer_timeout: Duration::from_secs(120),
            transfer_timeout: Duration::from_secs(30),
            file_rate: 12 * 1024 * 1024,
        }
    }
}

/// Everything [`start`] needs besides the identity.
#[derive(Clone)]
pub struct NetConfig {
    /// Where to listen and dial from. In production `0.0.0.0` and
    /// `network.port` (M2 is IPv4 only); tests use `127.0.0.1:0`.
    pub bind: SocketAddr,
    /// This device's name, sent to peers (cleaned and cut to 64
    /// characters).
    pub device_name: String,
    /// This build's version, sent to peers.
    pub app_version: String,
    /// The OS this device announces. [`NetConfig::new`] sets the real one;
    /// tests of the Cmd/Ctrl swap on one machine change it.
    pub os: Os,
    /// This device's displays, sent in `Hello`; see
    /// [`NetHandle::set_displays`].
    pub displays: Vec<DisplayInfo>,
    /// The paired peers.
    pub peers: Vec<PinnedPeer>,
    /// Advertise the `clipboard.text` capability, and send and accept
    /// `ClipboardText` on connections where both sides did (milestone M4).
    /// [`NetConfig::new`] leaves it off; the agent sets it from
    /// `clipboard.enabled`.
    pub clipboard: bool,
    /// Receive files into this folder and advertise the `files` capability
    /// (milestone M5). `None`, which [`NetConfig::new`] sets, means off;
    /// since both sides must advertise `files`, no files then travel either
    /// way. The folder is not touched until the first offer arrives.
    pub files: Option<FilesConfig>,
    /// Advertise this device and look for peers with mDNS. Tests turn it
    /// off: multicast would trigger macOS's Local Network prompt.
    pub discovery: bool,
    /// Timeouts and delays.
    pub timing: Timing,
    /// Queue and handshake bounds.
    pub limits: Limits,
    /// Called on a network thread after events are queued, so the agent
    /// can wake its core thread (which also waits for local input). It
    /// must return quickly and must not call into this crate.
    pub on_event: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl NetConfig {
    /// A config listening on `0.0.0.0:port`, with discovery on, no peers,
    /// no displays, clipboard and files off, and the default timing and
    /// limits.
    pub fn new(port: u16, device_name: impl Into<String>, app_version: impl Into<String>) -> Self {
        NetConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], port)),
            device_name: device_name.into(),
            app_version: app_version.into(),
            os: Os::current(),
            displays: Vec::new(),
            peers: Vec::new(),
            clipboard: false,
            files: None,
            discovery: true,
            timing: Timing::default(),
            limits: Limits::default(),
            on_event: None,
        }
    }
}

impl fmt::Debug for NetConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetConfig")
            .field("bind", &self.bind)
            .field("device_name", &self.device_name)
            .field("app_version", &self.app_version)
            .field("os", &self.os)
            .field("displays", &self.displays)
            .field("peers", &self.peers)
            .field("clipboard", &self.clipboard)
            .field("files", &self.files)
            .field("discovery", &self.discovery)
            .field("timing", &self.timing)
            .field("limits", &self.limits)
            .field("on_event", &self.on_event.is_some())
            .finish()
    }
}

/// Receiving files, for [`NetConfig::files`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesConfig {
    /// The folder received files are saved in: `transfer.received_dir`, or
    /// Downloads › Hop Received. Nothing reads or creates it before the
    /// first offer arrives (on the Mac, the first access inside Downloads
    /// asks the user once); it is created then, if needed, and swept of
    /// the temporary files an earlier run left.
    pub received_dir: PathBuf,
}

/// Why the link could not start.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The port could not be bound, usually because another copy of Hop
    /// is running. The agent binds before it starts capture, so a second
    /// copy fails here, before it touches input.
    #[error("could not listen on {addr} (is Hop already running?)")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// M2 is IPv4 only: on Windows a `[::]` socket is IPv6-only.
    #[error("{0} is not an IPv4 address; Hop uses IPv4 only for now")]
    NotIpv4(SocketAddr),
    /// The TLS or QUIC configuration could not be built.
    #[error("could not set up encryption: {0}")]
    Setup(String),
}

/// Why [`NetHandle::send`] did not send.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The peer has no current connection. The core hears (or has heard)
    /// `PeerDown`.
    #[error("the peer is not connected")]
    NotConnected,
    /// The connection's send queue was full, so it was closed with
    /// `OVERLOADED` rather than drop a reliable message. `PeerDown`
    /// follows, and both sides fail open.
    #[error("the connection was overloaded and has been closed")]
    Overloaded,
    /// The message needs a capability the two devices did not negotiate:
    /// `ClipboardText` needs `clipboard.text`, and files need `files`.
    #[error("the peer did not negotiate this kind of message")]
    NotNegotiated,
    /// The message could not be encoded (for example, clipboard text over
    /// the protocol limit).
    #[error("the message could not be encoded")]
    Encode(#[source] ProtoError),
}

/// The capabilities this device advertises: `input` and `keymap` always,
/// `clipboard.text` when [`NetConfig::clipboard`] is on, and `files` when
/// [`NetConfig::files`] is set. A capability is used only when both sides
/// advertise it, so turning one off here keeps it off in both directions.
pub(crate) fn implemented_capabilities(clipboard: bool, files: bool) -> Vec<Capability> {
    let mut capabilities = vec![
        Capability::new(caps::INPUT, 1),
        Capability::new(caps::KEYMAP, 1),
    ];
    if clipboard {
        capabilities.push(Capability::new(caps::CLIPBOARD_TEXT, 1));
    }
    if files {
        capabilities.push(Capability::new(caps::FILES, 1));
    }
    capabilities
}

/// Starts the link: binds the port, then listens for and dials the pinned
/// peers until shut down.
///
/// Must be called inside a tokio runtime (the agent's two-worker runtime);
/// the link's tasks run on it. Returns the [`NetHandle`] for sending and the
/// [`NetEvents`] the core reads.
///
/// Peers whose id does not match their fingerprint, peers pinned twice and
/// a peer with this device's own key are skipped with a warning.
///
/// # Errors
///
/// [`NetError::Bind`] if the port is taken (another copy of Hop),
/// [`NetError::NotIpv4`] for an IPv6 bind address, and
/// [`NetError::Setup`] if the TLS configuration cannot be built.
pub async fn start(
    identity: Arc<Identity>,
    config: NetConfig,
) -> Result<(NetHandle, NetEvents), NetError> {
    if !config.bind.is_ipv4() {
        return Err(NetError::NotIpv4(config.bind));
    }
    let me = identity.device_id();
    let peers = usable_peers(&identity, config.peers);
    let pins = Arc::new(PinSet::new(
        peers.iter().map(|peer| (peer.fingerprint, peer.id.clone())),
    ));
    let stats = Arc::new(VerifierStats::default());
    let mut timing = config.timing;
    // The dialler waits only `hello_timeout` for the listener's Hello, so
    // the listener must stop holding a connection well before that.
    timing.duplicate_grace = timing.duplicate_grace.min(timing.hello_timeout / 2);
    let limits = config.limits;
    let inbox = config
        .files
        .map(|files| Arc::new(Inbox::new(files.received_dir, &limits)));

    let setup = |err: String| NetError::Setup(err);
    let server_transport = Arc::new(session_transport(&timing, 1).build().map_err(setup)?);
    // The listener opens no streams, so the dialler allows none.
    let client_transport = Arc::new(session_transport(&timing, 0).build().map_err(setup)?);
    let server_tls = tls::server_config(
        &identity,
        Accept::Pinned(pins.clone()),
        tls::ALPN_SESSION,
        stats.clone(),
    )
    .map_err(|err| setup(err.to_string()))?;
    let server = tls::quic_server(server_tls, server_transport, limits.max_incoming)
        .map_err(|err| setup(err.to_string()))?;

    // Bind first: this is the single-instance lock.
    let socket = std::net::UdpSocket::bind(config.bind).map_err(|source| NetError::Bind {
        addr: config.bind,
        source,
    })?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server),
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .map_err(|source| NetError::Bind {
        addr: config.bind,
        source,
    })?;
    let local_addr = endpoint.local_addr().map_err(|source| NetError::Bind {
        addr: config.bind,
        source,
    })?;

    let notify: Option<Notify> = config.on_event;
    let queue = Arc::new(EventQueue::new(
        limits.event_capacity,
        limits.max_pending_connection_events,
        notify,
    ));
    let (changes, _) = watch::channel(0u64);
    let (stopping, _) = watch::channel(false);
    let (alive_tx, alive_rx) = mpsc::channel(1);

    let slots = peers
        .iter()
        .map(|peer| (peer.id.clone(), Slot::new(peer.clone())))
        .collect();
    let shared = Arc::new(Shared {
        me: me.clone(),
        identity,
        endpoint,
        local_addr,
        pins,
        table: Mutex::new(Table {
            closed: false,
            peers: slots,
        }),
        changes,
        queue: queue.clone(),
        local: Mutex::new(Local {
            name: display_text(&config.device_name, MAX_DISPLAY_CHARS),
            app_version: display_text(&config.app_version, MAX_DISPLAY_CHARS),
            os: config.os,
            displays: config.displays,
        }),
        timing,
        limits: limits.clone(),
        clipboard: config.clipboard,
        inbox,
        next_conn: AtomicU64::new(1),
        next_transfer: AtomicU64::new(1),
        handshakes: Arc::new(Semaphore::new(limits.max_handshakes.max(1))),
        stopping,
        stats,
        client_transport,
        discovery: Mutex::new(None),
        alive: Mutex::new(Some(alive_tx)),
        alive_rx: tokio::sync::Mutex::new(alive_rx),
        local_network_denied: AtomicBool::new(false),
    });

    tokio::spawn(accept_loop(shared.clone()));
    for peer in &peers {
        tokio::spawn(dial::run_dialer(shared.clone(), peer.id.clone()));
    }
    if config.discovery {
        start_discovery(&shared, &config.device_name);
    }
    tracing::info!(
        id = %me,
        address = %local_addr,
        peers = peers.len(),
        "Hop link listening"
    );
    let handle = NetHandle {
        inner: Arc::new(HandleInner { shared }),
    };
    Ok((handle, NetEvents::new(queue)))
}

/// The pinned peers that can be used, with a warning for each one that
/// cannot.
fn usable_peers(identity: &Identity, peers: Vec<PinnedPeer>) -> Vec<PinnedPeer> {
    let mut usable: Vec<PinnedPeer> = Vec::new();
    for peer in peers {
        let name = display_text(&peer.name, MAX_DISPLAY_CHARS);
        if !peer.fingerprint.matches_id(&peer.id) {
            tracing::warn!(peer = %name, id = %peer.id, "skipping a paired device whose id does not match its fingerprint; pair it again");
        } else if peer.fingerprint == identity.fingerprint() {
            tracing::warn!(peer = %name, "skipping a paired device that has this device's own key");
        } else if usable
            .iter()
            .any(|known| known.fingerprint == peer.fingerprint || known.id == peer.id)
        {
            tracing::warn!(peer = %name, id = %peer.id, "skipping a device that is paired twice");
        } else {
            usable.push(peer);
        }
    }
    usable
}

/// The transport limits of a session connection.
fn session_transport(timing: &Timing, peer_bidi_streams: u32) -> TransportParams {
    TransportParams {
        peer_bidi_streams,
        stream_window: 2 * 1024 * 1024,
        connection_window: 4 * 1024 * 1024,
        send_window: 4 * 1024 * 1024,
        datagram_receive: Some(64 * 1024),
        datagram_send: 8 * 1024,
        keep_alive: timing.keep_alive,
        idle_timeout: timing.idle_timeout,
    }
}

/// The sending half of the link, for the agent. Cheap to clone; every
/// clone talks to the same link.
///
/// When the last clone is dropped without [`NetHandle::shutdown`], every
/// connection is closed at once with `SHUTDOWN`.
#[derive(Clone)]
pub struct NetHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    shared: Arc<Shared>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        self.shared.abort();
    }
}

impl fmt::Debug for NetHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetHandle")
            .field("id", &self.inner.shared.me)
            .field("address", &self.inner.shared.local_addr)
            .finish()
    }
}

impl NetHandle {
    /// Sends `msg` to `to` without blocking; safe to call from any thread,
    /// including the core thread, which carries out `Action::Send` with it.
    ///
    /// `Motion` goes as a QUIC datagram (unreliable, latest wins) when the
    /// peer supports datagrams and the message fits the connection's
    /// datagram size; otherwise, and for every other message, it goes on
    /// the control stream, reliable and in order. A reliable message is
    /// never dropped: if the connection's send queue is full, the
    /// connection is closed with `OVERLOADED` and this returns
    /// [`SendError::Overloaded`].
    ///
    /// # Errors
    ///
    /// See [`SendError`]. None of them needs handling beyond logging: the
    /// core learns about lost connections from `PeerDown`.
    pub fn send(&self, to: &DeviceId, msg: CoreMessage) -> Result<(), SendError> {
        let shared = &self.inner.shared;
        let is_clipboard = matches!(msg, CoreMessage::ClipboardText { .. });
        let message = Message::Core(msg);
        let datagram = match &message {
            Message::Core(CoreMessage::Motion { .. }) => encode_datagram(&message).ok(),
            _ => None,
        };
        let frame = Bytes::from(encode_frame(&message).map_err(SendError::Encode)?);

        let table = shared.lock_table();
        let current = table
            .peers
            .get(to)
            .and_then(|slot| slot.current.as_ref())
            .ok_or(SendError::NotConnected)?;
        if is_clipboard
            && current
                .negotiated
                .capability_version(caps::CLIPBOARD_TEXT)
                .is_none()
        {
            return Err(SendError::NotNegotiated);
        }
        if let Some(datagram) = datagram {
            let conn = &current.link.conn;
            if conn
                .max_datagram_size()
                .is_some_and(|max| datagram.len() <= max)
            {
                match conn.send_datagram(Bytes::from(datagram)) {
                    Ok(()) => return Ok(()),
                    Err(quinn::SendDatagramError::ConnectionLost(_)) => {
                        return Err(SendError::NotConnected);
                    }
                    // Too large after all, or turned off: use the stream.
                    Err(_) => {}
                }
            }
        }
        match current.outgoing.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                current
                    .link
                    .close(CloseCode::Overloaded, "the send queue is full");
                Err(SendError::Overloaded)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::NotConnected),
        }
    }

    /// Sends the files at `paths` to `to` and returns the transfer's id at
    /// once, without blocking; safe to call from any thread. The outcome
    /// arrives later as a [`NetEvent::Transfer`](crate::NetEvent::Transfer)
    /// with that id: [`TransferEvent::Sent`](crate::TransferEvent::Sent) or
    /// [`TransferEvent::Failed`](crate::TransferEvent::Failed).
    ///
    /// Only regular files are sent, read on a thread of their own; folders
    /// and files that cannot be read are skipped and counted. The files are
    /// copied: nothing here moves or changes them. Transfers to one peer go
    /// one after the other, each at most [`Limits::file_rate`].
    ///
    /// # Errors
    ///
    /// [`SendError::NotConnected`] if `to` has no current connection, and
    /// [`SendError::NotNegotiated`] if the two did not negotiate `files`
    /// (one of them is too old, or has receiving turned off).
    pub fn send_files(&self, to: &DeviceId, paths: Vec<PathBuf>) -> Result<u64, SendError> {
        let shared = &self.inner.shared;
        let files = {
            let table = shared.lock_table();
            let current = table
                .peers
                .get(to)
                .and_then(|slot| slot.current.as_ref())
                .ok_or(SendError::NotConnected)?;
            current.files.clone().ok_or(SendError::NotNegotiated)?
        };
        let id = shared.next_transfer.fetch_add(1, Ordering::Relaxed);
        files.start_sending(id, paths)?;
        Ok(id)
    }

    /// Sends `to` a tiny datagram that every build ignores, so that its
    /// Wi-Fi radio is awake when the cursor crosses to it (carries out
    /// `Action::Nudge`). Best effort: it never blocks, never fails and logs
    /// nothing, and a peer that is not connected, or whose connection has
    /// no datagrams, is skipped.
    pub fn nudge(&self, to: &DeviceId) {
        let table = self.inner.shared.lock_table();
        let Some(current) = table.peers.get(to).and_then(|slot| slot.current.as_ref()) else {
            return;
        };
        let conn = &current.link.conn;
        if conn
            .max_datagram_size()
            .is_some_and(|max| NUDGE.len() <= max)
        {
            let _ = conn.send_datagram(Bytes::from_static(&NUDGE));
        }
    }

    /// Replaces the displays sent in `Hello`. Connections made from now on
    /// carry them; current ones keep what they had.
    pub fn set_displays(&self, displays: Vec<DisplayInfo>) {
        self.inner.shared.lock_local().displays = displays;
    }

    /// Closes `peer`'s current connection with `SHUTDOWN`, as if it had
    /// dropped. `PeerDown` follows, and both sides then reconnect as usual.
    /// Returns false if the peer was not connected.
    pub fn disconnect(&self, peer: &DeviceId) -> bool {
        let table = self.inner.shared.lock_table();
        match table.peers.get(peer).and_then(|slot| slot.current.as_ref()) {
            Some(current) => {
                current.link.close(CloseCode::Shutdown, "disconnected");
                true
            }
            None => false,
        }
    }

    /// Sets or clears the fixed address dialled for `peer` (the same text
    /// as [`PinnedPeer::address`]), for example after the user edits it.
    /// The peer's dialer picks it up at once if it is waiting for one.
    /// Returns false if `peer` is not pinned.
    pub fn set_peer_address(&self, peer: &DeviceId, address: Option<String>) -> bool {
        let shared = &self.inner.shared;
        let found = match shared.lock_table().peers.get_mut(peer) {
            Some(slot) => {
                slot.pin.address = address;
                true
            }
            None => false,
        };
        shared.bump();
        found
    }

    /// The peers with a current connection.
    pub fn connected_peers(&self) -> Vec<DeviceId> {
        let table = self.inner.shared.lock_table();
        let mut peers: Vec<DeviceId> = table
            .peers
            .iter()
            .filter(|(_, slot)| slot.current.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        peers.sort();
        peers
    }

    /// Whether macOS turned out to be denying Hop Local Network access
    /// (dials to the LAN fail with "no route to host"). The link logs it
    /// once; the agent can also tell the user. Always false elsewhere.
    pub fn local_network_denied(&self) -> bool {
        self.inner
            .shared
            .local_network_denied
            .load(Ordering::Relaxed)
    }

    /// The address the link is bound to (useful when binding port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.shared.local_addr
    }

    /// This device's id.
    pub fn device_id(&self) -> &DeviceId {
        &self.inner.shared.me
    }

    /// Shuts the link down, in the order the design gives for quitting:
    ///
    /// 1. no new connections, and no more events for the core;
    /// 2. messages already queued (the `Release` or `TakeBack` the core
    ///    sent just before) are delivered, waiting at most
    ///    [`Timing::shutdown_wait`] (500 ms);
    /// 3. every connection is closed with `SHUTDOWN`, so peers fail open at
    ///    once;
    /// 4. the mDNS advertisement is withdrawn, waiting at most 1 s.
    ///
    /// Input must already be given back before this is called, so a hung
    /// network can never delay it. Calling it again does nothing.
    pub async fn shutdown(&self) {
        let shared = &self.inner.shared;
        let deadline = Instant::now() + shared.timing.shutdown_wait;
        let Some(currents) = shared.begin_stop() else {
            return;
        };
        let links: Vec<Arc<Link>> = currents
            .iter()
            .map(|current| current.link.clone())
            .collect();
        // Dropping the send queues lets each writer deliver what is queued,
        // finish its stream and wait for the peer's acknowledgement.
        drop(currents);
        shared.wait_for_connections(deadline).await;
        for link in &links {
            link.close(CloseCode::Shutdown, "shutting down");
        }
        shared
            .endpoint
            .close(CloseCode::Shutdown.varint(), b"shutting down");
        // Give the close packets a moment to leave, even if delivering the
        // queued messages used up the time.
        let idle_deadline = deadline.max(Instant::now() + Duration::from_millis(50));
        let _ = tokio::time::timeout_at(idle_deadline, shared.endpoint.wait_idle()).await;
        let discovery = shared.lock_discovery().take();
        if let Some(discovery) = discovery {
            discovery.shutdown(Duration::from_secs(1)).await;
        }
        tracing::info!("Hop link stopped");
    }

    /// The verifier counters, for tests.
    #[cfg(test)]
    pub(crate) fn verifier_stats(&self) -> (usize, usize, usize) {
        self.inner.shared.stats.snapshot()
    }

    /// The current connection to `peer`, for tests.
    #[cfg(test)]
    pub(crate) fn current_connection(&self, peer: &DeviceId) -> Option<quinn::Connection> {
        let table = self.inner.shared.lock_table();
        table
            .peers
            .get(peer)
            .and_then(|slot| slot.current.as_ref())
            .map(|current| current.link.conn.clone())
    }
}

/// State shared by the link's tasks and the handle.
pub(crate) struct Shared {
    pub(crate) me: DeviceId,
    pub(crate) identity: Arc<Identity>,
    pub(crate) endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
    /// The pinned fingerprints, fixed for the life of the link.
    pins: Arc<PinSet>,
    /// Peer slots. Never held across an await, and never taken inside a
    /// TLS verifier.
    table: Mutex<Table>,
    /// Bumped after every change to the table, so tasks waiting for "not
    /// connected" or "my dial finished" wake up and look again.
    changes: watch::Sender<u64>,
    pub(crate) queue: Arc<EventQueue>,
    local: Mutex<Local>,
    pub(crate) timing: Timing,
    pub(crate) limits: Limits,
    /// Whether this device advertises `clipboard.text`.
    clipboard: bool,
    /// Receiving files, when this device advertises `files`.
    pub(crate) inbox: Option<Arc<Inbox>>,
    next_conn: AtomicU64,
    /// The id of the next file transfer this device sends.
    next_transfer: AtomicU64,
    pub(crate) handshakes: Arc<Semaphore>,
    stopping: watch::Sender<bool>,
    pub(crate) stats: Arc<VerifierStats>,
    pub(crate) client_transport: Arc<quinn::TransportConfig>,
    discovery: Mutex<Option<Discovery>>,
    /// Each connection task holds a clone; shutdown waits until all are
    /// gone.
    alive: Mutex<Option<mpsc::Sender<()>>>,
    alive_rx: tokio::sync::Mutex<mpsc::Receiver<()>>,
    /// Set when macOS turned out to be blocking Hop from the LAN.
    local_network_denied: AtomicBool,
}

/// What this device says about itself in `Hello`.
struct Local {
    name: String,
    app_version: String,
    os: Os,
    displays: Vec<DisplayInfo>,
}

pub(crate) struct Table {
    /// Set once shutdown starts: nothing is announced after that.
    pub(crate) closed: bool,
    pub(crate) peers: HashMap<DeviceId, Slot>,
}

/// One pinned peer.
pub(crate) struct Slot {
    pub(crate) pin: PinnedPeer,
    /// The connection the core was told about, if any.
    pub(crate) current: Option<Current>,
    /// Whether this device's dial to the peer is in progress.
    pub(crate) dialing: bool,
    /// How long the last current connection stayed up; taken by the
    /// dialer to decide whether to reset its backoff.
    pub(crate) last_uptime: Option<Duration>,
    /// Addresses mDNS found for the peer.
    pub(crate) discovered: Vec<SocketAddr>,
    /// Whether "incompatible" was logged since the last good connection.
    pub(crate) incompatible_logged: bool,
}

impl Slot {
    fn new(pin: PinnedPeer) -> Slot {
        Slot {
            pin,
            current: None,
            dialing: false,
            last_uptime: None,
            discovered: Vec::new(),
            incompatible_logged: false,
        }
    }
}

/// A peer's current connection.
pub(crate) struct Current {
    pub(crate) conn_id: u64,
    pub(crate) link: Arc<Link>,
    /// The control stream's send queue. Dropping it lets the writer flush
    /// and finish.
    pub(crate) outgoing: mpsc::Sender<Bytes>,
    pub(crate) dialled_by_us: bool,
    pub(crate) negotiated: Negotiated,
    /// The connection's file transfers, when both sides negotiated `files`.
    pub(crate) files: Option<Arc<ConnFiles>>,
}

impl Shared {
    pub(crate) fn lock_table(&self) -> MutexGuard<'_, Table> {
        // Panics are fatal for the agent; a poisoned lock only happens on
        // the way down, and the data is still consistent enough to close.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_local(&self) -> MutexGuard<'_, Local> {
        self.local.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_discovery(&self) -> MutexGuard<'_, Option<Discovery>> {
        self.discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes every task waiting for a table change.
    pub(crate) fn bump(&self) {
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    pub(crate) fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn subscribe_stopping(&self) -> watch::Receiver<bool> {
        self.stopping.subscribe()
    }

    pub(crate) fn is_stopping(&self) -> bool {
        *self.stopping.borrow()
    }

    /// The pinned device with exactly this fingerprint.
    pub(crate) fn pins_lookup(&self, fingerprint: &Fingerprint) -> Option<DeviceId> {
        self.pins.lookup(fingerprint).cloned()
    }

    /// Records that macOS denies Hop Local Network access, and says so once.
    pub(crate) fn note_local_network_denied(&self) {
        if !self.local_network_denied.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "macOS is blocking Hop from the local network: turn Hop on in System Settings > \
                 Privacy & Security > Local Network, then quit and reopen Hop"
            );
        }
    }

    pub(crate) fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// A token a connection task holds while it runs, or `None` once
    /// shutdown has started.
    pub(crate) fn alive_token(&self) -> Option<mpsc::Sender<()>> {
        self.alive
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// This device's `Hello`, with only the implemented capabilities.
    pub(crate) fn our_hello(&self) -> Hello {
        let local = self.lock_local();
        let mut hello = Hello::new(
            local.app_version.clone(),
            self.me.clone(),
            local.name.clone(),
            local.displays.clone(),
        );
        hello.os = local.os;
        hello.capabilities = implemented_capabilities(self.clipboard, self.inbox.is_some());
        hello
    }

    /// Marks the link as stopping and takes every current connection out of
    /// the table. `None` if it was already stopping.
    fn begin_stop(&self) -> Option<Vec<Current>> {
        let currents = {
            let mut table = self.lock_table();
            if table.closed {
                return None;
            }
            table.closed = true;
            table
                .peers
                .values_mut()
                .filter_map(|slot| slot.current.take())
                .collect()
        };
        self.queue.close();
        self.stopping.send_replace(true);
        self.alive
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.bump();
        Some(currents)
    }

    /// Waits until every connection task has finished, or `deadline`.
    async fn wait_for_connections(&self, deadline: Instant) {
        let mut alive = self.alive_rx.lock().await;
        // `recv` returns `None` once every token is dropped.
        let _ = tokio::time::timeout_at(deadline, alive.recv()).await;
    }

    /// Closes everything at once, without waiting: the handle was dropped
    /// without a shutdown.
    fn abort(&self) {
        if let Some(currents) = self.begin_stop() {
            for current in currents {
                current.link.close(CloseCode::Shutdown, "shutting down");
            }
        }
        self.endpoint
            .close(CloseCode::Shutdown.varint(), b"shutting down");
        if let Some(discovery) = self.lock_discovery().take() {
            discovery.stop_now();
        }
    }
}

/// Accepts incoming connections: at most `max_handshakes` handshakes at a
/// time, each within the handshake timeout.
async fn accept_loop(shared: Arc<Shared>) {
    let mut stopping = shared.subscribe_stopping();
    loop {
        let incoming = tokio::select! {
            incoming = shared.endpoint.accept() => match incoming {
                Some(incoming) => incoming,
                None => return,
            },
            _ = stopping.wait_for(|stopping| *stopping) => return,
        };
        let Ok(permit) = shared.handshakes.clone().try_acquire_owned() else {
            if let Some(suppressed) = ratelimit::allow("too many handshakes") {
                tracing::warn!(
                    from = %incoming.remote_address(),
                    suppressed,
                    "refusing a connection: too many handshakes in progress"
                );
            }
            incoming.refuse();
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            let connecting = match incoming.accept() {
                Ok(connecting) => connecting,
                Err(_) => return,
            };
            let conn = match tokio::time::timeout(shared.timing.handshake_timeout, connecting).await
            {
                Ok(Ok(conn)) => conn,
                Ok(Err(err)) => {
                    if let Some(suppressed) = ratelimit::allow("incoming handshake failed") {
                        tracing::warn!(
                            from = %remote,
                            suppressed,
                            error = %display_text(&err.to_string(), 120),
                            "refused a connection that is not from a paired device"
                        );
                    }
                    return;
                }
                Err(_) => {
                    if let Some(suppressed) = ratelimit::allow("incoming handshake slow") {
                        tracing::warn!(from = %remote, suppressed, "a connection attempt took too long");
                    }
                    return;
                }
            };
            drop(permit);
            let _ = conn::establish(shared, conn, conn::Direction::Incoming, None).await;
        });
    }
}

/// Starts mDNS: advertises this device and feeds discovered addresses of
/// pinned peers to their dialers. Failure is not fatal: configured
/// addresses still work.
fn start_discovery(shared: &Arc<Shared>, device_name: &str) {
    let weak = Arc::downgrade(shared);
    let on_change = Arc::new(move |found: &[crate::discovery::FoundDevice]| {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        {
            let mut table = shared.lock_table();
            for slot in table.peers.values_mut() {
                slot.discovered = found
                    .iter()
                    .filter(|device| !device.pairing && device.id == slot.pin.id)
                    .flat_map(|device| device.addresses.iter().copied().map(SocketAddr::V4))
                    .collect();
            }
        }
        shared.bump();
    });
    match Discovery::start(shared.me.clone(), on_change) {
        Ok(discovery) => {
            if let Err(err) = discovery.advertise(device_name, shared.local_addr.port(), false) {
                tracing::warn!(error = %err, "could not advertise this device on the local network");
            }
            *shared.lock_discovery() = Some(discovery);
        }
        Err(err) => {
            tracing::warn!(error = %err, "local network discovery is off; configured addresses still work");
        }
    }
}
