//! One session connection: the checks after the TLS handshake, the `Hello`
//! exchange, the duplicate rule, and then reading and writing until it
//! closes.

use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use hop_proto::{
    Decoded, FrameDecoder, Hello, KIND_HELLO, Message, PROTO_MAJOR, PROTO_MINOR, ProtoError, caps,
    decode_datagram, encode_frame, negotiate,
};
use hop_types::{CoreMessage, DeviceId};
use quinn::{RecvStream, SendStream};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Current, Shared};
use crate::events::{DownReason, Offer, PeerUp};
use crate::sanitize::{MAX_DISPLAY_CHARS, display_bytes, display_text};
use crate::{CloseCode, ratelimit, tls};

/// The largest `Hello` accepted (docs/M2-DESIGN.md, "Limits").
const MAX_HELLO_LEN: u32 = 64 * 1024;

/// How much the control-stream reader takes from quinn at a time.
const READ_CHUNK: usize = 64 * 1024;

/// Longest close reason this side sends or logs.
const MAX_REASON_CHARS: usize = 80;

/// How long to wait for a peer that finished its control stream to close
/// the connection itself.
const PEER_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Which side dialled a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    /// This device dialled it.
    Outgoing,
    /// The peer dialled it.
    Incoming,
}

/// A QUIC connection plus the code this side closed it with, so the close
/// can be reported accurately later.
pub(crate) struct Link {
    pub(crate) conn: quinn::Connection,
    closed_with: OnceLock<CloseCode>,
}

impl Link {
    fn new(conn: quinn::Connection) -> Link {
        Link {
            conn,
            closed_with: OnceLock::new(),
        }
    }

    /// Closes the connection, unless it is already closed. The first close
    /// wins: a connection replaced as a duplicate stays "duplicate" even if
    /// its tasks close it again, and one the peer closed keeps the peer's
    /// reason (quinn would otherwise record a later local close instead).
    pub(crate) fn close(&self, code: CloseCode, reason: &str) {
        if self.conn.close_reason().is_some() {
            return;
        }
        let _ = self.closed_with.set(code);
        self.conn.close(code.varint(), reason.as_bytes());
    }

    /// Why the connection closed, once it has.
    fn down_reason(&self) -> DownReason {
        match self.conn.close_reason() {
            Some(quinn::ConnectionError::LocallyClosed) | None => DownReason::Local(
                self.closed_with
                    .get()
                    .copied()
                    .unwrap_or(CloseCode::Shutdown),
            ),
            Some(quinn::ConnectionError::ApplicationClosed(close)) => DownReason::Remote {
                code: close.error_code.into_inner(),
                reason: display_bytes(&close.reason, MAX_REASON_CHARS),
            },
            Some(quinn::ConnectionError::TimedOut) => DownReason::TimedOut,
            Some(other) => {
                DownReason::Transport(display_text(&other.to_string(), MAX_REASON_CHARS))
            }
        }
    }
}

/// Why setting up a connection failed.
#[derive(Debug)]
pub(crate) enum SetupError {
    /// This side closed it with this code.
    Closed(CloseCode),
    /// The peer closed it with this code.
    Remote(u64),
    /// It failed below Hop.
    Lost,
}

impl SetupError {
    pub(crate) fn is_incompatible(&self) -> bool {
        matches!(self, SetupError::Closed(CloseCode::Incompatible))
            || matches!(self, SetupError::Remote(code) if *code == u64::from(CloseCode::Incompatible.code()))
    }

    pub(crate) fn is_duplicate(&self) -> bool {
        matches!(self, SetupError::Closed(CloseCode::Duplicate))
            || matches!(self, SetupError::Remote(code) if *code == u64::from(CloseCode::Duplicate.code()))
    }
}

