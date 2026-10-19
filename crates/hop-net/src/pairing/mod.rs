//! Pairing two laptops once with a one-time code: `hop-pair/1` (docs/M2-DESIGN.md,
//! "Pairing").
//!
//! The laptop that shows the code is the **host** ([`PairingHost`], SPAKE2
//! side B). The laptop where the user types it is the **joiner**
//! ([`pair_join`], SPAKE2 side A, and the TLS client). Pairing runs in its
//! own short-lived process (`hop pair`) on its own port, so the session
//! listener never accepts an unknown certificate.
//!
//! In order:
//!
//! 1. The joiner connects with ALPN `hop-pair/1`. Both verifiers accept
//!    any single Ed25519 certificate here, still checking the handshake
//!    signature, so each side learns the other's fingerprint from the
//!    handshake itself.
//! 2. SPAKE2 (`spake2` 0.4, `Ed25519Group`, asymmetric): the password is the
//!    six ASCII digits, and both sides pass `id_a` = the joiner's
//!    fingerprint and `id_b` = the host's, as 32 raw bytes, in that order.
//! 3. Key confirmation, joiner first: `cA = HMAC-SHA256(K, "hop pair confirm
//!    A" || E)` and `cB` likewise with "B", where `E` is 32 bytes of this
//!    connection's TLS exporter with the RFC 9266 label
//!    `EXPORTER-Channel-Binding` and an empty context. A relay holds two TLS
//!    sessions with different certificates and exporters, so it fails.
//! 4. The user confirms on the host, the host saves and tells the joiner,
//!    and the joiner saves.
//!
//! The code never crosses the network and is never logged: [`PairingCode`]
//! has no `Display`, and its `Debug` is redacted.

mod wire;

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hop_types::{DeviceId, Os};
use quinn::{RecvStream, SendStream};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use zeroize::{Zeroize, Zeroizing};

use crate::discovery::{Discovery, FoundDevice};
use crate::sanitize::{MAX_DISPLAY_CHARS, display_text};
use crate::tls::{self, Accept, VerifierStats};
use crate::transport::TransportParams;
use crate::{CloseCode, Fingerprint, Identity, ratelimit};
use wire::{Failure, Kind};

/// Digits in a pairing code.
const CODE_DIGITS: usize = 6;

/// The largest multiple of 1,000,000 that fits in a `u32`: random values at
/// or above it are drawn again, so every code is equally likely.
const CODE_REJECT_FROM: u32 = 4_294_000_000;

/// The default pairing port is the session port plus one.
pub const DEFAULT_PAIRING_PORT: u16 = 47801;

/// How long a side whose peer ended its stream early waits for the peer's
/// close code, which says why (see [`Failure::explained`]).
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// A six-digit one-time pairing code.
///
/// It is shown on the host's screen only ([`PairingCode::reveal`]) and
/// typed on the joiner. It never crosses the network, there is no `Display`
/// so it cannot slip into a log line, `Debug` is redacted, and its digits
/// are wiped when it is dropped.
pub struct PairingCode {
    digits: [u8; CODE_DIGITS],
}

impl PairingCode {
    /// A new code from the OS random number generator, uniform over all
    /// one million codes (rejection sampling, so no modulo bias).
    ///
    /// # Errors
    ///
    /// [`PairError::Random`] if the OS generator fails.
    pub fn generate() -> Result<PairingCode, PairError> {
        loop {
            let mut bytes = Zeroizing::new([0u8; 4]);
            getrandom::fill(bytes.as_mut()).map_err(|_| PairError::Random)?;
            let value = u32::from_le_bytes(*bytes);
            if value < CODE_REJECT_FROM {
                return Ok(PairingCode::from_number(value % 1_000_000));
            }
        }
    }

    /// Reads a code the user typed. Spaces and dashes are allowed ("482
    /// 913", "482-913"); anything else must be exactly six digits.
    ///
    /// # Errors
    ///
    /// [`PairError::InvalidCode`], which never repeats the input.
    pub fn parse(text: &str) -> Result<PairingCode, PairError> {
        let mut digits = [0u8; CODE_DIGITS];
        let mut count = 0;
        for c in text.chars() {
            match c {
                ' ' | '-' => {}
                '0'..='9' => {
                    let slot = digits.get_mut(count).ok_or(PairError::InvalidCode)?;
                    *slot = c as u8;
                    count += 1;
                }
                _ => {
                    digits.zeroize();
                    return Err(PairError::InvalidCode);
                }
            }
        }
        if count != CODE_DIGITS {
            digits.zeroize();
            return Err(PairError::InvalidCode);
        }
        Ok(PairingCode { digits })
    }

