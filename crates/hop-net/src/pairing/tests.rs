//! Pairing tests over loopback. Nothing is advertised with mDNS.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::*;

fn timing() -> PairingTiming {
    PairingTiming {
        code_lifetime: Duration::from_secs(20),
        exchange_timeout: Duration::from_secs(3),
        handshake_timeout: Duration::from_secs(2),
        verdict_timeout: Duration::from_secs(10),
        max_attempts: 3,
    }
}

fn code(text: &str) -> PairingCode {
    PairingCode::parse(text).unwrap()
}

fn host_info() -> PairInfo {
    PairInfo {
        name: "Krish's MacBook".to_owned(),
        os: Os::MacOs,
    }
}

fn joiner_info() -> PairInfo {
    PairInfo {
        name: "Krish's PC".to_owned(),
        os: Os::Windows,
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn host_with(
    identity: &Arc<Identity>,
    code: PairingCode,
    timing: PairingTiming,
) -> PairingHost {
    PairingHost::bind(
        identity.clone(),
        host_info(),
        loopback(),
        code,
        timing,
        false,
    )
    .await
    .unwrap()
}

/// Counts the host's failed attempts.
fn count_failures(host: &mut PairingHost) -> Arc<AtomicU32> {
    let failures = Arc::new(AtomicU32::new(0));
    let sink = failures.clone();
    host.on_failed_attempt(move |count| sink.store(count, Ordering::SeqCst));
    failures
}

async fn join(
    identity: &Arc<Identity>,
    code: PairingCode,
    addresses: &[SocketAddr],
) -> Result<PairedPeer, PairError> {
    pair_join(
        identity.clone(),
        joiner_info(),
        code,
        addresses,
        timing(),
        |_| {},
    )
    .await
}

/// A joiner that only connects, for driving the host by hand.
async fn raw_joiner(
    identity: &Identity,
    address: SocketAddr,
) -> (quinn::Endpoint, quinn::Connection) {
    let tls = tls::client_config(
        identity,
        Accept::AnyEd25519,
        tls::ALPN_PAIRING,
        Arc::default(),
    )
    .unwrap();
    let config = tls::quic_client(tls, Arc::new(pairing_transport(0).build().unwrap())).unwrap();
    let endpoint = bind_endpoint(loopback(), None).unwrap();
    let conn = endpoint
        .connect_with(config, address, tls::SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    (endpoint, conn)
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn codes_are_six_digits_and_parse_leniently() {
    for _ in 0..200 {
        let code = PairingCode::generate().unwrap();
        let shown = code.reveal();
        assert_eq!(shown.len(), 7);
        assert_eq!(shown.as_bytes()[3], b' ');
        assert!(
            shown
                .chars()
                .filter(|c| *c != ' ')
                .all(|c| c.is_ascii_digit())
        );
        assert_eq!(PairingCode::parse(&shown).unwrap().reveal(), shown);
    }
    assert_eq!(code("482913").reveal(), "482 913");
    assert_eq!(code(" 482-913 ").reveal(), "482 913");
    assert_eq!(code("000 007").reveal(), "000 007");
    for bad in ["", "48291", "4829134", "48291a", "482.913", "４８２９１３"] {
        assert!(
            matches!(PairingCode::parse(bad), Err(PairError::InvalidCode)),
            "{bad:?}"
        );
    }
    // The error never repeats what was typed.
    assert!(!PairError::InvalidCode.to_string().contains("48291"));
    assert_eq!(format!("{:?}", code("482913")), "PairingCode(redacted)");
}

#[test]
fn every_code_is_equally_likely() {
    // The sampling keeps only values below a multiple of 1,000,000, so
    // each code has exactly 4294 preimages.
    assert_eq!(CODE_REJECT_FROM % 1_000_000, 0);
    assert!(u64::from(CODE_REJECT_FROM) + 1_000_000 > u64::from(u32::MAX));
    assert_eq!(PairingCode::from_number(999_999).reveal(), "999 999");
    assert_eq!(PairingCode::from_number(0).reveal(), "000 000");
    // A rough check on real draws: the first digit is spread out.
    let mut first = [0u32; 10];
    for _ in 0..2000 {
        let shown = PairingCode::generate().unwrap().reveal();
        first[usize::from(shown.as_bytes()[0] - b'0')] += 1;
    }
    assert!(first.iter().all(|count| *count > 100), "{first:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_right_code_pins_both_sides() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("482913"), timing()).await;
    let address = host.local_addr();
    let saved: Arc<Mutex<Vec<PairedPeer>>> = Arc::default();
    let saved_by_host = saved.clone();
    let host_task = tokio::spawn(async move {
        let pending = host.wait_for_joiner().await.unwrap();
        pending
            .accept(|peer| {
                saved_by_host.lock().unwrap().push(peer.clone());
                Ok::<(), Infallible>(())
            })
            .await
    });
    let mut waiting_on = None;
    let peer = pair_join(
        joiner_id.clone(),
        joiner_info(),
        code("482 913"),
        &[address],
        timing(),
        |host| waiting_on = Some(host.name.clone()),
    )
    .await
    .unwrap();
    // The joiner pinned the host's key, from the handshake.
    assert_eq!(peer.fingerprint, host_id.fingerprint());
    assert_eq!(peer.id, host_id.device_id());
    assert_eq!(peer.name, "Krish's MacBook");
    assert_eq!(peer.os, Os::MacOs);
    assert_eq!(waiting_on.as_deref(), Some("Krish's MacBook"));
    // The host pinned the joiner's, and saved it before telling the joiner.
    let host_peer = host_task.await.unwrap().unwrap();
    assert_eq!(host_peer.fingerprint, joiner_id.fingerprint());
    assert_eq!(host_peer.id, joiner_id.device_id());
    assert_eq!(host_peer.name, "Krish's PC");
    assert_eq!(host_peer.os, Os::Windows);
    assert_eq!(*saved.lock().unwrap(), vec![host_peer]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_code_fails_and_the_third_failure_kills_the_code() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let mut host = host_with(&host_id, code("111111"), timing()).await;
    let failures = count_failures(&mut host);
    let address = host.local_addr();
    let host_task = tokio::spawn(host.wait_for_joiner());
    for attempt in 1..=3 {
        let result = pair_join(
            joiner_id.clone(),
            joiner_info(),
            code("222222"),
            &[address],
            timing(),
            |_| panic!("a wrong code never confirms"),
        )
        .await;
        assert!(matches!(result, Err(PairError::WrongCode)), "{result:?}");
        wait_until("the failure count", || {
            failures.load(Ordering::SeqCst) == attempt
        })
        .await;
    }
    let result = host_task.await.unwrap();
    assert!(
        matches!(result, Err(PairError::TooManyAttempts)),
        "{result:?}"
    );
    // The code is dead: even the right one gets nowhere now.
    let mut quick = timing();
    quick.handshake_timeout = Duration::from_millis(500);
    let result = pair_join(
        joiner_id,
        joiner_info(),
        code("111111"),
        &[address],
        quick,
        |_| {},
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joiner_that_hangs_up_after_the_hosts_spake2_message_counts_as_an_attempt() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let mut host = host_with(&host_id, code("333333"), timing()).await;
    let failures = count_failures(&mut host);
    let address = host.local_addr();
    let host_task = tokio::spawn(host.wait_for_joiner());

    // Hanging up before the host has sent anything costs nothing...
    let (_endpoint, conn) = raw_joiner(&joiner_id, address).await;
    conn.close(0u32.into(), b"bye");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(failures.load(Ordering::SeqCst), 0);

    // ...but once the host has answered with its SPAKE2 message, the
    // joiner could test a guess, so hanging up counts.
    let (_endpoint, conn) = raw_joiner(&joiner_id, address).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let (_, message) = Spake2::<Ed25519Group>::start_a(
        &Password::new(b"999999"),
        &SpakeIdentity::new(joiner_id.fingerprint().as_bytes()),
        &SpakeIdentity::new(host_id.fingerprint().as_bytes()),
    );
    wire::write_frame(&mut send, Kind::Spake, &message)
        .await
        .unwrap();
    wire::read_frame(&mut recv, Kind::Spake).await.unwrap();
    conn.close(0u32.into(), b"bye");
    wait_until("the counted hang-up", || {
        failures.load(Ordering::SeqCst) == 1
    })
    .await;

    // The code still works for the real joiner.
    let joiner = tokio::spawn({
        let joiner_id = joiner_id.clone();
        async move { join(&joiner_id, code("333333"), &[address]).await }
    });
    let pending = host_task.await.unwrap().unwrap();
    pending.accept(|_| Ok::<(), Infallible>(())).await.unwrap();
    joiner.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_joiner_never_runs_a_second_exchange_with_the_same_code() {
    let first_id = Arc::new(Identity::generate().unwrap());
    let second_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    // The first address answers with a different code (as a fake pairing
    // advertisement would); the second is the real laptop.
    let first = host_with(&first_id, code("444444"), timing()).await;
    let mut second = host_with(&second_id, code("555555"), timing()).await;
    let second_failures = count_failures(&mut second);
    let addresses = [first.local_addr(), second.local_addr()];
    let first_task = tokio::spawn(first.wait_for_joiner());
    let second_task = tokio::spawn(second.wait_for_joiner());

    let result = join(&joiner_id, code("555555"), &addresses).await;
    assert!(matches!(result, Err(PairError::WrongCode)), "{result:?}");
    // The joiner stopped after the failed confirmation: the second laptop
    // never ran an exchange (it would have succeeded with this code).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!second_task.is_finished());
    assert_eq!(second_failures.load(Ordering::SeqCst), 0);
    second_task.abort();
    first_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_addresses_are_skipped_before_the_exchange() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("121212"), timing()).await;
    let address = host.local_addr();
    let host_task = tokio::spawn(async move {
        let pending = host.wait_for_joiner().await.unwrap();
        pending.accept(|_| Ok::<(), Infallible>(())).await
    });
    // Nothing listens on the first address, so the joiner moves on.
    let dead = std::net::UdpSocket::bind(loopback()).unwrap();
    let dead_address = dead.local_addr().unwrap();
    drop(dead);
    let mut quick = timing();
    quick.handshake_timeout = Duration::from_millis(500);
    let peer = pair_join(
        joiner_id,
        joiner_info(),
        code("121212"),
        &[dead_address, address],
        quick,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(peer.fingerprint, host_id.fingerprint());
    host_task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_joiner_during_a_run_is_busy() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let other_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("666666"), timing()).await;
    let address = host.local_addr();
    let host_task = tokio::spawn(async move {
        let pending = host.wait_for_joiner().await.unwrap();
        let peer = pending.peer().clone();
        pending.accept(|_| Ok::<(), Infallible>(())).await.unwrap();
        peer
    });
    // A run is in progress: someone connected and is still silent.
    let (_endpoint, stalled) = raw_joiner(&other_id, address).await;
    let result = join(&joiner_id, code("666666"), &[address]).await;
    assert!(matches!(result, Err(PairError::Busy)), "{result:?}");
    // Once that run ends (it sent nothing, so it does not count), the real
    // joiner gets through.
    stalled.close(0u32.into(), b"bye");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let peer = join(&joiner_id, code("666666"), &[address]).await.unwrap();
    assert_eq!(peer.fingerprint, host_id.fingerprint());
    assert_eq!(
        host_task.await.unwrap().fingerprint,
        joiner_id.fingerprint()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joiner_arriving_while_the_user_decides_is_busy() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let other_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("676767"), timing()).await;
    let address = host.local_addr();
    let (pending_tx, pending_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = pending_tx.send(host.wait_for_joiner().await.unwrap());
    });
    let joiner = tokio::spawn({
        let joiner_id = joiner_id.clone();
        async move { join(&joiner_id, code("676767"), &[address]).await }
    });
    let pending = pending_rx.await.unwrap();
    assert_eq!(pending.peer().fingerprint, joiner_id.fingerprint());
    let result = join(&other_id, code("676767"), &[address]).await;
    assert!(matches!(result, Err(PairError::Busy)), "{result:?}");
    pending.accept(|_| Ok::<(), Infallible>(())).await.unwrap();
    joiner.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_code_expires() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let mut short = timing();
    short.code_lifetime = Duration::from_millis(300);
    let host = host_with(&host_id, code("787878"), short).await;
    let started = Instant::now();
    let result = host.wait_for_joiner().await;
    assert!(matches!(result, Err(PairError::Expired)), "{result:?}");
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_after_expiry_saves_nothing() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let mut short = timing();
    short.code_lifetime = Duration::from_secs(2);
    let host = host_with(&host_id, code("898989"), short).await;
    let address = host.local_addr();
    let joiner = tokio::spawn({
        let joiner_id = joiner_id.clone();
        async move { join(&joiner_id, code("898989"), &[address]).await }
    });
    let pending = host.wait_for_joiner().await.unwrap();
    tokio::time::sleep(pending.remaining() + Duration::from_millis(50)).await;
    let saved = AtomicU32::new(0);
    let result = pending
        .accept(|_| {
            saved.fetch_add(1, Ordering::SeqCst);
            Ok::<(), Infallible>(())
        })
        .await;
    assert!(matches!(result, Err(PairError::Expired)), "{result:?}");
    assert_eq!(saved.load(Ordering::SeqCst), 0);
    let joined = joiner.await.unwrap();
    assert!(matches!(joined, Err(PairError::Expired)), "{joined:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_pairing_saves_nothing_on_either_side() {
    for drop_instead in [false, true] {
        let host_id = Arc::new(Identity::generate().unwrap());
        let joiner_id = Arc::new(Identity::generate().unwrap());
        let host = host_with(&host_id, code("909090"), timing()).await;
        let address = host.local_addr();
        let joiner = tokio::spawn({
            let joiner_id = joiner_id.clone();
            async move { join(&joiner_id, code("909090"), &[address]).await }
        });
        let pending = host.wait_for_joiner().await.unwrap();
        if drop_instead {
            drop(pending);
        } else {
            pending.decline().await;
        }
        let result = joiner.await.unwrap();
        assert!(matches!(result, Err(PairError::Declined)), "{result:?}");
    }
}

/// The host takes a normal close (code 0) from the joiner as proof that its
/// "accepted" arrived. A joiner that stopped waiting must therefore close
/// with another code; with 0, a host whose user answered at that moment
/// would report a pairing the joiner never saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joiner_that_stops_waiting_never_looks_paired_to_the_host() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("373737"), timing()).await;
    let address = host.local_addr();
    let mut impatient = timing();
    impatient.verdict_timeout = Duration::from_millis(300);
    let joiner = tokio::spawn(async move {
        pair_join(
            joiner_id,
            joiner_info(),
            code("373737"),
            &[address],
            impatient,
            |_| {},
        )
        .await
    });
    let pending = host.wait_for_joiner().await.unwrap();
    // The user on the host is still deciding when the joiner gives up.
    let joined = joiner.await.unwrap();
    assert!(matches!(joined, Err(PairError::Declined)), "{joined:?}");
    let closed = tokio::time::timeout(Duration::from_secs(3), pending.conn.closed())
        .await
        .unwrap();
    let quinn::ConnectionError::ApplicationClosed(close) = closed else {
        panic!("{closed:?}");
    };
    assert_eq!(
        CloseCode::from_code(close.error_code.into_inner()),
        Some(CloseCode::Expired)
    );
    // Saying yes now saves nothing.
    let saved = AtomicU32::new(0);
    let result = pending
        .accept(|_| {
            saved.fetch_add(1, Ordering::SeqCst);
            Ok::<(), Infallible>(())
        })
        .await;
    assert!(
        matches!(result, Err(PairError::Connection(_))),
        "{result:?}"
    );
    assert_eq!(saved.load(Ordering::SeqCst), 0);
}

/// The same for a join that is dropped while it waits (the app's Cancel, or
/// its window closing): quinn's own close would carry code 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_dropped_while_it_waits_never_looks_paired_to_the_host() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("464646"), timing()).await;
    let address = host.local_addr();
    let (confirmed_tx, confirmed) = tokio::sync::oneshot::channel();
    let joiner = tokio::spawn(async move {
        pair_join(
            joiner_id,
            joiner_info(),
            code("464646"),
            &[address],
            timing(),
            move |_| {
                let _ = confirmed_tx.send(());
            },
        )
        .await
    });
    let pending = host.wait_for_joiner().await.unwrap();
    // The joiner now waits for the answer; it is dropped while the user on
    // the host still decides.
    confirmed.await.unwrap();
    joiner.abort();
    assert!(joiner.await.unwrap_err().is_cancelled());
    let closed = tokio::time::timeout(Duration::from_secs(3), pending.conn.closed())
        .await
        .unwrap();
    let quinn::ConnectionError::ApplicationClosed(close) = closed else {
        panic!("{closed:?}");
    };
    assert_eq!(
        CloseCode::from_code(close.error_code.into_inner()),
        Some(CloseCode::Expired)
    );
    let saved = AtomicU32::new(0);
    let result = pending
        .accept(|_| {
            saved.fetch_add(1, Ordering::SeqCst);
            Ok::<(), Infallible>(())
        })
        .await;
    assert!(
        matches!(result, Err(PairError::Connection(_))),
        "{result:?}"
    );
    assert_eq!(saved.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_save_tells_the_joiner_no() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let host = host_with(&host_id, code("929292"), timing()).await;
    let address = host.local_addr();
    let joiner = tokio::spawn({
        let joiner_id = joiner_id.clone();
        async move { join(&joiner_id, code("929292"), &[address]).await }
    });
    let pending = host.wait_for_joiner().await.unwrap();
    let result = pending.accept(|_| Err("disk full")).await;
    assert!(
        matches!(&result, Err(PairError::Save(why)) if why == "disk full"),
        "{result:?}"
    );
    assert!(matches!(joiner.await.unwrap(), Err(PairError::Declined)));
}

/// Forwards one pairing connection to `target`, as a machine in the middle
/// would: it shows the joiner the certificate of `as_host` and the host the
/// certificate of `as_joiner`, holding their keys.
async fn relay(as_host: Arc<Identity>, as_joiner: Arc<Identity>, target: SocketAddr) -> SocketAddr {
    let tls = tls::server_config(
        &as_host,
        Accept::AnyEd25519,
        tls::ALPN_PAIRING,
        Arc::default(),
    )
    .unwrap();
    let transport = Arc::new(pairing_transport(1).build().unwrap());
    let server = tls::quic_server(tls, transport, 16).unwrap();
    let endpoint = bind_endpoint(loopback(), Some(server)).unwrap();
    let address = endpoint.local_addr().unwrap();
    tokio::spawn(async move {
        let incoming = endpoint.accept().await.unwrap();
        let joiner_side = incoming.await.unwrap();
        let (_, host_side) = raw_joiner(&as_joiner, target).await;
        let (mut to_joiner, mut from_joiner) = joiner_side.accept_bi().await.unwrap();
        let (mut to_host, mut from_host) = host_side.open_bi().await.unwrap();
        let upstream = async move {
            while let Ok(Some(chunk)) = from_joiner.read_chunk(1024, true).await {
                if to_host.write_all(&chunk.bytes).await.is_err() {
                    break;
                }
            }
        };
        let downstream = async move {
            while let Ok(Some(chunk)) = from_host.read_chunk(1024, true).await {
                if to_joiner.write_all(&chunk.bytes).await.is_err() {
                    break;
                }
            }
        };
        let closer = {
            let joiner_side = joiner_side.clone();
            let host_side = host_side.clone();
            async move {
                if let quinn::ConnectionError::ApplicationClosed(close) = host_side.closed().await {
                    joiner_side.close(close.error_code, &close.reason);
                }
            }
        };
        tokio::join!(upstream, downstream, closer);
        let _ = endpoint;
    });
    address
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_in_the_middle_fails() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let relay_id = Arc::new(Identity::generate().unwrap());
    let mut host = host_with(&host_id, code("135791"), timing()).await;
    let failures = count_failures(&mut host);
    let host_address = host.local_addr();
    let host_task = tokio::spawn(host.wait_for_joiner());
    let relay_address = relay(relay_id.clone(), relay_id, host_address).await;
    // Even with the right code, the two TLS sessions (and so the SPAKE2
    // identities and exporter values) differ on each side of the relay.
    let result = join(&joiner_id, code("135791"), &[relay_address]).await;
    assert!(matches!(result, Err(PairError::WrongCode)), "{result:?}");
    wait_until("the host to count the relayed attempt", || {
        failures.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(!host_task.is_finished(), "the host paired with nobody");
    host_task.abort();
}

/// The relay above already fails on the SPAKE2 identities. This one holds
/// both laptops' keys, so each side sees the other's real certificate, the
/// identities match and the relayed SPAKE2 messages give both sides the same
/// key. Only the exporter values of its two TLS sessions differ, so this
/// fails only if both confirmations really are bound to them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_holding_both_keys_fails_on_the_exporter_alone() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let mut host = host_with(&host_id, code("246802"), timing()).await;
    let failures = count_failures(&mut host);
    let host_address = host.local_addr();
    let host_task = tokio::spawn(host.wait_for_joiner());
    let relay_address = relay(host_id.clone(), joiner_id.clone(), host_address).await;
    let result = join(&joiner_id, code("246802"), &[relay_address]).await;
    assert!(matches!(result, Err(PairError::WrongCode)), "{result:?}");
    wait_until("the host to count the relayed attempt", || {
        failures.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(!host_task.is_finished(), "the host paired with nobody");
    host_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joiner_with_no_address_or_its_own_key_gets_a_clear_error() {
    let identity = Arc::new(Identity::generate().unwrap());
    assert!(matches!(
        join(&identity, code("101010"), &[]).await,
        Err(PairError::NoAddress)
    ));
    let host = host_with(&identity, code("101010"), timing()).await;
    let address = host.local_addr();
    let host_task = tokio::spawn(host.wait_for_joiner());
    let result = join(&identity, code("101010"), &[address]).await;
    assert!(matches!(result, Err(PairError::SelfPairing)), "{result:?}");
    host_task.abort();
}

/// A joiner that typed a wrong code sends its confirmation and reads the
/// answer only after the connection has closed. The host must say why
/// with its close code (`WRONG_CODE`) and nothing else: a finished stream
/// reaching the joiner first would read as "the stream ended early" (the
/// flaky failure of hop-agent's wrong-code test on Windows CI).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_reader_hears_wrong_code_and_never_an_early_end() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let mut patient = timing();
    patient.max_attempts = 1000;
    let mut host = host_with(&host_id, code("111111"), patient).await;
    let failures = count_failures(&mut host);
    let address = host.local_addr();
    let host_task = tokio::spawn(host.wait_for_joiner());
    for attempt in 1..=30 {
        let (_endpoint, conn) = raw_joiner(&joiner_id, address).await;
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let (spake, ours) = Spake2::<Ed25519Group>::start_a(
            &Password::new(b"222222"),
            &SpakeIdentity::new(joiner_id.fingerprint().as_bytes()),
            &SpakeIdentity::new(host_id.fingerprint().as_bytes()),
        );
        wire::write_frame(&mut send, Kind::Spake, &ours)
            .await
            .unwrap();
        let theirs = wire::read_frame(&mut recv, Kind::Spake).await.unwrap();
        let key = spake.finish(&theirs).unwrap();
        let binding = wire::channel_binding(&conn).unwrap();
        let mac = wire::confirmation(&key, wire::LABEL_JOINER, &binding);
        wire::write_frame(
            &mut send,
            Kind::Confirm,
            &wire::encode_confirm(&mac, &joiner_info()),
        )
        .await
        .unwrap();
        // Read only once the host has hung up.
        let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("the host closes");
        let quinn::ConnectionError::ApplicationClosed(close) = closed else {
            panic!("attempt {attempt}: {closed:?}");
        };
        assert_eq!(
            CloseCode::from_code(close.error_code.into_inner()),
            Some(CloseCode::WrongCode)
        );
        let read = wire::read_frame(&mut recv, Kind::Confirm).await;
        assert!(
            matches!(&read, Err(failure) if matches!(failure.error, PairError::WrongCode)),
            "attempt {attempt}: {read:?}"
        );
        wait_until("the failure count", || {
            failures.load(Ordering::SeqCst) == attempt
        })
        .await;
    }
    host_task.abort();
}

/// A host from before the fix above could let the end of its stream
/// overtake its `WRONG_CODE` close. The joiner waits a moment for the close
/// code that follows an early end, so it still reports a wrong code. Here
/// the race is made certain: the host ends its stream, waits, then closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joiner_explains_an_early_end_by_the_close_that_follows() {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let tls = tls::server_config(
        &host_id,
        Accept::AnyEd25519,
        tls::ALPN_PAIRING,
        Arc::default(),
    )
    .unwrap();
    let transport = Arc::new(pairing_transport(1).build().unwrap());
    let server = tls::quic_server(tls, transport, 16).unwrap();
    let endpoint = bind_endpoint(loopback(), Some(server)).unwrap();
    let address = endpoint.local_addr().unwrap();
    let old_host = tokio::spawn(async move {
        let conn = endpoint.accept().await.unwrap().await.unwrap();
        let joiner = tls::peer_fingerprint(&conn).unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let theirs = wire::read_frame(&mut recv, Kind::Spake).await.unwrap();
        let (spake, ours) = Spake2::<Ed25519Group>::start_b(
            &Password::new(b"111111"),
            &SpakeIdentity::new(joiner.as_bytes()),
            &SpakeIdentity::new(host_id.fingerprint().as_bytes()),
        );
        wire::write_frame(&mut send, Kind::Spake, &ours)
            .await
            .unwrap();
        let _ = spake.finish(&theirs);
        wire::read_frame(&mut recv, Kind::Confirm).await.unwrap();
        // The end of the stream first, then the close that says why.
        send.finish().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        conn.close(CloseCode::WrongCode.varint(), b"wrong code");
        let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    });
    let result = join(&joiner_id, code("222222"), &[address]).await;
    assert!(matches!(result, Err(PairError::WrongCode)), "{result:?}");
    old_host.await.unwrap();
}