/// Takes a connection whose TLS handshake just finished through the
/// checks, the `Hello` exchange and the duplicate rule, and on success
/// announces it and spawns its reader and writer.
///
/// `expected` is the peer being dialled (outgoing connections only).
pub(crate) async fn establish(
    shared: Arc<Shared>,
    conn: quinn::Connection,
    direction: Direction,
    expected: Option<DeviceId>,
) -> Result<(), SetupError> {
    let link = Arc::new(Link::new(conn));
    let Some(alive) = shared.alive_token() else {
        link.close(CloseCode::Shutdown, "shutting down");
        return Err(SetupError::Closed(CloseCode::Shutdown));
    };

    // 1. Who is it? The verifier already checked the pin; check the
    //    finished handshake again before reading or sending anything.
    let fingerprint = tls::peer_fingerprint(&link.conn);
    let peer = fingerprint.and_then(|fingerprint| shared.pins_lookup(&fingerprint));
    let peer = match (peer, expected) {
        (Some(peer), None) => peer,
        (Some(peer), Some(expected)) if peer == expected => peer,
        _ => return Err(refuse(&link, CloseCode::Protocol, "unexpected certificate")),
    };
    if !tls::negotiated_alpn_is(&link.conn, tls::ALPN_SESSION) {
        return Err(refuse(&link, CloseCode::Protocol, "unexpected protocol"));
    }
    let conn_id = shared.next_conn_id();

    // Datagrams are read from the start, so any that arrive before the
    // announcement are dropped rather than delivered late.
    tokio::spawn(read_datagrams(
        shared.clone(),
        link.clone(),
        peer.clone(),
        conn_id,
    ));

    // 2. The Hello exchange. The dialler writes first, because quinn
    //    announces a stream to the peer only once data is written. The
    //    listener replies only after the duplicate rule, below, so a
    //    connection it rejects is never announced by the dialler either.
    let our_hello = shared.our_hello();
    let our_frame = match encode_frame(&Message::Hello(our_hello.clone())) {
        Ok(frame) => Bytes::from(frame),
        Err(_) => return Err(refuse(&link, CloseCode::Protocol, "cannot encode Hello")),
    };
    let exchange = hello_exchange(&link, direction, &our_frame);
    let (send, recv, hello) =
        match tokio::time::timeout(shared.timing.hello_timeout, exchange).await {
            Ok(Ok(exchanged)) => exchanged,
            Ok(Err(HelloError::Lost(error))) => return Err(lost(&link, error)),
            Ok(Err(HelloError::Incompatible(versions))) => {
                return Err(incompatible(&shared, &link, &peer, &versions));
            }
            Ok(Err(HelloError::Bad(why))) => return Err(refuse(&link, CloseCode::Protocol, why)),
            Err(_) => return Err(refuse(&link, CloseCode::Protocol, "no Hello in time")),
        };

    // 3. The Hello must name the device the certificate gives, and the two
    //    builds must agree on a version and share `input`.
    if hello.device_id != peer {
        return Err(refuse(
            &link,
            CloseCode::Protocol,
            "Hello names another device",
        ));
    }
    let negotiated = match negotiate(&our_hello, &hello) {
        Ok(negotiated) if negotiated.capability_version(caps::INPUT).is_some() => negotiated,
        Ok(_) => {
            let why = "the peer does not support input sharing";
            return Err(incompatible(&shared, &link, &peer, why));
        }
        Err(_) => {
            let versions = format!(
                "this device speaks {PROTO_MAJOR}.{PROTO_MINOR}, the peer {}.{}",
                hello.proto_major, hello.proto_minor
            );
            return Err(incompatible(&shared, &link, &peer, &versions));
        }
    };
    let up = PeerUp {
        peer: peer.clone(),
        name: display_text(&hello.device_name, MAX_DISPLAY_CHARS),
        app_version: display_text(&hello.app_version, MAX_DISPLAY_CHARS),
        os: hello.os,
        displays: hello.displays,
        negotiated,
        remote: link.conn.remote_address(),
        dialled_by_us: direction == Direction::Outgoing,
    };
    let (name, remote) = (up.name.clone(), up.remote);

    // 4. One connection per pair.
    let outgoing = decide_and_announce(&shared, &link, conn_id, direction, &our_frame, up).await?;
    shared.queue.wake();
    tracing::info!(
        peer = %name,
        id = %peer,
        address = %remote,
        dialled_by_us = direction == Direction::Outgoing,
        "connected"
    );

    // 5. Read and write until the connection closes.
    tokio::spawn(run(
        shared, link, peer, conn_id, send, recv, outgoing, alive,
    ));
    Ok(())
}