    /// The code as the host shows it, "482 913". Show it on screen only:
    /// never log it, and never show it in a screen share.
    pub fn reveal(&self) -> String {
        let text: String = self.digits.iter().map(|digit| char::from(*digit)).collect();
        let (first, second) = text.split_at(CODE_DIGITS / 2);
        format!("{first} {second}")
    }

    fn from_number(number: u32) -> PairingCode {
        let mut digits = [b'0'; CODE_DIGITS];
        let mut rest = number;
        for slot in digits.iter_mut().rev() {
            // `rest % 10` is a single digit, so the cast cannot truncate.
            *slot = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        PairingCode { digits }
    }

    /// The SPAKE2 password: the six ASCII digits.
    fn password(&self) -> Password {
        Password::new(self.digits)
    }
}

impl Drop for PairingCode {
    fn drop(&mut self) {
        self.digits.zeroize();
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(redacted)")
    }
}

/// What this laptop tells the other one about itself while pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairInfo {
    /// This laptop's name, such as "Krish's MacBook" (cleaned and cut to 64
    /// characters before it is sent).
    pub name: String,
    /// This laptop's OS.
    pub os: Os,
}

/// The other laptop, as pairing proved it. Save it in `peers[]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedPeer {
    /// The name it gave, cleaned for display (no control characters, at
    /// most 64 characters). Trusted only because the key confirmation
    /// checked out.
    pub name: String,
    /// Its device id: the first 32 hex characters of `fingerprint`.
    pub id: DeviceId,
    /// Its OS.
    pub os: Os,
    /// Its fingerprint, from the TLS handshake the confirmation is bound
    /// to. This is the pin.
    pub fingerprint: Fingerprint,
}

/// Pairing timeouts and limits. The defaults are the design's values;
/// tests shorten them.
#[derive(Clone, Debug)]
pub struct PairingTiming {
    /// How long a code stays valid on the host (2 minutes).
    pub code_lifetime: Duration,
    /// The SPAKE2 messages and both confirmations must be exchanged within
    /// this, from the end of the TLS handshake (10 s).
    pub exchange_timeout: Duration,
    /// A TLS handshake must finish within this (5 s).
    pub handshake_timeout: Duration,
    /// How long the joiner waits for the user to answer on the host (2
    /// minutes).
    pub verdict_timeout: Duration,
    /// Failed attempts after which the code dies (3).
    pub max_attempts: u32,
}

impl Default for PairingTiming {
    fn default() -> Self {
        PairingTiming {
            code_lifetime: Duration::from_secs(120),
            exchange_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(5),
            verdict_timeout: Duration::from_secs(120),
            max_attempts: 3,
        }
    }
}

/// Why pairing failed.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// The typed code is not six digits (spaces and dashes allowed).
    #[error("the pairing code must be 6 digits")]
    InvalidCode,
    /// The OS random number generator failed.
    #[error("could not get random numbers from the system")]
    Random,
    /// The pairing port could not be bound (another `hop pair` running?).
    #[error("could not listen on {addr} for pairing (is hop pair already running?)")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// The encryption setup could not be built.
    #[error("could not set up encryption: {0}")]
    Setup(String),
    /// The code's two minutes ran out, or the other laptop's did.
    #[error("the pairing code expired; run hop pair again on both laptops")]
    Expired,
    /// Too many joiners used a wrong code, so the code is dead.
    #[error(
        "3 attempts used a wrong code, so this code no longer works. Someone may be \
         trying to pair with this laptop; run hop pair again to get a new code"
    )]
    TooManyAttempts,
    /// The codes did not match (or a relay sat in between).
    #[error("the code did not match; check it and run hop pair again on both laptops")]
    WrongCode,
    /// The host is busy with another joiner.
    #[error("the other laptop is busy pairing with another device")]
    Busy,
    /// The user on the host said no, or did not answer in time.
    #[error("pairing was declined on the other laptop")]
    Declined,
    /// No address could be reached.
    #[error(
        "could not reach the other laptop. It may be blocking incoming connections \
         (a managed Mac's firewall can); try showing the code on the other laptop instead"
    )]
    Unreachable,
    /// No address was given.
    #[error("no address to pair with")]
    NoAddress,
    /// The other side broke the pairing protocol.
    #[error("the other device broke the pairing protocol: {0}")]
    Protocol(String),
    /// The connection failed partway.
    #[error("the connection to the other laptop was lost: {0}")]
    Connection(String),
    /// The host could not save the pairing, so it declined.
    #[error("could not save the pairing: {0}")]
    Save(String),
    /// The other side holds this laptop's own key.
    #[error("that is this laptop itself")]
    SelfPairing,
    /// The pairing advertisement could not start.
    #[error("could not advertise pairing on the local network: {0}")]
    Discovery(String),
}

