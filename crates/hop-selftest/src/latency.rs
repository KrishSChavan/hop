//! A Wi-Fi latency probe (`hop latency`): small numbered UDP packets bounced
//! off the other computer, to see how cursor movement would feel.
//!
//! One side runs [`latency_serve`], the other [`latency_probe`]. A packet
//! carries a magic header, the probe's id, a sequence number and its send
//! time, and nothing else: never any input. The server answers only
//! well-formed requests, with a packet marked as a reply, so two servers can
//! never bounce packets between each other forever.
//!
//! Packet layout (24 bytes, big-endian):
//!
//! | bytes  | field                                           |
//! |--------|-------------------------------------------------|
//! | 0..6   | `HOPLAT`                                        |
//! | 6      | format version, 1                               |
//! | 7      | 0 for a request, 1 for a reply                  |
//! | 8..12  | probe id, random per run                        |
//! | 12..16 | sequence number                                 |
//! | 16..24 | send time, nanoseconds since the probe started  |

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The UDP port the latency probe uses unless told otherwise, next to Hop's
/// own 47800.
pub const DEFAULT_LATENCY_PORT: u16 = 47801;

/// A reply slower than this counts as lost: for moving a cursor it is as
/// good as lost anyway. The probe also waits this long after its last packet.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

const MAGIC: [u8; 6] = *b"HOPLAT";
const VERSION: u8 = 1;
const REQUEST: u8 = 0;
const REPLY: u8 = 1;
const PACKET_LEN: usize = 24;
/// How often the server looks at its stop flag.
const SERVE_POLL: Duration = Duration::from_millis(100);
/// The most packets one probe sends: over half an hour at 20 ms apart.
const MAX_PACKETS: u32 = 100_000;
/// The longest gap between packets a probe uses.
const MAX_INTERVAL: Duration = Duration::from_secs(60);

/// One probe packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Packet {
    pub reply: bool,
    pub probe: u32,
    pub seq: u32,
    pub sent_ns: u64,
}

impl Packet {
    pub(crate) fn encode(&self) -> [u8; PACKET_LEN] {
        let mut bytes = [0; PACKET_LEN];
        bytes[..6].copy_from_slice(&MAGIC);
        bytes[6] = VERSION;
        bytes[7] = if self.reply { REPLY } else { REQUEST };
        bytes[8..12].copy_from_slice(&self.probe.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.seq.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.sent_ns.to_be_bytes());
        bytes
    }

    /// The packet in `bytes`, or `None` for anything that is not exactly a
    /// probe packet of this version.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Packet> {
        let bytes: &[u8; PACKET_LEN] = bytes.try_into().ok()?;
        if bytes[..6] != MAGIC || bytes[6] != VERSION {
            return None;
        }
        let reply = match bytes[7] {
            REQUEST => false,
            REPLY => true,
            _ => return None,
        };
        let probe = u32::from_be_bytes(bytes[8..12].try_into().ok()?);
        let seq = u32::from_be_bytes(bytes[12..16].try_into().ok()?);
        let sent_ns = u64::from_be_bytes(bytes[16..24].try_into().ok()?);
        Some(Packet {
            reply,
            probe,
            seq,
            sent_ns,
        })
    }
}

