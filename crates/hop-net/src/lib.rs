//! Hop's network layer: device identity, the encrypted QUIC link with
//! pinned keys, pairing with a one-time code, discovery on the local
//! network, and on the link the clipboard text and file transfers. The spec
//! is docs/M2-DESIGN.md, with docs/M4-M5-DESIGN.md for the clipboard,
//! nudges and files; this crate follows them, and its rustdoc says where a
//! detail is decided.
//!
//! # Pieces
//!
//! | Piece | Entry points | Runs in |
//! |---|---|---|
//! | Identity | [`Identity::load_or_create`], [`Fingerprint`] | `hop run` and `hop pair` |
//! | Session link | [`start`] → [`NetHandle`] + [`NetEvents`] | `hop run` |
//! | Files | [`NetHandle::send_files`] → [`NetEvent::Transfer`] | `hop run` |
//! | Pairing | [`PairingHost`], [`pair_join`], [`PairingCode`] | `hop pair` |
//! | Discovery | built into [`start`] and [`PairingHost::bind`]; [`find_pairing_hosts`] | both |
//!
//! # Using the link from the agent (`hop run`)
//!
//! 1. **Identity.** `Identity::load_or_create(path)` with the identity path
//!    (`~/Library/Application Support/Hop/identity.pem`, or
//!    `%LOCALAPPDATA%\Hop\identity.pem`). Correct `device.id` in the config
//!    when it differs from [`Identity::device_id`]. A file that exists but
//!    does not parse is an error to show the user, never a reason to make a
//!    new key.
//! 2. **Peers.** Turn each `peers[]` entry that has a fingerprint into a
//!    [`PinnedPeer`] ([`Fingerprint::from_hex`]); an entry without one
//!    cannot connect until it is paired.
//! 3. **Start before capture.** Inside the agent's tokio runtime (two
//!    workers), call [`start`] with a [`NetConfig`]
//!    ([`NetConfig::new`]`(network.port, name, version)`, then set
//!    `displays`, `peers` and `on_event`, `clipboard` from
//!    `clipboard.enabled`, and `files` from `transfer.enabled` and
//!    `transfer.received_dir`). It binds `0.0.0.0:<port>` at once, so a
//!    second copy of Hop fails here, before it touches input.
//! 4. **The core thread** (a plain thread, not tokio) waits on a doorbell
//!    with a 10 ms timeout. `NetConfig::on_event` rings it; so does the
//!    capture sink. After each wake-up it drains [`NetEvents::try_recv`]
//!    and feeds the core: [`NetEvent::PeerUp`] → `PeerConnected`,
//!    [`NetEvent::PeerDown`] → `PeerDisconnected`, [`NetEvent::Message`] →
//!    `PeerMessage`. [`NetEvent::Transfer`] (files sent or received) stays
//!    with the agent: every transfer ends with exactly one. `PeerUp::os` is
//!    what the Cmd/Ctrl swap keys on; [`PeerUp::supports_clipboard`] and
//!    [`PeerUp::supports_files`] say what else the two negotiated.
//! 5. **Sending.** Carry out `Action::Send { to, msg }` with
//!    [`NetHandle::send`]. It never blocks and never drops a reliable
//!    message: a full queue closes that connection with `OVERLOADED`, and
//!    the core then gets `PeerDown` and fails open. `Motion` goes as a
//!    datagram when it fits. Send errors only need logging. Carry out
//!    `Action::Nudge { to }` with [`NetHandle::nudge`] (a datagram every
//!    build ignores, best effort), and send dropped files with
//!    [`NetHandle::send_files`], which returns the transfer's id at once.
//! 6. **Quitting**, in this order: give input back (capture passive,
//!    injected keys released, cursor restored); send `Release` or
//!    `TakeBack` if in a session; then [`NetHandle::shutdown`], which
//!    delivers what is queued (at most 500 ms), closes every connection
//!    with `SHUTDOWN` so the peer fails open at once, and withdraws the mDNS
//!    advertisement (at most 1 s).
//!
//! The link owns everything else: both devices listen and dial (the lower
//! id at once, the higher after 1 s, only while not connected), duplicate
//! connections are resolved the same way on both sides, reconnects back
//! off from 250 ms to 5 s with jitter, and incompatible builds are retried
//! once a minute.
//!
//! # Pairing (`hop pair`)
//!
//! Run from Terminal on both laptops (not through `open`, which would send
//! the code to a log file).
//!
//! - **Showing the code:** [`PairingCode::generate`], show
//!   [`PairingCode::reveal`] on screen, [`PairingHost::bind`] on
//!   `0.0.0.0:<network.port + 1>` with `advertise = true`, then
//!   [`PairingHost::wait_for_joiner`]. Ask the user y/N about
//!   [`PendingPairing::peer`] within [`PendingPairing::remaining`], then
//!   [`PendingPairing::accept`] with a closure that saves the peer (it runs
//!   before the joiner is told), or [`PendingPairing::decline`].
//! - **Typing the code:** [`PairingCode::parse`] the argument. With
//!   `--address`, [`resolve_address`] it (default port
//!   [`DEFAULT_PAIRING_PORT`]); otherwise [`find_pairing_hosts`], and if
//!   more than one laptop is found, list them and ask for `--address`
//!   instead of picking. Then [`pair_join`], and save the returned
//!   [`PairedPeer`].
//! - Saving replaces an entry with the same id, or else one without a
//!   fingerprint whose `os` matches; re-read the config just before saving.
//!   A running agent picks the new peer up when it restarts.
//!
//! # Guarantees
//!
//! - Every connection is a full mutual TLS 1.3 handshake with Ed25519 only,
//!   no resumption and no early data. The dialler accepts exactly the pin
//!   of the peer it dials, the listener only pinned peers, and both verify
//!   the handshake signature against the pinned certificate.
//! - The core hears only about each peer's current connection: nothing
//!   from a replaced connection reaches it, and datagrams that arrive
//!   before `PeerUp` are dropped.
//! - Everything a peer or a stranger sends is bounded ([`Limits`], plus a
//!   64 KiB `Hello`, 1 KiB pairing messages, and fixed QUIC windows). At
//!   most one `ClipboardText` per peer waits for the core.
//! - Clipboard text and files travel only between devices that both turned
//!   them on: what was not negotiated is refused when sent and dropped when
//!   received, and a peer without `files` never gets a stream for data.
//! - Files go only into the Received folder, under cleaned names that
//!   never overwrite anything, and only once their size and SHA-256 check
//!   out; a failure or a lost connection deletes what had not finished. On
//!   Windows each one is marked as coming from another computer (the Mark
//!   of the Web) before it gets its name, so it is checked as a download is.
//!   The folder is not touched before the first offer. File data is paced
//!   and comes after the control stream, so input keeps flowing, and disk
//!   work never runs on the network's workers.
//! - Logs (through `tracing`) hold connections, fingerprints, counts and
//!   errors, never keys, key codes, clipboard text, file names, file
//!   contents or checksums, pairing codes, SPAKE2 values, MACs, exporter
//!   values or TLS secrets; text from peers is cleaned first, and warnings
//!   peers or strangers can trigger are rate-limited. rustls is built
//!   without logging and mdns-sd with its logging off; quinn logs through
//!   `tracing`, so the agent should cap it at `warn`.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use hop_net::{FilesConfig, Identity, NetConfig, NetEvent};
//!
//! # async fn demo(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
//! let identity = Arc::new(Identity::load_or_create(path)?.identity);
//! let mut config = NetConfig::new(47800, "Krish's MacBook", env!("CARGO_PKG_VERSION"));
//! config.on_event = Some(Arc::new(|| { /* ring the core thread's doorbell */ }));
//! config.clipboard = true;
//! config.files = Some(FilesConfig {
//!     received_dir: "/Users/krish/Downloads/Hop Received".into(),
//! });
//! let (net, events) = hop_net::start(identity, config).await?;
//! // On the core thread:
//! while let Some(event) = events.try_recv() {
//!     match event {
//!         NetEvent::PeerUp(up) => { /* core: PeerConnected { up.peer, up.os, up.displays } */ }
//!         NetEvent::PeerDown { peer, .. } => { /* core: PeerDisconnected { peer } */ }
//!         NetEvent::Message { from, msg } => { /* core: PeerMessage { from, msg } */ }
//!         NetEvent::Transfer(transfer) => { /* the agent's own: how a transfer ended */ }
//!     }
//! }
//! // When quitting, after giving input back:
//! net.shutdown().await;
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Bytes come from other machines, so library code must not be able to
// panic on them. Tests may unwrap freely.
#![cfg_attr(
    not(test),
    warn(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod address;
mod backoff;
mod close;
mod discovery;
mod events;
mod fingerprint;
mod identity;
mod link;
mod pairing;
mod ratelimit;
mod sanitize;
mod tls;
mod transfer;
mod transport;

pub use address::resolve_address;
pub use close::CloseCode;
pub use discovery::{DiscoveryError, FoundDevice, SERVICE_TYPE, find_pairing_hosts};
pub use events::{DownReason, NetEvent, NetEvents, PeerUp};
pub use fingerprint::{Fingerprint, FingerprintError};
pub use identity::{Identity, IdentityError, LoadedIdentity};
pub use link::{
    FilesConfig, Limits, NetConfig, NetError, NetHandle, PinnedPeer, SendError, Timing, start,
};
pub use pairing::{
    DEFAULT_PAIRING_PORT, PairError, PairInfo, PairedPeer, PairingCode, PairingHost, PairingTiming,
    PendingPairing, pair_join,
};
pub use transfer::{Refusal, TransferEvent, TransferFailure};