/// The laptop showing the code: listens on the pairing port for one
/// joiner at a time.
///
/// ```no_run
/// # async fn demo(identity: std::sync::Arc<hop_net::Identity>) -> Result<(), hop_net::PairError> {
/// use hop_net::{PairInfo, PairingCode, PairingHost, PairingTiming};
///
/// let code = PairingCode::generate()?;
/// println!("Pairing code: {}", code.reveal()); // on screen only, never in a log
/// let me = PairInfo { name: "Krish's MacBook".into(), os: hop_types::Os::MacOs };
/// let address = "0.0.0.0:47801".parse().expect("valid");
/// let host = PairingHost::bind(identity, me, address, code, PairingTiming::default(), true).await?;
/// let pending = host.wait_for_joiner().await?;
/// // Ask "Pair with <name>? [y/N]", giving up when pending.remaining() runs out.
/// let peer = pending.accept(|peer| save_to_config(peer)).await?;
/// # Ok(()) }
/// # fn save_to_config(_: &hop_net::PairedPeer) -> Result<(), std::io::Error> { Ok(()) }
/// ```
pub struct PairingHost {
    ctx: Arc<HostContext>,
    endpoint: quinn::Endpoint,
    expires_at: Instant,
    failures: u32,
    on_failure: Option<Box<dyn FnMut(u32) + Send>>,
    discovery: Option<Discovery>,
}

impl fmt::Debug for PairingHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingHost")
            .field("address", &self.endpoint.local_addr().ok())
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

/// What a host's pairing runs share.
struct HostContext {
    identity: Arc<Identity>,
    me: PairInfo,
    code: PairingCode,
    timing: PairingTiming,
}

impl PairingHost {
    /// Binds the pairing port and starts the code's lifetime. With
    /// `advertise`, also announces a `<name> (pairing)` mDNS instance with
    /// `pair=1`, so the joiner can find this laptop without `--address`
    /// (that sends multicast; tests pass false). Must be called inside a
    /// tokio runtime.
    ///
    /// # Errors
    ///
    /// [`PairError::Bind`] if the port is taken, [`PairError::Setup`] if
    /// the TLS configuration cannot be built, and [`PairError::Discovery`]
    /// if advertising fails.
    pub async fn bind(
        identity: Arc<Identity>,
        me: PairInfo,
        address: SocketAddr,
        code: PairingCode,
        timing: PairingTiming,
        advertise: bool,
    ) -> Result<PairingHost, PairError> {
        let setup = |err: String| PairError::Setup(err);
        let transport = Arc::new(pairing_transport(1).build().map_err(setup)?);
        let tls = tls::server_config(
            &identity,
            Accept::AnyEd25519,
            tls::ALPN_PAIRING,
            Arc::new(VerifierStats::default()),
        )
        .map_err(|err| setup(err.to_string()))?;
        let server = tls::quic_server(tls, transport, 16).map_err(|err| setup(err.to_string()))?;
        let endpoint = bind_endpoint(address, Some(server))?;
        let local_port = endpoint
            .local_addr()
            .map_err(|source| PairError::Bind {
                addr: address,
                source,
            })?
            .port();
        let discovery = if advertise {
            let discovery =
                Discovery::start(identity.device_id(), Arc::new(|_: &[FoundDevice]| {}))
                    .map_err(|err| PairError::Discovery(err.to_string()))?;
            discovery
                .advertise(&me.name, local_port, true)
                .map_err(|err| PairError::Discovery(err.to_string()))?;
            Some(discovery)
        } else {
            None
        };
        let me = PairInfo {
            name: display_text(&me.name, MAX_DISPLAY_CHARS),
            os: me.os,
        };
        Ok(PairingHost {
            expires_at: Instant::now() + timing.code_lifetime,
            ctx: Arc::new(HostContext {
                identity,
                me,
                code,
                timing,
            }),
            endpoint,
            failures: 0,
            on_failure: None,
            discovery,
        })
    }

    /// The bound address (useful when binding port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    /// How long the code stays valid.
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// Calls `callback` with the number of failed attempts so far after
    /// each failed attempt, so `hop pair` can say that someone tried a
    /// wrong code.
    pub fn on_failed_attempt(&mut self, callback: impl FnMut(u32) + Send + 'static) {
        self.on_failure = Some(Box::new(callback));
    }