/// Why the probe could not measure anything.
#[derive(Debug, thiserror::Error)]
pub enum LatencyError {
    /// A probe of zero packets measures nothing.
    #[error("the probe needs at least one packet")]
    NoPackets,
    /// The UDP socket could not be opened on this address.
    #[error("could not open a UDP socket on {addr}: {source}")]
    Bind {
        /// The address the socket was to use.
        addr: SocketAddr,
        /// What the OS said.
        #[source]
        source: io::Error,
    },
    /// The socket could not be set up.
    #[error("could not set up the UDP socket: {0}")]
    Socket(#[source] io::Error),
    /// Not a single reply came back.
    #[error("{}", no_replies(.peer, *.sent, .last_error.as_deref()))]
    NoReplies {
        /// Where the packets went.
        peer: SocketAddr,
        /// How many were sent.
        sent: u32,
        /// The last network error, if there was one.
        last_error: Option<String>,
    },
}

fn no_replies(peer: &SocketAddr, sent: u32, last_error: Option<&str>) -> String {
    let error = last_error.map_or(String::new(), |error| format!(" (last error: {error})"));
    format!(
        "no replies from {peer} to {sent} packets{error}. Is `hop latency` serving on the other \
         computer, and does its firewall allow UDP port {}? On a Mac, also check System \
         Settings > Privacy & Security > Local Network.",
        peer.port()
    )
}

/// Answers latency probes on `bind` until `stop` is set, checking it at
/// least every 100 ms.
///
/// Only well-formed probe requests get an answer, and the answer is the same
/// size as the request.
pub fn latency_serve(bind: SocketAddr, stop: Arc<AtomicBool>) -> Result<(), LatencyError> {
    let socket =
        UdpSocket::bind(bind).map_err(|source| LatencyError::Bind { addr: bind, source })?;
    serve_on(&socket, &stop)
}

/// [`latency_serve`] on a socket that is already bound.
pub(crate) fn serve_on(socket: &UdpSocket, stop: &AtomicBool) -> Result<(), LatencyError> {
    socket
        .set_read_timeout(Some(SERVE_POLL))
        .map_err(LatencyError::Socket)?;
    let mut buf = [0; 1500];
    while !stop.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                if let Some(packet) = Packet::decode(&buf[..len])
                    && !packet.reply
                {
                    let reply = Packet {
                        reply: true,
                        ..packet
                    };
                    // A reply that fails to go out is a lost packet to the
                    // prober, which counts it as such.
                    let _ = socket.send_to(&reply.encode(), from);
                }
            }
            Err(error) if is_timeout(&error) => {}
            // Nothing a single packet does may stop the server: Windows, for
            // one, reports an earlier reply that bounced as an error on the
            // next receive. Pause briefly so a broken socket cannot spin.
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    Ok(())
}

/// Sends `count` probe packets to `peer`, one every `interval`, and measures
/// how long each takes to come back.
///
/// Replies may arrive in any order or not at all: each one is matched to its
/// packet by sequence number, duplicates and strays are ignored, and a
/// packet whose reply has not arrived [`REPLY_TIMEOUT`] after it was sent
/// counts as lost. Fails with [`LatencyError::NoReplies`] when nothing comes
/// back. At most 100 000 packets are sent, at most a minute apart.
pub fn latency_probe(
    peer: SocketAddr,
    count: u32,
    interval: Duration,
) -> Result<LatencyStats, LatencyError> {
    probe(peer, count, interval, REPLY_TIMEOUT)
}