/// Why the `Hello` exchange failed.
enum HelloError {
    /// The connection failed.
    Lost(Option<quinn::ConnectionError>),
    /// The peer speaks another major version.
    Incompatible(String),
    /// The peer broke the protocol.
    Bad(&'static str),
}

/// Opens or accepts the control stream and reads the peer's `Hello`. The
/// dialler also writes its own first.
async fn hello_exchange(
    link: &Link,
    direction: Direction,
    our_frame: &Bytes,
) -> Result<(SendStream, RecvStream, Hello), HelloError> {
    let (mut send, mut recv) = match direction {
        Direction::Outgoing => link.conn.open_bi().await,
        Direction::Incoming => link.conn.accept_bi().await,
    }
    .map_err(|error| HelloError::Lost(Some(error)))?;
    if direction == Direction::Outgoing {
        send.write_all(our_frame)
            .await
            .map_err(|_| HelloError::Lost(None))?;
    }
    let hello = read_hello(&mut recv).await?;
    Ok((send, recv, hello))
}

/// Reads exactly one frame, which must be a `Hello` of at most 64 KiB. The
/// size is checked from the header, before any of the body is buffered.
async fn read_hello(recv: &mut RecvStream) -> Result<Hello, HelloError> {
    let mut header = [0u8; 6];
    recv.read_exact(&mut header).await.map_err(read_error)?;
    let [k0, k1, l0, l1, l2, l3] = header;
    if u16::from_be_bytes([k0, k1]) != KIND_HELLO {
        return Err(HelloError::Bad("the first frame is not a Hello"));
    }
    let len = u32::from_be_bytes([l0, l1, l2, l3]);
    if len > MAX_HELLO_LEN {
        return Err(HelloError::Bad("the Hello is too large"));
    }
    let mut body = vec![0u8; usize::try_from(len).unwrap_or(0)];
    recv.read_exact(&mut body).await.map_err(read_error)?;
    let mut decoder = FrameDecoder::new();
    let pushed = decoder.push(&header).and_then(|()| decoder.push(&body));
    match (pushed, decoder.pop()) {
        (Ok(()), Some(Ok(Decoded::Message(Message::Hello(hello))))) => Ok(hello),
        (
            Ok(()),
            Some(Err(ProtoError::Incompatible {
                remote_major,
                remote_minor,
                ..
            })),
        ) => Err(HelloError::Incompatible(format!(
            "this device speaks {PROTO_MAJOR}.{PROTO_MINOR}, the peer {remote_major}.{remote_minor}"
        ))),
        _ => Err(HelloError::Bad("the Hello does not decode")),
    }
}

fn read_error(error: quinn::ReadExactError) -> HelloError {
    match error {
        quinn::ReadExactError::FinishedEarly(_) => {
            HelloError::Bad("the stream ended inside the Hello")
        }
        quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(error)) => {
            HelloError::Lost(Some(error))
        }
        quinn::ReadExactError::ReadError(_) => HelloError::Bad("the control stream was reset"),
    }
}

/// Closes a connection that broke the rules and returns the matching
/// error. Rate-limited, because a misbehaving paired device could repeat it.
fn refuse(link: &Link, code: CloseCode, why: &'static str) -> SetupError {
    if let Some(suppressed) = ratelimit::allow("connection refused") {
        tracing::warn!(
            address = %link.conn.remote_address(),
            code = %code,
            suppressed,
            "closing a connection: {why}"
        );
    }
    link.close(code, why);
    SetupError::Closed(code)
}

/// The error for a connection that failed during setup.
fn lost(link: &Link, error: Option<quinn::ConnectionError>) -> SetupError {
    match error.or_else(|| link.conn.close_reason()) {
        Some(quinn::ConnectionError::ApplicationClosed(close)) => {
            SetupError::Remote(close.error_code.into_inner())
        }
        _ => SetupError::Lost,
    }
}