    /// Waits until a joiner proves it knows the code, handling one run at a
    /// time; a second joiner during a run is closed with `BUSY`.
    ///
    /// Every run in which this host has sent its SPAKE2 message counts as
    /// an attempt unless it succeeds, including a joiner that hangs up
    /// before confirming. The code dies after `max_attempts` failures,
    /// after its lifetime, or with this first success.
    ///
    /// The returned [`PendingPairing`] keeps refusing other joiners until
    /// it is accepted or declined.
    ///
    /// # Errors
    ///
    /// [`PairError::Expired`] when the lifetime runs out and
    /// [`PairError::TooManyAttempts`] after too many failures.
    pub async fn wait_for_joiner(mut self) -> Result<PendingPairing, PairError> {
        type Run = Pin<Box<dyn Future<Output = RunOutcome> + Send>>;
        let busy_permits = Arc::new(Semaphore::new(4));
        let mut run: Option<Run> = None;
        let result = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(self.expires_at) => break Err(PairError::Expired),
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else {
                        break Err(PairError::Connection("the pairing listener stopped".to_owned()));
                    };
                    if run.is_some() {
                        refuse_busy(incoming, &busy_permits, self.ctx.timing.handshake_timeout);
                    } else {
                        run = Some(Box::pin(host_run(incoming, self.ctx.clone())));
                    }
                }
                outcome = poll_run(&mut run) => {
                    run = None;
                    match outcome {
                        RunOutcome::Confirmed(confirmed) => break Ok(confirmed),
                        RunOutcome::Failed { counted: false } => {}
                        RunOutcome::Failed { counted: true } => {
                            self.failures += 1;
                            tracing::warn!(
                                failures = self.failures,
                                "a pairing attempt failed (wrong code, or the other side gave up)"
                            );
                            if let Some(callback) = &mut self.on_failure {
                                callback(self.failures);
                            }
                            if self.failures >= self.ctx.timing.max_attempts {
                                break Err(PairError::TooManyAttempts);
                            }
                        }
                    }
                }
            }
        };
        match result {
            Ok(confirmed) => {
                let refuser = tokio::spawn(refuse_all(
                    self.endpoint.clone(),
                    busy_permits,
                    self.ctx.timing.handshake_timeout,
                ));
                Ok(PendingPairing {
                    peer: confirmed.peer,
                    conn: confirmed.conn,
                    send: Some(confirmed.send),
                    endpoint: self.endpoint.clone(),
                    expires_at: self.expires_at,
                    refuser,
                    discovery: self.discovery.take(),
                    resolved: false,
                })
            }
            Err(error) => {
                // The code is dead: close whatever is still connected.
                self.endpoint.close(
                    CloseCode::Expired.varint(),
                    b"the pairing code is no longer valid",
                );
                if let Some(discovery) = self.discovery.take() {
                    discovery.shutdown(Duration::from_secs(1)).await;
                }
                let _ = tokio::time::timeout(Duration::from_millis(200), self.endpoint.wait_idle())
                    .await;
                Err(error)
            }
        }
    }
}

impl Drop for PairingHost {
    fn drop(&mut self) {
        if let Some(discovery) = self.discovery.take() {
            discovery.stop_now();
        }
    }
}

/// A joiner that proved it knows the code, waiting for the user on the
/// host to say yes or no.
///
/// Ask "Krish's PC (Windows) entered the code. Pair with it? Answer y only
/// if Krish's PC says it is waiting for you. [y/N]", giving up when
/// [`PendingPairing::remaining`] runs out. Dropping it declines.
pub struct PendingPairing {
    peer: PairedPeer,
    conn: quinn::Connection,
    send: Option<SendStream>,
    endpoint: quinn::Endpoint,
    expires_at: Instant,
    refuser: tokio::task::JoinHandle<()>,
    discovery: Option<Discovery>,
    resolved: bool,
}

