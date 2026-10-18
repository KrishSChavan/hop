//! Dialling a pinned peer while not connected to it, with backoff.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hop_types::DeviceId;
use tokio::time::Instant;

use super::Shared;
use super::conn::{self, Direction, SetupError};
use crate::Fingerprint;
use crate::address::resolve_address;
use crate::backoff::Backoff;
use crate::ratelimit;
use crate::sanitize::display_text;
use crate::tls::{self, Accept};

/// What one round of dialling every candidate address gave.
enum Round {
    /// A connection to the peer is current (this round's or another).
    Connected,
    /// The peer runs an incompatible build.
    Incompatible,
    /// There was no address to try.
    NoCandidates,
    /// Every address failed.
    Failed,
}

/// Dials `peer` whenever it has no current connection, until shutdown.
///
/// Both devices dial, so an inbound block on one laptop (a Wi-Fi network
/// Windows marks Public, a managed Mac's firewall) cannot keep them apart:
/// the device with the lower id dials at once, the other after
/// `higher_id_delay`, and only while still not connected. Reconnects back
/// off with jitter, and the backoff starts over only after a connection
/// stayed up for `stable_after`.
pub(crate) async fn run_dialer(shared: Arc<Shared>, peer: DeviceId) {
    let timing = shared.timing.clone();
    let mut backoff = Backoff::new(timing.backoff_min, timing.backoff_max);
    loop {
        let Some(last_uptime) = wait_until_disconnected(&shared, &peer).await else {
            return;
        };
        // A connection that stayed up long enough starts the backoff over;
        // one that dropped quickly waits first, so a peer that accepts and
        // drops at once cannot cause a dial storm.
        let quick_drop = match last_uptime {
            Some(uptime) if uptime >= timing.stable_after => {
                backoff.reset();
                false
            }
            Some(_) => true,
            None => false,
        };
        if quick_drop && !pause(&shared, backoff.next_delay(), false).await {
            return;
        }
        if shared.me > peer && !pause(&shared, timing.higher_id_delay, false).await {
            return;
        }
        loop {
            if shared.is_stopping() {
                return;
            }
            if is_connected(&shared, &peer) {
                break;
            }
            let (delay, wake_on_change) = match dial_round(&shared, &peer).await {
                Round::Connected => break,
                Round::Incompatible => (timing.incompatible_retry, false),
                // Nothing to dial yet: wait for mDNS to find something, or
                // look up the configured address again later.
                Round::NoCandidates => (backoff.next_delay(), true),
                Round::Failed => (backoff.next_delay(), false),
            };
            if !pause(&shared, delay, wake_on_change).await {
                return;
            }
        }
    }
}

/// Waits until `peer` has no current connection. Returns how long the
/// last one stayed up (`None` if there was none), or `None` at shutdown.
async fn wait_until_disconnected(shared: &Shared, peer: &DeviceId) -> Option<Option<Duration>> {
    let mut changes = shared.subscribe_changes();
    loop {
        changes.borrow_and_update();
        {
            let mut table = shared.lock_table();
            if table.closed {
                return None;
            }
            let slot = table.peers.get_mut(peer)?;
            if slot.current.is_none() {
                return Some(slot.last_uptime.take());
            }
        }
        changes.changed().await.ok()?;
    }
}

/// Sleeps for `delay`, or until a table change when `wake_on_change`.
/// Returns false at shutdown.
async fn pause(shared: &Shared, delay: Duration, wake_on_change: bool) -> bool {
    let mut stopping = shared.subscribe_stopping();
    let mut changes = shared.subscribe_changes();
    changes.borrow_and_update();
    let deadline = Instant::now() + delay;
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => {}
        _ = stopping.wait_for(|stopping| *stopping) => {}
        _ = changes.changed(), if wake_on_change => {}
    }
    !shared.is_stopping()
}

fn is_connected(shared: &Shared, peer: &DeviceId) -> bool {
    shared
        .lock_table()
        .peers
        .get(peer)
        .is_some_and(|slot| slot.current.is_some())
}