/// Logs that `peer` runs an incompatible build, once per peer until a good
/// connection is made (retries come once a minute).
pub(crate) fn log_incompatible_once(shared: &Shared, peer: &DeviceId, why: &str) {
    let first = {
        let mut table = shared.lock_table();
        table.peers.get_mut(peer).is_some_and(|slot| {
            let first = !slot.incompatible_logged;
            slot.incompatible_logged = true;
            first
        })
    };
    if first {
        tracing::warn!(
            id = %peer,
            "the other device runs a Hop version this one cannot talk to ({}); update Hop on both devices",
            display_text(why, MAX_REASON_CHARS)
        );
    }
}

/// Closes a connection to an incompatible build, logging it once.
fn incompatible(shared: &Shared, link: &Link, peer: &DeviceId, why: &str) -> SetupError {
    log_incompatible_once(shared, peer, why);
    let reason = display_text(&format!("incompatible: {why}"), MAX_REASON_CHARS);
    link.close(CloseCode::Incompatible, &reason);
    SetupError::Closed(CloseCode::Incompatible)
}

/// The duplicate rule's verdict on a new connection.
enum Verdict {
    Accept,
    Reject,
    /// Hold it: this device's own dial to the peer, which would win, is
    /// still in progress.
    Wait,
}

/// Applies the duplicate rule (docs/M2-DESIGN.md, "One connection per
/// pair") and, if the new connection wins, makes it the peer's current
/// one and announces it.
///
/// Both devices decide from the pinned ids alone, so they keep the same
/// connection: the one dialled by the lower id. Between two connections
/// dialled by the same device, the newer wins, since that device dials only
/// when it has given up on the older one.
///
/// The listener decides before it writes its `Hello`, so a connection it
/// rejects never reaches the dialler's core. While its own dial to the
/// same peer is still in progress and would win, the listener waits for it
/// (at most `duplicate_grace`), so its core never hears about the
/// connection that is about to lose.
async fn decide_and_announce(
    shared: &Arc<Shared>,
    link: &Arc<Link>,
    conn_id: u64,
    direction: Direction,
    our_frame: &Bytes,
    up: PeerUp,
) -> Result<mpsc::Receiver<Bytes>, SetupError> {
    let peer = up.peer.clone();
    let grace_end = Instant::now() + shared.timing.duplicate_grace;
    let mut changes = shared.subscribe_changes();
    let mut up = Some(up);
    loop {
        changes.borrow_and_update();
        {
            let mut table = shared.lock_table();
            if table.closed {
                drop(table);
                link.close(CloseCode::Shutdown, "shutting down");
                return Err(SetupError::Closed(CloseCode::Shutdown));
            }
            let Some(slot) = table.peers.get_mut(&peer) else {
                drop(table);
                return Err(refuse(link, CloseCode::Protocol, "not a paired device"));
            };
            let may_wait = Instant::now() < grace_end;
            match verdict(&shared.me, &peer, slot, direction, may_wait) {
                Verdict::Wait => {}
                Verdict::Reject => {
                    drop(table);
                    tracing::debug!(id = %peer, "keeping the other connection to this device");
                    link.close(CloseCode::Duplicate, "another connection is kept");
                    return Err(SetupError::Closed(CloseCode::Duplicate));
                }
                Verdict::Accept => {
                    if link.conn.close_reason().is_some() {
                        // It died while it waited: nothing to announce.
                        drop(table);
                        return Err(lost(link, None));
                    }
                    let (outgoing, outgoing_rx) = mpsc::channel(shared.limits.send_capacity.max(1));
                    if direction == Direction::Incoming
                        && outgoing.try_send(our_frame.clone()).is_err()
                    {
                        // Cannot happen: the queue is new and has room.
                        drop(table);
                        return Err(refuse(link, CloseCode::Protocol, "cannot queue Hello"));
                    }
                    let Some(up) = up.take() else {
                        drop(table);
                        return Err(refuse(link, CloseCode::Protocol, "announced twice"));
                    };
                    let negotiated = up.negotiated.clone();
                    if !shared.queue.announce(conn_id, up) {
                        drop(table);
                        link.close(CloseCode::Overloaded, "the agent is not keeping up");
                        return Err(SetupError::Closed(CloseCode::Overloaded));
                    }
                    let replaced = slot.current.replace(Current {
                        conn_id,
                        link: link.clone(),
                        outgoing,
                        dialled_by_us: direction == Direction::Outgoing,
                        negotiated,
                    });
                    slot.incompatible_logged = false;
                    drop(table);
                    if let Some(old) = replaced {
                        tracing::debug!(id = %peer, "a newer connection replaces the current one");
                        old.link
                            .close(CloseCode::Duplicate, "replaced by another connection");
                    }
                    shared.bump();
                    return Ok(outgoing_rx);
                }
            }
        }
        tokio::select! {
            _ = changes.changed() => {}
            _ = tokio::time::sleep_until(grace_end) => {}
        }
    }
}