impl fmt::Debug for PendingPairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingPairing")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl PendingPairing {
    /// The joiner: name, id, OS and fingerprint.
    pub fn peer(&self) -> &PairedPeer {
        &self.peer
    }

    /// How long the user has left to answer.
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// The user said yes: calls `save` with the peer (save it to the
    /// config), then tells the joiner, which saves too. Returns the peer.
    ///
    /// # Errors
    ///
    /// [`PairError::Expired`] if the code ran out first (nothing is saved),
    /// [`PairError::Connection`] if the joiner already left (nothing is
    /// saved), [`PairError::Save`] if `save` failed (the joiner is told no),
    /// and [`PairError::Connection`] if the joiner could not be told after
    /// saving: then run `hop pair` again on both laptops; pairing again
    /// replaces this entry.
    pub async fn accept<E: fmt::Display>(
        mut self,
        save: impl FnOnce(&PairedPeer) -> Result<(), E>,
    ) -> Result<PairedPeer, PairError> {
        self.resolved = true;
        let result = self.accept_inner(save).await;
        self.finish().await;
        result
    }

    async fn accept_inner<E: fmt::Display>(
        &mut self,
        save: impl FnOnce(&PairedPeer) -> Result<(), E>,
    ) -> Result<PairedPeer, PairError> {
        if Instant::now() >= self.expires_at {
            self.close(CloseCode::Expired, "the pairing code expired");
            return Err(PairError::Expired);
        }
        if let Some(reason) = self.conn.close_reason() {
            return Err(PairError::Connection(display_text(&reason.to_string(), 80)));
        }
        if let Err(error) = save(&self.peer) {
            self.close(CloseCode::Declined, "could not save the pairing");
            return Err(PairError::Save(display_text(&error.to_string(), 200)));
        }
        let Some(mut send) = self.send.take() else {
            return Err(PairError::Connection("no stream".to_owned()));
        };
        wire::write_frame(&mut send, Kind::Accepted, &[])
            .await
            .map_err(|failure| failure.error)?;
        let _ = send.finish();
        // Wait until the joiner has the message: its QUIC stack acknowledges
        // the whole stream, or it closes normally (code 0), which it does
        // only once it has read "accepted". Any other close, or a stream the
        // joiner stopped reading, means it may not have the message.
        let delivered = match tokio::time::timeout(Duration::from_secs(5), send.stopped()).await {
            Ok(Ok(None)) => true,
            Ok(Err(quinn::StoppedError::ConnectionLost(
                quinn::ConnectionError::ApplicationClosed(close),
            ))) => close.error_code.into_inner() == 0,
            _ => false,
        };
        if !delivered {
            return Err(PairError::Connection(
                "this laptop saved the pairing, but the other laptop may not have received the \
                 confirmation; if it reports a failure, run hop pair again on both laptops"
                    .to_owned(),
            ));
        }
        self.close(CloseCode::Shutdown, "paired");
        Ok(self.peer.clone())
    }

    /// The user said no (or did not answer): tells the joiner, and saves
    /// nothing.
    pub async fn decline(mut self) {
        self.resolved = true;
        self.close(CloseCode::Declined, "declined");
        self.finish().await;
    }

    fn close(&self, code: CloseCode, reason: &str) {
        self.conn.close(code.varint(), reason.as_bytes());
    }

    /// Stops refusing joiners, closes the listener and withdraws the
    /// advertisement.
    async fn finish(&mut self) {
        self.refuser.abort();
        self.endpoint.close(
            CloseCode::Expired.varint(),
            b"the pairing code is no longer valid",
        );
        let _ = tokio::time::timeout(Duration::from_millis(500), self.endpoint.wait_idle()).await;
        if let Some(discovery) = self.discovery.take() {
            discovery.shutdown(Duration::from_secs(1)).await;
        }
    }
}

impl Drop for PendingPairing {
    fn drop(&mut self) {
        if !self.resolved {
            self.close(CloseCode::Declined, "declined");
        }
        self.refuser.abort();
        self.endpoint.close(
            CloseCode::Expired.varint(),
            b"the pairing code is no longer valid",
        );
        if let Some(discovery) = self.discovery.take() {
            discovery.stop_now();
        }
    }
}