/// Tries every candidate address once, in order: the configured address,
/// then what mDNS found. A failed dial never removes a candidate.
async fn dial_round(shared: &Arc<Shared>, peer: &DeviceId) -> Round {
    let Some((fingerprint, configured, discovered)) = ({
        let table = shared.lock_table();
        table.peers.get(peer).map(|slot| {
            (
                slot.pin.fingerprint,
                slot.pin.address.clone(),
                slot.discovered.clone(),
            )
        })
    }) else {
        return Round::Failed;
    };
    let mut candidates: Vec<SocketAddr> = Vec::new();
    if let Some(address) = configured {
        let port = shared.endpoint.local_addr().map_or(0, |addr| addr.port());
        match resolve_address(&address, port, shared.timing.resolve_timeout).await {
            Ok(found) => candidates.extend(found),
            Err(err) => {
                if let Some(suppressed) = ratelimit::allow("address lookup failed") {
                    tracing::warn!(
                        address = %display_text(&address, 80),
                        suppressed,
                        error = %err,
                        "could not look up a paired device's address"
                    );
                }
            }
        }
    }
    for address in discovered {
        if !candidates.contains(&address) {
            candidates.push(address);
        }
    }
    candidates.retain(|address| address.port() != 0);
    if candidates.is_empty() {
        return Round::NoCandidates;
    }
    for address in candidates {
        if shared.is_stopping() || is_connected(shared, peer) {
            return Round::Connected;
        }
        match dial_one(shared, peer, fingerprint, address).await {
            Ok(()) => return Round::Connected,
            Err(err) if err.is_incompatible() => {
                if matches!(err, SetupError::Remote(_)) {
                    // The peer found this build incompatible; when this side
                    // finds it, the Hello check logs it instead.
                    super::conn::log_incompatible_once(
                        shared,
                        peer,
                        "the other device refused this version",
                    );
                }
                return Round::Incompatible;
            }
            // The peer kept another connection. That one is normally
            // already current here; if not (the peer still holds a stale
            // connection), back off like any failure rather than redial at
            // once.
            Err(err) if err.is_duplicate() && is_connected(shared, peer) => {
                return Round::Connected;
            }
            Err(SetupError::Lost) => check_local_network(shared, address),
            Err(_) => {}
        }
    }
    Round::Failed
}

/// After a dial to a LAN address failed without an answer: on macOS, a
/// denied Local Network permission makes every send to the LAN fail with
/// "no route to host", which quinn only reports as a timeout. A throwaway
/// empty datagram tells the two apart, so Hop can say what is wrong
/// instead of retrying silently.
fn check_local_network(shared: &Shared, address: SocketAddr) {
    if local_network_denied(address) {
        shared.note_local_network_denied();
    }
}

#[cfg(target_os = "macos")]
fn local_network_denied(address: SocketAddr) -> bool {
    let SocketAddr::V4(v4) = address else {
        return false;
    };
    if !crate::address::is_local_lan(*v4.ip()) {
        return false;
    }
    let Ok(socket) = std::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))) else {
        return false;
    };
    matches!(
        socket.send_to(&[], address),
        Err(err) if err.kind() == std::io::ErrorKind::HostUnreachable
    )
}

/// Only macOS has a Local Network permission.
#[cfg(not(target_os = "macos"))]
fn local_network_denied(_address: SocketAddr) -> bool {
    false
}

/// Marks a dial in progress, for the duplicate rule, until dropped (even
/// if the dial is cancelled).
struct Dialing<'a> {
    shared: &'a Shared,
    peer: &'a DeviceId,
}

impl<'a> Dialing<'a> {
    fn start(shared: &'a Shared, peer: &'a DeviceId) -> Dialing<'a> {
        if let Some(slot) = shared.lock_table().peers.get_mut(peer) {
            slot.dialing = true;
        }
        shared.bump();
        Dialing { shared, peer }
    }
}

impl Drop for Dialing<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.shared.lock_table().peers.get_mut(self.peer) {
            slot.dialing = false;
        }
        self.shared.bump();
    }
}

/// Dials one address, checking the server against exactly this peer's
/// pin, and sets the connection up.
async fn dial_one(
    shared: &Arc<Shared>,
    peer: &DeviceId,
    fingerprint: Fingerprint,
    address: SocketAddr,
) -> Result<(), SetupError> {
    let _dialing = Dialing::start(shared, peer);
    let config = tls::client_config(
        &shared.identity,
        Accept::Exactly(fingerprint),
        tls::ALPN_SESSION,
        shared.stats.clone(),
    )
    .and_then(|tls| tls::quic_client(tls, shared.client_transport.clone()))
    .map_err(|_| SetupError::Lost)?;
    let connecting = shared
        .endpoint
        .connect_with(config, address, tls::SERVER_NAME)
        .map_err(|_| SetupError::Lost)?;
    let conn = match tokio::time::timeout(shared.timing.handshake_timeout, connecting).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(err)) => {
            if let Some(suppressed) = ratelimit::allow("dial failed") {
                tracing::debug!(
                    id = %peer,
                    %address,
                    suppressed,
                    error = %display_text(&err.to_string(), 120),
                    "could not connect"
                );
            }
            return Err(match err {
                quinn::ConnectionError::ApplicationClosed(close) => {
                    SetupError::Remote(close.error_code.into_inner())
                }
                _ => SetupError::Lost,
            });
        }
        // Dropping the attempt abandons it.
        Err(_) => return Err(SetupError::Lost),
    };
    conn::establish(
        shared.clone(),
        conn,
        Direction::Outgoing,
        Some(peer.clone()),
    )
    .await
}