/// The duplicate rule for a new connection to `peer` arriving in
/// `direction`.
fn verdict(
    me: &DeviceId,
    peer: &DeviceId,
    slot: &super::Slot,
    direction: Direction,
    may_wait: bool,
) -> Verdict {
    if let Some(current) = &slot.current {
        let current_direction = if current.dialled_by_us {
            Direction::Outgoing
        } else {
            Direction::Incoming
        };
        return if new_wins(me, peer, direction, current_direction) {
            Verdict::Accept
        } else {
            Verdict::Reject
        };
    }
    // Only an incoming connection can lose to this device's own dial in
    // progress, and only when this device has the lower id.
    if may_wait && direction == Direction::Incoming && slot.dialing && me < peer {
        Verdict::Wait
    } else {
        Verdict::Accept
    }
}

/// Whether a new connection in direction `new` beats the current one in
/// direction `current`.
pub(crate) fn new_wins(me: &DeviceId, peer: &DeviceId, new: Direction, current: Direction) -> bool {
    if new == current {
        return true;
    }
    match new {
        Direction::Outgoing => me < peer,
        Direction::Incoming => peer < me,
    }
}

/// How a running connection ended, as seen by its tasks.
enum End {
    /// Reading or writing the control stream failed: usually because the
    /// connection is closed, but also if the peer reset or stopped it.
    Lost,
    /// The peer finished its control stream: it is going away.
    PeerFinished,
    /// The send queue was dropped and everything queued was delivered.
    Flushed,
    /// A newer connection replaced this one.
    Replaced,
    /// The core's queue is full and a reliable message arrived.
    Overloaded,
    /// The peer broke the protocol.
    Protocol(&'static str),
}

/// Runs a connection after its announcement: reads the control stream,
/// writes the send queue, and reports the close if it is still the peer's
/// current connection.
#[allow(clippy::too_many_arguments)]
async fn run(
    shared: Arc<Shared>,
    link: Arc<Link>,
    peer: DeviceId,
    conn_id: u64,
    send: SendStream,
    recv: RecvStream,
    outgoing: mpsc::Receiver<Bytes>,
    alive: mpsc::Sender<()>,
) {
    let since = Instant::now();
    // No branch waits on `closed()`: when the peer closes, the reader
    // first hands over everything that already arrived (the `Release` a
    // quitting peer sent last) and only then reports the loss.
    let end = tokio::select! {
        end = read_control(&shared, &peer, conn_id, recv) => end,
        end = write_control(send, outgoing) => end,
    };
    match end {
        // Closed with `DUPLICATE` when it was replaced.
        End::Replaced => {}
        End::Lost => {
            // Normally the connection is already gone. A peer that reset or
            // stopped the control stream but kept the connection open must
            // not keep it: this side reports the peer down, while the peer
            // would still count the connection as current (refusing new
            // ones as duplicates) and keep-alives would hold it open
            // forever. A peer that is closing anyway gets a moment, so the
            // reason reported is its own.
            if link.conn.close_reason().is_none() {
                let _ = tokio::time::timeout(PEER_CLOSE_WAIT, link.conn.closed()).await;
                link.close(CloseCode::Protocol, "the control stream failed");
            }
        }
        End::PeerFinished => {
            // A quitting peer finishes its stream and then closes; wait a
            // moment for its close so the reason reported is its own.
            let _ = tokio::time::timeout(PEER_CLOSE_WAIT, link.conn.closed()).await;
            link.close(CloseCode::Shutdown, "goodbye");
        }
        End::Flushed => link.close(CloseCode::Shutdown, "goodbye"),
        End::Overloaded => link.close(CloseCode::Overloaded, "the agent is not keeping up"),
        End::Protocol(why) => {
            if let Some(suppressed) = ratelimit::allow("protocol error") {
                tracing::warn!(id = %peer, suppressed, "closing the connection: {why}");
            }
            link.close(CloseCode::Protocol, why);
        }
    }
    let reason = link.down_reason();
    let reported = {
        let mut table = shared.lock_table();
        if let Some(slot) = table.peers.get_mut(&peer)
            && slot
                .current
                .as_ref()
                .is_some_and(|current| current.conn_id == conn_id)
        {
            slot.current = None;
            slot.last_uptime = Some(since.elapsed());
        }
        shared.queue.disconnect(&peer, conn_id, reason.clone())
    };
    shared.bump();
    if reported {
        shared.queue.wake();
        tracing::info!(id = %peer, reason = %reason, "disconnected");
    }
    drop(alive);
}

/// Reads frames from the control stream and queues their messages for the
/// core.
async fn read_control(shared: &Shared, peer: &DeviceId, conn_id: u64, mut recv: RecvStream) -> End {
    let mut decoder = FrameDecoder::new();
    loop {
        let chunk = match recv.read_chunk(READ_CHUNK, true).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return End::PeerFinished,
            Err(_) => return End::Lost,
        };
        if decoder.push(&chunk.bytes).is_err() {
            return End::Protocol("a frame header declared an oversized body");
        }
        let mut queued = false;
        let end = loop {
            let Some(frame) = decoder.pop() else {
                break None;
            };
            let msg = match frame {
                Ok(Decoded::Message(Message::Core(msg))) => msg,
                Ok(Decoded::Message(Message::Hello(_))) => {
                    break Some(End::Protocol("a second Hello"));
                }
                // A kind from a newer build: skipped, as hop-proto's rules say.
                Ok(Decoded::Unknown { .. }) => continue,
                // Skipping a reliable message could lose a key-up, so a frame
                // that does not decode closes the connection.
                Err(_) => break Some(End::Protocol("a frame that does not decode")),
            };
            let reliable = match &msg {
                // Not negotiated in M2: dropped, and never logged.
                CoreMessage::ClipboardText { .. } => continue,
                CoreMessage::Motion { .. } => false,
                _ => true,
            };
            match shared.queue.offer(peer, conn_id, msg) {
                Offer::Queued => queued = true,
                // Motion is latest-wins: a dropped one is repaired by the next.
                Offer::Full if !reliable => {}
                Offer::Full => break Some(End::Overloaded),
                Offer::NotCurrent => break Some(End::Replaced),
                // Shutting down: keep reading so the writer can finish
                // delivering, but pass nothing on.
                Offer::Closed => {}
            }
        };
        if queued {
            shared.queue.wake();
        }
        if let Some(end) = end {
            return end;
        }
    }
}