/// Joins the laptop showing the code: tries `addresses` in order until one
/// answers, runs the exchange once, waits for the user there to confirm,
/// and returns the host for the caller to save.
///
/// `on_confirmed` runs once the host proved it knows the code, so the
/// caller can show "Waiting for you to confirm on Krish's MacBook."
///
/// Several addresses may be tried before the exchange starts, but once
/// this side has sent its key confirmation it never tries another: a fake
/// pairing advertisement must not be able to test one guess per retry. The
/// code is consumed for the same reason.
///
/// Pass the host's pairing address (`--address`, default port
/// [`DEFAULT_PAIRING_PORT`]), or what [`crate::find_pairing_hosts`] found
/// when exactly one laptop advertises pairing.
///
/// # Errors
///
/// See [`PairError`]: notably [`PairError::WrongCode`],
/// [`PairError::Declined`], [`PairError::Expired`], [`PairError::Busy`]
/// and [`PairError::Unreachable`].
pub async fn pair_join(
    identity: Arc<Identity>,
    me: PairInfo,
    code: PairingCode,
    addresses: &[SocketAddr],
    timing: PairingTiming,
    on_confirmed: impl FnOnce(&PairedPeer),
) -> Result<PairedPeer, PairError> {
    if addresses.is_empty() {
        return Err(PairError::NoAddress);
    }
    let me = PairInfo {
        name: display_text(&me.name, MAX_DISPLAY_CHARS),
        os: me.os,
    };
    let setup = |err: String| PairError::Setup(err);
    let transport = Arc::new(pairing_transport(0).build().map_err(setup)?);
    let mut last_error = PairError::Unreachable;
    for &address in addresses {
        let tls = tls::client_config(
            &identity,
            Accept::AnyEd25519,
            tls::ALPN_PAIRING,
            Arc::new(VerifierStats::default()),
        )
        .and_then(|tls| tls::quic_client(tls, transport.clone()))
        .map_err(|err| setup(err.to_string()))?;
        // A client socket on loopback for a loopback target (tests), on all
        // interfaces otherwise.
        let local = if address.ip().is_loopback() {
            SocketAddr::from(([127, 0, 0, 1], 0))
        } else {
            SocketAddr::from(([0, 0, 0, 0], 0))
        };
        let endpoint = bind_endpoint(local, None)?;
        let conn = match connect(&endpoint, tls, address, timing.handshake_timeout).await {
            Ok(conn) => conn,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let Some(host) = tls::peer_fingerprint(&conn) else {
            conn.close(CloseCode::Protocol.varint(), b"unexpected certificate");
            last_error = PairError::Protocol("unexpected certificate".to_owned());
            continue;
        };
        if !tls::negotiated_alpn_is(&conn, tls::ALPN_PAIRING) {
            conn.close(CloseCode::Protocol.varint(), b"unexpected protocol");
            last_error = PairError::Protocol("not a Hop pairing listener".to_owned());
            continue;
        }
        if host == identity.fingerprint() {
            conn.close(CloseCode::Protocol.varint(), b"this is the same device");
            last_error = PairError::SelfPairing;
            continue;
        }
        let mut confirmation_sent = false;
        // The stream lives out here, so that a failure closes the
        // connection before the stream is dropped (see `host_run`).
        let mut streams = None;
        let exchange = joiner_exchange(
            &conn,
            &identity,
            host,
            &code,
            &me,
            &mut confirmation_sent,
            &mut streams,
        );
        let result = tokio::time::timeout(timing.exchange_timeout, exchange).await;
        let (peer, (send, recv)) = match (result, streams.take()) {
            (Ok(Ok(peer)), Some(streams)) => (peer, streams),
            (Ok(Err(failure)), streams) => {
                let failure = failure.explained(&conn, CLOSE_WAIT).await;
                if let Some(code) = failure.close {
                    conn.close(code.varint(), code.name().as_bytes());
                }
                drop(streams);
                if confirmation_sent {
                    return Err(failure.error);
                }
                last_error = failure.error;
                continue;
            }
            (_, streams) => {
                conn.close(CloseCode::Protocol.varint(), b"too slow");
                drop(streams);
                let error = PairError::Connection("the other laptop stopped answering".to_owned());
                if confirmation_sent {
                    return Err(error);
                }
                last_error = error;
                continue;
            }
        };
        // Nothing more to send: the stream ends here, as the host expects.
        drop(send);
        on_confirmed(&peer);
        let mut recv = recv;
        // Declared after `recv`, so it is dropped first: a join dropped while
        // it waits (the app's Cancel) closes with EXPIRED before the stream
        // could send anything.
        let mut stop_waiting = CloseIfDropped(Some(conn.clone()));
        let verdict = wait_for_verdict(&conn, &mut recv, timing.verdict_timeout).await;
        stop_waiting.disarm();
        // The host takes a normal close (SHUTDOWN, 0) as proof that its
        // "accepted" arrived, so it is sent only then. Otherwise a host
        // whose user said yes just as this side stopped waiting would report
        // a pairing that this side never saved.
        match &verdict {
            Ok(()) => conn.close(CloseCode::Shutdown.varint(), b"paired"),
            Err(failure) => {
                let code = failure.close.unwrap_or(CloseCode::Expired);
                conn.close(code.varint(), b"not paired");
            }
        }
        // Dropped only after the close, so the stream sends no STOP_SENDING
        // that the host could mistake for a delivered "accepted".
        drop(recv);
        let _ = tokio::time::timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
        return verdict.map(|()| peer).map_err(|failure| failure.error);
    }
    Err(last_error)
}

/// Closes a joiner's connection with `EXPIRED` when the join is dropped
/// while it waits for the host's answer. quinn would otherwise close it by
/// itself with code 0, which the host takes as proof that its "accepted"
/// arrived: a host whose user said yes at that moment would then report a
/// pairing this side never saved.
struct CloseIfDropped(Option<quinn::Connection>);

impl CloseIfDropped {
    /// The join closes the connection itself from here on.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CloseIfDropped {
    fn drop(&mut self) {
        if let Some(conn) = self.0.take() {
            conn.close(CloseCode::Expired.varint(), b"stopped waiting");
        }
    }
}

/// The joiner's steps 2 and 3 on one connection. The stream goes into
/// `streams`, which outlives the exchange.
async fn joiner_exchange(
    conn: &quinn::Connection,
    identity: &Identity,
    host: Fingerprint,
    code: &PairingCode,
    me: &PairInfo,
    confirmation_sent: &mut bool,
    streams: &mut Option<(SendStream, RecvStream)>,
) -> Result<PairedPeer, Failure> {
    let (send, recv) = streams.insert(conn.open_bi().await.map_err(Failure::connection)?);
    let joiner = identity.fingerprint();
    let (spake, ours) = Spake2::<Ed25519Group>::start_a(
        &code.password(),
        &SpakeIdentity::new(joiner.as_bytes()),
        &SpakeIdentity::new(host.as_bytes()),
    );
    wire::write_frame(send, Kind::Spake, &ours).await?;
    let theirs = wire::read_frame(recv, Kind::Spake).await?;
    let key = Zeroizing::new(spake.finish(&theirs).map_err(|_| Failure::wrong_code())?);
    let binding = wire::channel_binding(conn)?;
    let mac = wire::confirmation(&key, wire::LABEL_JOINER, &binding);
    *confirmation_sent = true;
    wire::write_frame(send, Kind::Confirm, &wire::encode_confirm(&mac, me)).await?;
    let reply = wire::read_frame(recv, Kind::Confirm).await?;
    let (their_mac, os, name) = wire::parse_confirm(&reply)?;
    if !wire::verify_confirmation(&key, wire::LABEL_HOST, &binding, &their_mac) {
        return Err(Failure::wrong_code());
    }
    Ok(PairedPeer {
        name: display_text(&name, MAX_DISPLAY_CHARS),
        id: host.device_id(),
        os,
        fingerprint: host,
    })
}

/// Waits for the host's "accepted", or its close. No answer in time counts
/// as no, and the joiner then closes with `EXPIRED`.
async fn wait_for_verdict(
    conn: &quinn::Connection,
    recv: &mut RecvStream,
    timeout: Duration,
) -> Result<(), Failure> {
    match tokio::time::timeout(timeout, wire::read_frame(recv, Kind::Accepted)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(failure)) => Err(failure.explained(conn, CLOSE_WAIT).await),
        Err(_) => Err(Failure {
            error: PairError::Declined,
            close: Some(CloseCode::Expired),
            ended_early: false,
        }),
    }
}