/// [`latency_probe`] with a chosen reply timeout.
pub(crate) fn probe(
    peer: SocketAddr,
    count: u32,
    interval: Duration,
    reply_timeout: Duration,
) -> Result<LatencyStats, LatencyError> {
    if count == 0 {
        return Err(LatencyError::NoPackets);
    }
    let count = count.min(MAX_PACKETS);
    let interval = interval.min(MAX_INTERVAL);
    let local = local_address_for(peer);
    let socket = UdpSocket::bind(local).map_err(|source| LatencyError::Bind {
        addr: local,
        source,
    })?;
    // A connected socket only hears from the peer.
    socket.connect(peer).map_err(LatencyError::Socket)?;

    let probe_id = probe_id();
    let start = Instant::now();
    // Per packet, indexed by sequence number: when it went out (as an
    // offset from `start`; `None` if sending failed) and how long its reply
    // took. They grow as packets go out.
    let mut sent_at: Vec<Option<Duration>> = Vec::new();
    let mut round_trips: Vec<Option<Duration>> = Vec::new();
    let mut sent = 0_u32;
    let mut received = 0_u32;
    let mut next_send = start;
    let mut last_send = start;
    let mut last_error = None;
    let mut buf = [0; 1500];
    loop {
        let now = Instant::now();
        if sent < count && now >= next_send {
            let offset = now.duration_since(start);
            let packet = Packet {
                reply: false,
                probe: probe_id,
                seq: sent,
                sent_ns: nanos(offset),
            };
            // A packet that cannot be sent is a lost packet; Wi-Fi drops
            // and sleeping peers make that normal.
            match socket.send(&packet.encode()) {
                Ok(_) => sent_at.push(Some(offset)),
                Err(error) => {
                    sent_at.push(None);
                    last_error = Some(error.to_string());
                }
            }
            round_trips.push(None);
            sent += 1;
            last_send = now;
            // Keep to the schedule, but never send a burst to catch up.
            next_send = (next_send + interval).max(now);
            continue;
        }
        if sent == count && (received == count || now >= last_send + reply_timeout) {
            break;
        }
        let until = if sent < count {
            next_send
        } else {
            last_send + reply_timeout
        };
        let wait = until
            .saturating_duration_since(now)
            .max(Duration::from_millis(1));
        socket
            .set_read_timeout(Some(wait))
            .map_err(LatencyError::Socket)?;
        match socket.recv(&mut buf) {
            Ok(len) => {
                let arrived = Instant::now().duration_since(start);
                let Some(packet) = Packet::decode(&buf[..len]) else {
                    continue;
                };
                if !packet.reply || packet.probe != probe_id {
                    continue;
                }
                let i = index(packet.seq);
                // The echoed send time must be the one this probe sent, so
                // a stale or forged reply cannot pass for a real one.
                let Some(Some(sent_offset)) = sent_at.get(i).copied() else {
                    continue;
                };
                if packet.sent_ns != nanos(sent_offset) {
                    continue;
                }
                let round_trip = arrived.saturating_sub(sent_offset);
                if let Some(slot @ None) = round_trips.get_mut(i)
                    && round_trip <= reply_timeout
                {
                    *slot = Some(round_trip);
                    received += 1;
                }
            }
            Err(error) if is_timeout(&error) => {}
            // An earlier packet bounced (the peer's port is closed), or the
            // network is having a moment: the packets in question are lost.
            Err(error) => {
                last_error = Some(error.to_string());
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let samples: Vec<f64> = round_trips.iter().flatten().map(as_ms).collect();
    LatencyStats::from_samples(count, samples).ok_or(LatencyError::NoReplies {
        peer,
        sent: count,
        last_error,
    })
}

/// A local address of the peer's family: loopback for a loopback peer, so a
/// local test never touches a real network, and any address otherwise.
fn local_address_for(peer: SocketAddr) -> SocketAddr {
    match peer {
        SocketAddr::V4(peer) if peer.ip().is_loopback() => (Ipv4Addr::LOCALHOST, 0).into(),
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(peer) if peer.ip().is_loopback() => (Ipv6Addr::LOCALHOST, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    }
}

/// A probe id that differs between runs, so replies to an earlier run that
/// arrive late are ignored. Not a secret; it only needs to differ.
fn probe_id() -> u32 {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    clock ^ std::process::id().rotate_left(16)
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn index(seq: u32) -> usize {
    usize::try_from(seq).unwrap_or(usize::MAX)
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn as_ms(duration: &Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// What a latency probe measured. Round trips are in milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct LatencyStats {
    /// Packets sent.
    pub sent: u32,
    /// Replies that came back in time.
    pub received: u32,
    /// Packets with no reply within [`REPLY_TIMEOUT`].
    pub lost: u32,
    /// The fastest round trip.
    pub min_ms: f64,
    /// The median round trip: what most cursor movement feels like.
    pub p50_ms: f64,
    /// 90% of round trips were at most this.
    pub p90_ms: f64,
    /// 99% of round trips were at most this: the worst hitches.
    pub p99_ms: f64,
    /// The slowest round trip.
    pub max_ms: f64,
}

impl LatencyStats {
    /// Statistics of `samples` round trips (milliseconds) out of `sent`
    /// packets, or `None` without any sample.
    pub(crate) fn from_samples(sent: u32, mut samples: Vec<f64>) -> Option<Self> {
        samples.retain(|ms| ms.is_finite());
        samples.sort_by(f64::total_cmp);
        let min_ms = *samples.first()?;
        let max_ms = *samples.last()?;
        let received = u32::try_from(samples.len()).unwrap_or(u32::MAX).min(sent);
        // Nearest-rank percentiles: the smallest sample with at least p% of
        // all samples at or below it.
        let percentile = |p: usize| {
            let rank = (p * samples.len()).div_ceil(100).max(1);
            samples.get(rank - 1).copied().unwrap_or(max_ms)
        };
        Some(Self {
            sent,
            received,
            lost: sent - received,
            min_ms,
            p50_ms: percentile(50),
            p90_ms: percentile(90),
            p99_ms: percentile(99),
            max_ms,
        })
    }

    /// The share of packets lost, in percent.
    pub fn loss_percent(&self) -> f64 {
        if self.sent == 0 {
            0.0
        } else {
            f64::from(self.lost) * 100.0 / f64::from(self.sent)
        }
    }

    /// How cursor movement would feel over this connection, in plain words.
    ///
    /// The cursor lags by about half a round trip plus processing. Typical
    /// motion follows the median; the 99th percentile shows the hitches.
    pub fn feel(&self) -> String {
        let mut feel = if self.p50_ms <= 10.0 && self.p99_ms <= 30.0 {
            "Excellent: cursor movement should feel like a local mouse.".to_string()
        } else if self.p50_ms <= 20.0 && self.p99_ms <= 60.0 {
            "Good: cursor movement should feel smooth, with a rare small hitch.".to_string()
        } else if self.p50_ms <= 40.0 && self.p99_ms <= 120.0 {
            "Fair: fast cursor movement will lag a little; clicking and typing are fine."
                .to_string()
        } else {
            "Poor: the cursor will feel sluggish or jumpy. Moving closer to the router, or \
             using 5 GHz Wi-Fi, may help."
                .to_string()
        };
        if self.p99_ms > 30.0 && self.p99_ms > 4.0 * self.p50_ms {
            feel.push_str(&format!(
                " Occasional spikes up to {:.0} ms will show as brief stutters.",
                self.p99_ms
            ));
        }
        if self.loss_percent() >= 1.0 {
            feel.push_str(&format!(
                " {:.1}% of packets were lost; Hop repairs lost motion, so that shows as small \
                 jumps rather than a drifting cursor.",
                self.loss_percent()
            ));
        }
        feel
    }
}

impl fmt::Display for LatencyStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Sent {} packets: {} came back, {} lost ({:.1}%).",
            self.sent,
            self.received,
            self.lost,
            self.loss_percent()
        )?;
        writeln!(
            f,
            "Round trip: min {:.1} ms, median {:.1} ms, p90 {:.1} ms, p99 {:.1} ms, max {:.1} ms.",
            self.min_ms, self.p50_ms, self.p90_ms, self.p99_ms, self.max_ms
        )?;
        write!(f, "How the cursor would feel: {}", self.feel())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use super::*;

    const LOOPBACK: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    #[test]
    fn packets_round_trip_and_strangers_are_rejected() {
        let packet = Packet {
            reply: false,
            probe: 0xDEAD_BEEF,
            seq: 7,
            sent_ns: 123_456_789,
        };
        let bytes = packet.encode();
        assert_eq!(&bytes[..8], b"HOPLAT\x01\x00");
        assert_eq!(Packet::decode(&bytes), Some(packet));
        let reply = Packet {
            reply: true,
            ..packet
        };
        assert_eq!(Packet::decode(&reply.encode()), Some(reply));

        assert_eq!(Packet::decode(&bytes[..23]), None);
        assert_eq!(Packet::decode(&[bytes.as_slice(), &[0]].concat()), None);
        let mut wrong = bytes;
        wrong[0] = b'X';
        assert_eq!(Packet::decode(&wrong), None);
        let mut wrong = bytes;
        wrong[6] = 2;
        assert_eq!(Packet::decode(&wrong), None);
        let mut wrong = bytes;
        wrong[7] = 9;
        assert_eq!(Packet::decode(&wrong), None);
    }

    #[test]
    fn statistics_use_nearest_rank_percentiles() {
        let samples: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        let stats = LatencyStats::from_samples(104, samples).unwrap();
        assert_eq!((stats.sent, stats.received, stats.lost), (104, 100, 4));
        assert_eq!(
            (
                stats.min_ms,
                stats.p50_ms,
                stats.p90_ms,
                stats.p99_ms,
                stats.max_ms
            ),
            (1.0, 50.0, 90.0, 99.0, 100.0)
        );
        let one = LatencyStats::from_samples(1, vec![4.5]).unwrap();
        assert_eq!((one.p50_ms, one.p99_ms, one.lost), (4.5, 4.5, 0));
        assert_eq!(LatencyStats::from_samples(3, vec![]), None);
    }

    fn stats(p50: f64, p99: f64, sent: u32, lost: u32) -> LatencyStats {
        LatencyStats {
            sent,
            received: sent - lost,
            lost,
            min_ms: p50 / 2.0,
            p50_ms: p50,
            p90_ms: p50,
            p99_ms: p99,
            max_ms: p99,
        }
    }

    #[test]
    fn the_display_says_how_the_cursor_would_feel() {
        assert!(stats(4.0, 12.0, 100, 0).feel().starts_with("Excellent"));
        assert!(stats(15.0, 50.0, 100, 0).feel().starts_with("Good"));
        assert!(stats(30.0, 100.0, 100, 0).feel().starts_with("Fair"));
        assert!(stats(80.0, 300.0, 100, 0).feel().starts_with("Poor"));
        assert!(
            stats(5.0, 90.0, 100, 0)
                .feel()
                .contains("spikes up to 90 ms")
        );
        assert!(
            stats(4.0, 12.0, 100, 3)
                .feel()
                .contains("3.0% of packets were lost")
        );
        assert_eq!(
            stats(4.0, 12.0, 100, 2).to_string(),
            "Sent 100 packets: 98 came back, 2 lost (2.0%).\n\
             Round trip: min 2.0 ms, median 4.0 ms, p90 4.0 ms, p99 12.0 ms, max 12.0 ms.\n\
             How the cursor would feel: Excellent: cursor movement should feel like a local \
             mouse. 2.0% of packets were lost; Hop repairs lost motion, so that shows as small \
             jumps rather than a drifting cursor."
        );
    }

    /// A latency server on an ephemeral loopback port.
    struct Server {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<Result<(), LatencyError>>>,
    }

    impl Server {
        fn start() -> Self {
            let socket = UdpSocket::bind(LOOPBACK).unwrap();
            let addr = socket.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let thread = thread::spawn(move || serve_on(&socket, &flag));
            Self {
                addr,
                stop,
                thread: Some(thread),
            }
        }

        fn stop(mut self) -> Result<(), LatencyError> {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn serve_and_probe_over_loopback() {
        let server = Server::start();
        let stats = probe(
            server.addr,
            20,
            Duration::from_millis(2),
            Duration::from_millis(500),
        )
        .unwrap();
        assert_eq!((stats.sent, stats.received, stats.lost), (20, 20, 0));
        assert!(stats.min_ms <= stats.p50_ms && stats.p50_ms <= stats.p99_ms);
        assert!(stats.max_ms < 500.0);
        // The server stops when asked, within its poll interval.
        let asked = Instant::now();
        server.stop().unwrap();
        assert!(asked.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn latency_serve_binds_and_stops() {
        // Find a free loopback port, then let latency_serve bind it.
        let addr = UdpSocket::bind(LOOPBACK).unwrap().local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let server = thread::spawn(move || latency_serve(addr, flag));
        // Give the server a moment to bind, or the first packets bounce.
        thread::sleep(Duration::from_millis(100));
        let stats = probe(
            addr,
            5,
            Duration::from_millis(20),
            Duration::from_millis(500),
        );
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap().unwrap();
        // Another process could in theory grab the port in between; only a
        // reply proves the server ran, and then every packet must be there.
        if let Ok(stats) = stats {
            assert_eq!(stats.received, 5);
        }
        let taken = UdpSocket::bind(LOOPBACK).unwrap();
        let busy = latency_serve(taken.local_addr().unwrap(), Arc::new(AtomicBool::new(true)));
        assert!(matches!(busy, Err(LatencyError::Bind { .. })));
    }

    #[test]
    fn the_server_ignores_replies_and_strangers() {
        let server = Server::start();
        let client = UdpSocket::bind(LOOPBACK).unwrap();
        client.connect(server.addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let reply = Packet {
            reply: true,
            probe: 1,
            seq: 0,
            sent_ns: 0,
        };
        client.send(&reply.encode()).unwrap();
        client.send(b"hello").unwrap();
        let mut buf = [0; 64];
        assert!(
            client.recv(&mut buf).is_err(),
            "no answer to a reply or junk"
        );
        // A real request still gets its answer, marked as a reply.
        let request = Packet {
            reply: false,
            ..reply
        };
        client.send(&request.encode()).unwrap();
        let len = client.recv(&mut buf).unwrap();
        assert_eq!(Packet::decode(&buf[..len]), Some(reply));
        server.stop().unwrap();
    }

    /// A peer that drops every fourth packet (seq 1, 5, 9, ...), holds back
    /// another to answer it after the next one, answers one twice, and
    /// mixes in junk, a stale reply and an unanswered echo.
    fn lossy_peer(socket: UdpSocket, stop: Arc<AtomicBool>, dropped: Arc<Mutex<Vec<u32>>>) {
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut held: Option<(Packet, SocketAddr)> = None;
        let mut buf = [0; 64];
        while !stop.load(Ordering::SeqCst) {
            let Ok((len, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let Some(request) = Packet::decode(&buf[..len]) else {
                continue;
            };
            let reply = Packet {
                reply: true,
                ..request
            };
            match request.seq % 4 {
                1 => {
                    dropped.lock().unwrap().push(request.seq);
                    continue;
                }
                2 => {
                    // Reordered: answered after the next packet.
                    held = Some((reply, from));
                    continue;
                }
                _ => {}
            }
            if request.seq == 0 {
                socket.send_to(b"junk", from).unwrap();
                // The request echoed back unchanged is not a reply.
                socket.send_to(&request.encode(), from).unwrap();
                let stale = Packet {
                    probe: request.probe ^ 1,
                    ..reply
                };
                socket.send_to(&stale.encode(), from).unwrap();
                let forged = Packet {
                    sent_ns: reply.sent_ns + 1,
                    ..reply
                };
                socket.send_to(&forged.encode(), from).unwrap();
                // A duplicate.
                socket.send_to(&reply.encode(), from).unwrap();
            }
            socket.send_to(&reply.encode(), from).unwrap();
            if let Some((late, to)) = held.take() {
                socket.send_to(&late.encode(), to).unwrap();
            }
        }
    }

    #[test]
    fn lost_reordered_and_duplicate_replies_are_counted_right() {
        let socket = UdpSocket::bind(LOOPBACK).unwrap();
        let addr = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let peer = {
            let stop = Arc::clone(&stop);
            let dropped = Arc::clone(&dropped);
            thread::spawn(move || lossy_peer(socket, stop, dropped))
        };
        let stats = probe(
            addr,
            20,
            Duration::from_millis(2),
            Duration::from_millis(400),
        )
        .unwrap();
        stop.store(true, Ordering::SeqCst);
        peer.join().unwrap();
        assert_eq!(*dropped.lock().unwrap(), [1, 5, 9, 13, 17]);
        assert_eq!((stats.sent, stats.received, stats.lost), (20, 15, 5));
    }

    #[test]
    fn a_silent_peer_is_an_error_that_says_what_to_check() {
        // Bound but never read: nothing comes back.
        let silent = UdpSocket::bind(LOOPBACK).unwrap();
        let addr = silent.local_addr().unwrap();
        let error = probe(
            addr,
            3,
            Duration::from_millis(1),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(matches!(error, LatencyError::NoReplies { sent: 3, .. }));
        assert!(error.to_string().contains("firewall allow UDP port"));
        assert!(matches!(
            latency_probe(addr, 0, Duration::ZERO),
            Err(LatencyError::NoPackets)
        ));
    }
}