/// Writes queued frames to the control stream, in order. When the queue's
/// sender is dropped (replacement or shutdown), finishes the stream and
/// waits until the peer has acknowledged everything.
async fn write_control(mut send: SendStream, mut outgoing: mpsc::Receiver<Bytes>) -> End {
    while let Some(frame) = outgoing.recv().await {
        if send.write_chunk(frame).await.is_err() {
            return End::Lost;
        }
    }
    let _ = send.finish();
    let _ = send.stopped().await;
    End::Flushed
}

/// Reads datagrams and queues the `Motion` messages among them. Anything
/// else, and anything that arrives before the connection is announced, is
/// dropped.
async fn read_datagrams(shared: Arc<Shared>, link: Arc<Link>, peer: DeviceId, conn_id: u64) {
    let mut was_current = false;
    loop {
        let Ok(datagram) = link.conn.read_datagram().await else {
            return;
        };
        let msg = match decode_datagram(&datagram) {
            Ok(Decoded::Message(Message::Core(msg @ CoreMessage::Motion { .. }))) => msg,
            // A bad datagram, or a message that belongs on the stream.
            _ => continue,
        };
        match shared.queue.offer(&peer, conn_id, msg) {
            Offer::Queued => {
                was_current = true;
                shared.queue.wake();
            }
            Offer::NotCurrent if was_current => return,
            Offer::NotCurrent | Offer::Full | Offer::Closed => {}
        }
    }
}