/// How a host's run on one connection ended.
enum RunOutcome {
    Confirmed(Confirmed),
    /// `counted` when this host had sent its SPAKE2 message.
    Failed {
        counted: bool,
    },
}

/// A joiner whose confirmation checked out.
struct Confirmed {
    peer: PairedPeer,
    conn: quinn::Connection,
    send: SendStream,
}

async fn poll_run(
    run: &mut Option<Pin<Box<dyn Future<Output = RunOutcome> + Send>>>,
) -> RunOutcome {
    match run {
        Some(run) => run.await,
        None => std::future::pending().await,
    }
}

/// One host run: the TLS handshake, then steps 2 and 3 within the
/// exchange timeout.
async fn host_run(incoming: quinn::Incoming, ctx: Arc<HostContext>) -> RunOutcome {
    let timing = &ctx.timing;
    let Ok(connecting) = incoming.accept() else {
        return RunOutcome::Failed { counted: false };
    };
    let conn = match tokio::time::timeout(timing.handshake_timeout, connecting).await {
        Ok(Ok(conn)) => conn,
        _ => {
            if let Some(suppressed) = ratelimit::allow("pairing handshake failed") {
                tracing::warn!(
                    suppressed,
                    "a pairing connection failed during the handshake"
                );
            }
            return RunOutcome::Failed { counted: false };
        }
    };
    let joiner = match tls::peer_fingerprint(&conn) {
        Some(joiner) if tls::negotiated_alpn_is(&conn, tls::ALPN_PAIRING) => joiner,
        _ => {
            conn.close(
                CloseCode::Protocol.varint(),
                b"unexpected certificate or protocol",
            );
            return RunOutcome::Failed { counted: false };
        }
    };
    if joiner == ctx.identity.fingerprint() {
        conn.close(CloseCode::Protocol.varint(), b"this is the same device");
        return RunOutcome::Failed { counted: false };
    }
    let mut spake_sent = false;
    // The stream lives out here, outside the timed exchange, so that a run
    // that fails closes the connection while the stream is still open.
    // Dropping the stream first would finish it, and that end could reach
    // the joiner before the close code saying why (`WRONG_CODE`): the
    // joiner would then report a broken stream instead of a wrong code.
    // Once the connection is closed, dropping the stream sends nothing.
    let mut streams = None;
    let exchange = host_exchange(&conn, &ctx, joiner, &mut spake_sent, &mut streams);
    let close: Option<(CloseCode, &[u8])> =
        match tokio::time::timeout(timing.exchange_timeout, exchange).await {
            Ok(Ok(peer)) => match streams.take() {
                Some((send, _recv)) => {
                    return RunOutcome::Confirmed(Confirmed { peer, conn, send });
                }
                None => Some((CloseCode::Protocol, b"no stream")),
            },
            Ok(Err(failure)) => failure.close.map(|code| (code, code.name().as_bytes())),
            Err(_) => Some((CloseCode::Protocol, b"too slow")),
        };
    if let Some((code, reason)) = close {
        conn.close(code.varint(), reason);
    }
    drop(streams);
    RunOutcome::Failed {
        counted: spake_sent,
    }
}

/// The host's steps 2 and 3 on one connection. The stream goes into
/// `streams`, which outlives the exchange.
async fn host_exchange(
    conn: &quinn::Connection,
    ctx: &HostContext,
    joiner: Fingerprint,
    spake_sent: &mut bool,
    streams: &mut Option<(SendStream, RecvStream)>,
) -> Result<PairedPeer, Failure> {
    let (send, recv) = streams.insert(conn.accept_bi().await.map_err(Failure::connection)?);
    let theirs = wire::read_frame(recv, Kind::Spake).await?;
    let host = ctx.identity.fingerprint();
    let (spake, ours) = Spake2::<Ed25519Group>::start_b(
        &ctx.code.password(),
        &SpakeIdentity::new(joiner.as_bytes()),
        &SpakeIdentity::new(host.as_bytes()),
    );
    // From here on this run counts as an attempt unless it succeeds: the
    // joiner could test a guess against this message.
    *spake_sent = true;
    wire::write_frame(send, Kind::Spake, &ours).await?;
    let key = Zeroizing::new(spake.finish(&theirs).map_err(|_| Failure::wrong_code())?);
    let binding = wire::channel_binding(conn)?;
    let confirm = wire::read_frame(recv, Kind::Confirm).await?;
    let (their_mac, os, name) = wire::parse_confirm(&confirm)?;
    // Nothing the joiner sent is trusted until its confirmation checks out.
    if !wire::verify_confirmation(&key, wire::LABEL_JOINER, &binding, &their_mac) {
        return Err(Failure::wrong_code());
    }
    let mac = wire::confirmation(&key, wire::LABEL_HOST, &binding);
    wire::write_frame(send, Kind::Confirm, &wire::encode_confirm(&mac, &ctx.me)).await?;
    Ok(PairedPeer {
        name: display_text(&name, MAX_DISPLAY_CHARS),
        id: joiner.device_id(),
        os,
        fingerprint: joiner,
    })
}

/// Closes a joiner that arrived during a run with `BUSY`, after its
/// handshake (a close code needs a connection). At most four at a time;
/// more are refused outright.
fn refuse_busy(incoming: quinn::Incoming, permits: &Arc<Semaphore>, handshake_timeout: Duration) {
    let Ok(permit) = permits.clone().try_acquire_owned() else {
        incoming.refuse();
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        if let Ok(connecting) = incoming.accept()
            && let Ok(Ok(conn)) = tokio::time::timeout(handshake_timeout, connecting).await
        {
            conn.close(
                CloseCode::Busy.varint(),
                b"busy pairing with another device",
            );
        }
    });
}

/// Refuses every joiner with `BUSY` while the user decides.
async fn refuse_all(
    endpoint: quinn::Endpoint,
    permits: Arc<Semaphore>,
    handshake_timeout: Duration,
) {
    while let Some(incoming) = endpoint.accept().await {
        refuse_busy(incoming, &permits, handshake_timeout);
    }
}

async fn connect(
    endpoint: &quinn::Endpoint,
    tls: quinn::ClientConfig,
    address: SocketAddr,
    timeout: Duration,
) -> Result<quinn::Connection, PairError> {
    let connecting = endpoint
        .connect_with(tls, address, tls::SERVER_NAME)
        .map_err(|_| PairError::Unreachable)?;
    match tokio::time::timeout(timeout, connecting).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(quinn::ConnectionError::ApplicationClosed(close))) => {
            Err(wire::error_for_close(close.error_code.into_inner()))
        }
        _ => Err(PairError::Unreachable),
    }
}

fn bind_endpoint(
    address: SocketAddr,
    server: Option<quinn::ServerConfig>,
) -> Result<quinn::Endpoint, PairError> {
    let bind_error = |source| PairError::Bind {
        addr: address,
        source,
    };
    let socket = std::net::UdpSocket::bind(address).map_err(bind_error)?;
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        server,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .map_err(bind_error)
}

/// The transport limits of a pairing connection: one stream, small
/// windows (each message is at most 1 KiB), no datagrams.
fn pairing_transport(peer_bidi_streams: u32) -> TransportParams {
    TransportParams {
        peer_bidi_streams,
        stream_window: 16 * 1024,
        connection_window: 16 * 1024,
        send_window: 16 * 1024,
        datagram_receive: None,
        datagram_send: 0,
        keep_alive: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(5),
    }
}

#[cfg(test)]
mod tests;
