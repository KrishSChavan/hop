//! Loopback tests of the session link. Every socket binds 127.0.0.1, so no
//! test sends anything onto the real network (and macOS shows no Local
//! Network prompt). The file transfer tests in `files/tests.rs` share the
//! helpers here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hop_proto::{Hello, KIND_CORE, KIND_HELLO, Message, encode_frame};
use hop_types::{CoreMessage, DeviceId, InputEvent, KeyCode, MotionMark};
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;
use tokio::time::Instant;

use super::*;
use crate::tls::{self, Accept, VerifierStats};
use crate::{DownReason, Identity, NetEvent, PeerUp};

/// Test timing: short enough to keep the tests quick, long enough for a
/// loaded CI machine.
pub(super) fn timing() -> Timing {
    Timing {
        handshake_timeout: Duration::from_secs(3),
        hello_timeout: Duration::from_millis(1500),
        keep_alive: Duration::from_millis(200),
        idle_timeout: Duration::from_secs(3),
        higher_id_delay: Duration::from_millis(50),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        stable_after: Duration::from_secs(10),
        incompatible_retry: Duration::from_secs(1),
        duplicate_grace: Duration::from_millis(700),
        shutdown_wait: Duration::from_millis(500),
        resolve_timeout: Duration::from_secs(1),
    }
}

pub(super) fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

pub(super) fn pin(identity: &Identity, name: &str) -> PinnedPeer {
    PinnedPeer {
        id: identity.device_id(),
        fingerprint: identity.fingerprint(),
        name: name.to_owned(),
        address: None,
    }
}

pub(super) struct Node {
    pub(super) identity: Arc<Identity>,
    pub(super) handle: NetHandle,
    pub(super) events: NetEvents,
    /// Every event taken from `events`, in order.
    pub(super) log: Vec<NetEvent>,
}

impl Node {
    pub(super) fn id(&self) -> DeviceId {
        self.identity.device_id()
    }

    pub(super) fn address(&self) -> String {
        self.handle.local_addr().to_string()
    }

    /// The next event, waiting up to `wait`. Polls, so the tokio worker
    /// is never blocked.
    pub(super) async fn next(&mut self, wait: Duration) -> Option<NetEvent> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(event) = self.events.try_recv() {
                self.log.push(event.clone());
                return Some(event);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits for an event matching `want`, failing on timeout.
    pub(super) async fn expect(
        &mut self,
        what: &str,
        want: impl Fn(&NetEvent) -> bool,
    ) -> NetEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next(left).await {
                Some(event) if want(&event) => return event,
                Some(_) => {}
                None => panic!("timed out waiting for {what}; saw {:?}", self.log),
            }
        }
    }

    pub(super) async fn expect_up(&mut self) -> PeerUp {
        match self
            .expect("PeerUp", |e| matches!(e, NetEvent::PeerUp(_)))
            .await
        {
            NetEvent::PeerUp(up) => up,
            _ => unreachable!(),
        }
    }

    pub(super) async fn expect_down(&mut self) -> DownReason {
        match self
            .expect("PeerDown", |e| matches!(e, NetEvent::PeerDown { .. }))
            .await
        {
            NetEvent::PeerDown { reason, .. } => reason,
            _ => unreachable!(),
        }
    }

    /// Asserts that nothing arrives for `wait`.
    pub(super) async fn expect_quiet(&mut self, wait: Duration) {
        if let Some(event) = self.next(wait).await {
            panic!("expected no event, got {event:?}");
        }
    }
}

pub(super) async fn node_with(
    identity: Arc<Identity>,
    peers: Vec<PinnedPeer>,
    edit: impl FnOnce(&mut NetConfig),
) -> Node {
    let mut config = NetConfig::new(0, "test device", "0.1.0-test");
    config.bind = loopback();
    config.discovery = false;
    config.peers = peers;
    config.timing = timing();
    edit(&mut config);
    let (handle, events) = start(identity.clone(), config).await.unwrap();
    Node {
        identity,
        handle,
        events,
        log: Vec::new(),
    }
}

pub(super) async fn node(identity: Arc<Identity>, peers: Vec<PinnedPeer>) -> Node {
    node_with(identity, peers, |_| {}).await
}

/// Two nodes that pin each other; only `a` knows `b`'s address, so only
/// `a` dials.
async fn pair() -> (Node, Node) {
    pair_with(|_| {}, |_| {}).await
}

/// [`pair`], with `edit_a` and `edit_b` applied to the two configs.
pub(super) async fn pair_with(
    edit_a: impl FnOnce(&mut NetConfig),
    edit_b: impl FnOnce(&mut NetConfig),
) -> (Node, Node) {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let b = node_with(b_id.clone(), vec![pin(&a_id, "a")], edit_b).await;
    let a = node_with(a_id, vec![pin(&b_id, "b")], edit_a).await;
    assert!(a.handle.set_peer_address(&b.id(), Some(b.address())));
    (a, b)
}

pub(super) fn heartbeat(seq: u64) -> CoreMessage {
    CoreMessage::Heartbeat { seq }
}

pub(super) fn key_input(seq: u64) -> CoreMessage {
    CoreMessage::Input {
        seq,
        mark: MotionMark::default(),
        events: vec![InputEvent::Key {
            code: KeyCode::keyboard(0x04),
            pressed: true,
            repeat: false,
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_pair_connects_and_exchanges_messages() {
    let (mut a, mut b) = pair().await;
    let up_a = a.expect_up().await;
    let up_b = b.expect_up().await;
    assert_eq!(up_a.peer, b.id());
    assert_eq!(up_b.peer, a.id());
    assert!(up_a.dialled_by_us && !up_b.dialled_by_us);
    assert_eq!(up_a.name, "test device");
    assert_eq!(up_a.app_version, "0.1.0-test");
    // The clipboard and files stay off unless the config turns them on.
    let names: Vec<&str> = up_a
        .negotiated
        .capabilities
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["input", "keymap"]);
    assert!(!up_a.supports_clipboard() && !up_a.supports_files());
    assert_eq!(a.handle.connected_peers(), vec![b.id()]);

    // Reliable messages arrive in order, both ways.
    for seq in 1..=20 {
        a.handle.send(&b.id(), key_input(seq)).unwrap();
    }
    for seq in 1..=20 {
        let event = b
            .expect("Input", |e| matches!(e, NetEvent::Message { .. }))
            .await;
        let NetEvent::Message { from, msg } = event else {
            unreachable!()
        };
        assert_eq!(from, a.id());
        assert_eq!(msg, key_input(seq));
    }
    b.handle.send(&a.id(), heartbeat(9)).unwrap();
    let event = a
        .expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert_eq!(
        event,
        NetEvent::Message {
            from: b.id(),
            msg: heartbeat(9)
        }
    );

    // Clipboard text was not negotiated, so it is refused.
    let clipboard = CoreMessage::ClipboardText {
        generation: 1,
        text: "secret".to_owned(),
    };
    assert!(matches!(
        a.handle.send(&b.id(), clipboard),
        Err(SendError::NotNegotiated)
    ));
    assert!(matches!(
        a.handle.send(&DeviceId::new("nobody"), heartbeat(1)),
        Err(SendError::NotConnected)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_travels_as_datagrams() {
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    let conn = b.handle.current_connection(&a.id()).unwrap();
    let datagrams_before = conn.stats().frame_rx.datagram;
    let motion = CoreMessage::Motion {
        seq: 3,
        mark: MotionMark {
            n: 1,
            total_dx: 4.0,
            total_dy: -2.5,
        },
    };
    // Datagrams may be lost even on loopback; send a few.
    for _ in 0..5 {
        a.handle.send(&b.id(), motion.clone()).unwrap();
    }
    let event = b
        .expect("Motion", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert_eq!(
        event,
        NetEvent::Message {
            from: a.id(),
            msg: motion
        }
    );
    assert!(conn.stats().frame_rx.datagram > datagrams_before);
}

fn clipboard_text(generation: u64, text: &str) -> CoreMessage {
    CoreMessage::ClipboardText {
        generation,
        text: text.to_owned(),
    }
}

#[test]
fn the_capabilities_advertised_follow_the_config() {
    let names = |clipboard, files| -> Vec<String> {
        implemented_capabilities(clipboard, files)
            .into_iter()
            .map(|capability| format!("{} {}", capability.name, capability.version))
            .collect()
    };
    assert_eq!(names(false, false), ["input 1", "keymap 1"]);
    assert_eq!(
        names(true, false),
        ["input 1", "keymap 1", "clipboard.text 1"]
    );
    assert_eq!(names(false, true), ["input 1", "keymap 1", "files 1"]);
    assert_eq!(
        names(true, true),
        ["input 1", "keymap 1", "clipboard.text 1", "files 1"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_text_travels_before_the_keys_sent_after_it() {
    let on = |config: &mut NetConfig| config.clipboard = true;
    let (mut a, mut b) = pair_with(on, on).await;
    let up = a.expect_up().await;
    b.expect_up().await;
    assert!(up.supports_clipboard());
    let names: Vec<&str> = up
        .negotiated
        .capabilities
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["clipboard.text", "input", "keymap"]);
    // As after a crossing: the text, then the keys that may paste it.
    a.handle
        .send(&b.id(), clipboard_text(1, "copied on a"))
        .unwrap();
    a.handle.send(&b.id(), key_input(2)).unwrap();
    for expected in [clipboard_text(1, "copied on a"), key_input(2)] {
        let event = b
            .expect("a message", |e| matches!(e, NetEvent::Message { .. }))
            .await;
        assert_eq!(
            event,
            NetEvent::Message {
                from: a.id(),
                msg: expected
            }
        );
    }
    // And the other way.
    b.handle.send(&a.id(), clipboard_text(5, "from b")).unwrap();
    let event = a
        .expect("ClipboardText", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert_eq!(
        event,
        NetEvent::Message {
            from: b.id(),
            msg: clipboard_text(5, "from b")
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_text_needs_both_sides_to_turn_it_on() {
    // Only `a` turns it on: not negotiated, so `a` cannot send it.
    let (mut a, mut b) = pair_with(|config| config.clipboard = true, |_| {}).await;
    let up = a.expect_up().await;
    b.expect_up().await;
    assert!(!up.supports_clipboard());
    assert!(matches!(
        a.handle.send(&b.id(), clipboard_text(1, "secret")),
        Err(SendError::NotNegotiated)
    ));

    // A peer that sends it anyway (this raw one advertises everything) to
    // a device with the clipboard off: dropped, and what follows arrives.
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
    let (_endpoint, _conn, mut send, _recv) =
        raw_session(&a_id, &b_id, b.handle.local_addr(), None).await;
    b.expect_up().await;
    let text = Message::Core(clipboard_text(1, "unwanted"));
    send.write_all(&encode_frame(&text).unwrap()).await.unwrap();
    send.write_all(&encode_frame(&Message::Core(heartbeat(4))).unwrap())
        .await
        .unwrap();
    let event = b
        .expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert_eq!(
        event,
        NetEvent::Message {
            from: a_id.device_id(),
            msg: heartbeat(4)
        }
    );
}

/// With room for four messages, fifty clipboard texts would close the
/// connection as overloaded; only the newest one waits, so it stays up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_newest_clipboard_text_waits_for_the_core() {
    let on = |config: &mut NetConfig| config.clipboard = true;
    let small = |config: &mut NetConfig| {
        config.clipboard = true;
        config.limits.event_capacity = 4;
    };
    let (mut a, mut b) = pair_with(on, small).await;
    a.expect_up().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.events.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // b's core reads nothing meanwhile.
    for generation in 1..=50 {
        let text = format!("text {generation} {}", "x".repeat(1000));
        a.handle
            .send(&b.id(), clipboard_text(generation, &text))
            .unwrap();
    }
    a.handle.send(&b.id(), heartbeat(51)).unwrap();
    // The heartbeat came last on the stream, so once it waits, everything
    // before it was handled.
    while b.events.len() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(matches!(
        b.next(Duration::ZERO).await,
        Some(NetEvent::PeerUp(_))
    ));
    let Some(NetEvent::Message { msg, .. }) = b.next(Duration::ZERO).await else {
        panic!("a clipboard text: {:?}", b.log);
    };
    let CoreMessage::ClipboardText { generation, .. } = msg else {
        panic!("the clipboard text first");
    };
    assert_eq!(generation, 50, "the newest");
    assert_eq!(
        b.next(Duration::ZERO).await,
        Some(NetEvent::Message {
            from: a.id(),
            msg: heartbeat(51)
        })
    );
    b.expect_quiet(Duration::from_millis(200)).await;
    a.expect_quiet(Duration::ZERO).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nudge_is_a_datagram_the_peer_ignores() {
    assert_eq!(encode_datagram(&Message::Nudge).unwrap(), NUDGE);
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    let conn = b.handle.current_connection(&a.id()).unwrap();
    let before = conn.stats().frame_rx.datagram;
    // Datagrams may be lost even on loopback; nudge a few times.
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.stats().frame_rx.datagram == before {
        assert!(Instant::now() < deadline, "no nudge arrived");
        a.handle.nudge(&b.id());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    b.expect_quiet(Duration::from_millis(100)).await;
    // A peer that is not connected is skipped, quietly.
    a.handle.nudge(&DeviceId::new("nobody"));
    a.expect_quiet(Duration::ZERO).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_a_drop() {
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    assert!(b.handle.disconnect(&a.id()));
    assert_eq!(
        b.expect_down().await,
        DownReason::Local(CloseCode::Shutdown)
    );
    assert!(matches!(
        a.expect_down().await,
        DownReason::Remote { code: 0, .. }
    ));
    // `a` dials again after its backoff, and both hear about it.
    a.expect_up().await;
    b.expect_up().await;
    a.handle.send(&b.id(), heartbeat(2)).unwrap();
    b.expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_connection_runs_both_verifiers() {
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    // One full handshake each: a certificate check and a signature check.
    assert_eq!(a.handle.verifier_stats(), (1, 1, 0));
    assert_eq!(b.handle.verifier_stats(), (1, 1, 0));
    b.handle.disconnect(&a.id());
    a.expect_down().await;
    a.expect_up().await;
    b.expect_up().await;
    // The second connection between the same configs ran both checks
    // again: no session was resumed.
    assert_eq!(a.handle.verifier_stats(), (2, 2, 0));
    assert_eq!(b.handle.verifier_stats(), (2, 2, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_refuses_an_unpinned_client() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let stranger = Identity::generate().unwrap();
    // `b` pins someone else, so `a`'s certificate is unknown to it.
    let mut b = node(b_id.clone(), vec![pin(&stranger, "stranger")]).await;
    let mut a = node(a_id, vec![pin(&b_id, "b")]).await;
    a.handle.set_peer_address(&b.id(), Some(b.address()));
    a.expect_quiet(Duration::from_millis(800)).await;
    b.expect_quiet(Duration::ZERO).await;
    let (_, _, rejected) = b.handle.verifier_stats();
    assert!(rejected >= 1, "b's verifier refused a's certificate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dialler_refuses_a_server_with_the_wrong_certificate() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let impostor = Identity::generate().unwrap();
    let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
    // `a` expects the impostor's key at `b`'s address: `b`'s own
    // certificate must be refused.
    let mut expected = pin(&impostor, "b?");
    expected.address = Some(b.address());
    let mut a = node(a_id, vec![expected]).await;
    a.expect_quiet(Duration::from_millis(800)).await;
    b.expect_quiet(Duration::ZERO).await;
    let (certificates, _, rejected) = a.handle.verifier_stats();
    assert!(certificates >= 1 && rejected >= 1);
    // The dialler checks the server before sending its own certificate,
    // so `b` never even saw one.
    assert_eq!(b.handle.verifier_stats().0, 0);
}

/// A signing key that is not the one in the certificate it is paired
/// with: what an attacker who copied a paired laptop's certificate has.
fn mismatched_key(cert_of: &Identity, key_of: &Identity) -> Arc<CertifiedKey> {
    let provider = tls::provider();
    let signer = provider
        .key_provider
        .load_private_key(key_of.key())
        .unwrap();
    Arc::new(CertifiedKey::new(vec![cert_of.cert()], signer))
}

#[derive(Debug)]
struct FixedServerCert(Arc<CertifiedKey>);

impl rustls::server::ResolvesServerCert for FixedServerCert {
    fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

#[derive(Debug)]
struct FixedClientCert(Arc<CertifiedKey>);

impl rustls::client::ResolvesClientCert for FixedClientCert {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

fn test_endpoint(server: Option<quinn::ServerConfig>) -> quinn::Endpoint {
    let socket = std::net::UdpSocket::bind(loopback()).unwrap();
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        server,
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_a_copied_certificate_but_another_key_is_refused() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let attacker = Identity::generate().unwrap();
    // The rogue server presents b's (public) certificate but can only sign
    // with its own key.
    let mut rogue_tls = rustls::ServerConfig::builder_with_provider(tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(FixedServerCert(mismatched_key(&b_id, &attacker))));
    rogue_tls.alpn_protocols = vec![tls::ALPN_SESSION.to_vec()];
    let transport = Arc::new(session_transport(&timing(), 1).build().unwrap());
    let rogue = test_endpoint(Some(tls::quic_server(rogue_tls, transport, 16).unwrap()));
    let accepting = rogue.clone();
    tokio::spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            tokio::spawn(async move {
                let _ = incoming.await;
            });
        }
    });

    let mut expected = pin(&b_id, "b");
    expected.address = Some(rogue.local_addr().unwrap().to_string());
    let mut a = node(a_id, vec![expected]).await;
    a.expect_quiet(Duration::from_millis(800)).await;
    let (certificates, signatures, rejected) = a.handle.verifier_stats();
    assert!(
        certificates >= 1,
        "the copied certificate passed the pin check"
    );
    assert!(
        signatures >= 1 && rejected >= 1,
        "but its signature was refused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_with_a_copied_certificate_but_another_key_is_refused() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let attacker = Identity::generate().unwrap();
    let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;

    let server_check = Arc::new(tls::PinVerifier::new(
        Accept::Exactly(b_id.fingerprint()),
        Arc::new(VerifierStats::default()),
    ));
    let mut rogue_tls = rustls::ClientConfig::builder_with_provider(tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(server_check)
        .with_client_cert_resolver(Arc::new(FixedClientCert(mismatched_key(&a_id, &attacker))));
    rogue_tls.alpn_protocols = vec![tls::ALPN_SESSION.to_vec()];
    rogue_tls.enable_sni = false;
    let transport = Arc::new(session_transport(&timing(), 0).build().unwrap());
    let client = tls::quic_client(rogue_tls, transport).unwrap();
    let endpoint = test_endpoint(None);
    let attempt = endpoint
        .connect_with(client, b.handle.local_addr(), tls::SERVER_NAME)
        .unwrap()
        .await;
    if let Ok(conn) = attempt {
        // The client may finish its side before the server's verdict
        // arrives; the server must close it.
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
    }
    b.expect_quiet(Duration::from_millis(300)).await;
    let (certificates, signatures, rejected) = b.handle.verifier_stats();
    assert!(certificates >= 1 && signatures >= 1 && rejected >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_dials_leave_one_connection_and_the_core_hears_only_it() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let both_at_once = |config: &mut NetConfig| config.timing.higher_id_delay = Duration::ZERO;
    let mut a = node_with(a_id.clone(), vec![pin(&b_id, "b")], both_at_once).await;
    let mut b = node_with(b_id.clone(), vec![pin(&a_id, "a")], both_at_once).await;
    // Both learn the other's address at the same moment, so both dial.
    a.handle.set_peer_address(&b.id(), Some(b.address()));
    b.handle.set_peer_address(&a.id(), Some(a.address()));

    let up_a = a.expect_up().await;
    let up_b = b.expect_up().await;
    // Let any losing connection be closed, then check nothing else came.
    a.expect_quiet(Duration::from_millis(600)).await;
    b.expect_quiet(Duration::ZERO).await;
    assert_eq!(a.log.len(), 1, "{:?}", a.log);
    assert_eq!(b.log.len(), 1, "{:?}", b.log);
    // Both kept the same connection: one side dialled it, the other
    // accepted it.
    assert_ne!(up_a.dialled_by_us, up_b.dialled_by_us);
    assert_eq!(a.handle.connected_peers(), vec![b.id()]);
    assert_eq!(b.handle.connected_peers(), vec![a.id()]);
    // And it works.
    a.handle.send(&b.id(), heartbeat(1)).unwrap();
    b.expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    b.handle.send(&a.id(), heartbeat(2)).unwrap();
    a.expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
}

#[test]
fn the_duplicate_rule_keeps_the_connection_dialled_by_the_lower_id() {
    use super::conn::{Direction, new_wins};
    let low = DeviceId::new("0000");
    let high = DeviceId::new("ffff");
    // On the lower device: its own dial wins, the higher's loses.
    assert!(new_wins(
        &low,
        &high,
        Direction::Outgoing,
        Direction::Incoming
    ));
    assert!(!new_wins(
        &low,
        &high,
        Direction::Incoming,
        Direction::Outgoing
    ));
    // On the higher device: the lower's dial wins, its own loses.
    assert!(new_wins(
        &high,
        &low,
        Direction::Incoming,
        Direction::Outgoing
    ));
    assert!(!new_wins(
        &high,
        &low,
        Direction::Outgoing,
        Direction::Incoming
    ));
    // The same device dialled twice: it gave up on the old one.
    assert!(new_wins(
        &low,
        &high,
        Direction::Incoming,
        Direction::Incoming
    ));
    assert!(new_wins(
        &high,
        &low,
        Direction::Outgoing,
        Direction::Outgoing
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_send_queue_closes_the_connection() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let mut b = node_with(b_id.clone(), vec![pin(&a_id, "a")], |config| {
        config.limits.send_capacity = 4;
    })
    .await;
    // A peer that completes the Hello exchange and then stops reading,
    // with a stream window of only 2 KiB, so b's writer blocks.
    let (_endpoint, conn, _send, _recv) =
        raw_session(&a_id, &b_id, b.handle.local_addr(), Some(2048)).await;
    b.expect_up().await;
    let mut result = Ok(());
    for seq in 0..200 {
        result = b.handle.send(&a_id.device_id(), key_input(seq));
        if result.is_err() {
            break;
        }
    }
    assert!(matches!(result, Err(SendError::Overloaded)), "{result:?}");
    assert_eq!(
        b.expect_down().await,
        DownReason::Local(CloseCode::Overloaded)
    );
    let closed = tokio::time::timeout(Duration::from_secs(3), conn.closed())
        .await
        .unwrap();
    assert_eq!(app_code(&closed), Some(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_core_that_does_not_keep_up_closes_the_connection_instead_of_dropping() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let mut b = node_with(b_id.clone(), vec![pin(&a_id, "a")], |config| {
        config.limits.event_capacity = 4;
    })
    .await;
    let mut a = node(a_id.clone(), vec![pin(&b_id, "b")]).await;
    a.handle.set_peer_address(&b.id(), Some(b.address()));
    a.expect_up().await;
    // b's core reads nothing while a sends more than its queue holds.
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.events.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for seq in 0..10 {
        let _ = a.handle.send(&b.id(), key_input(seq));
    }
    assert!(matches!(
        a.expect_down().await,
        DownReason::Remote { code: 4, .. }
    ));
    // b's core gets everything that fit, in order, then PeerDown.
    b.expect_up().await;
    let mut seqs = Vec::new();
    loop {
        match b.next(Duration::from_secs(5)).await.unwrap() {
            NetEvent::Message { msg, .. } => seqs.push(msg.seq().unwrap()),
            NetEvent::PeerDown { reason, .. } => {
                assert_eq!(reason, DownReason::Local(CloseCode::Overloaded));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(seqs, [0, 1, 2, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_delivers_queued_messages_then_closes() {
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    let release = CoreMessage::Release { seq: 7 };
    a.handle.send(&b.id(), release.clone()).unwrap();
    a.handle.shutdown().await;
    let event = b
        .expect("Release", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert_eq!(
        event,
        NetEvent::Message {
            from: a.id(),
            msg: release
        }
    );
    let reason = b.expect_down().await;
    assert!(
        matches!(reason, DownReason::Remote { code: 0, .. }),
        "{reason:?}"
    );
    // After shutdown nothing reaches a's core.
    a.expect_quiet(Duration::from_millis(100)).await;
    a.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_port_is_a_single_instance_lock() {
    let first = node(Arc::new(Identity::generate().unwrap()), Vec::new()).await;
    let mut config = NetConfig::new(0, "second", "0.1.0");
    config.bind = first.handle.local_addr();
    config.discovery = false;
    let second = start(Arc::new(Identity::generate().unwrap()), config).await;
    assert!(matches!(second, Err(NetError::Bind { .. })), "{second:?}");
    let mut ipv6 = NetConfig::new(0, "v6", "0.1.0");
    ipv6.bind = "[::1]:0".parse().unwrap();
    ipv6.discovery = false;
    assert!(matches!(
        start(Arc::new(Identity::generate().unwrap()), ipv6).await,
        Err(NetError::NotIpv4(_))
    ));
}

#[test]
fn unusable_peers_are_skipped() {
    let me = Identity::generate().unwrap();
    let other = Identity::generate().unwrap();
    let third = Identity::generate().unwrap();
    let mut wrong_id = pin(&third, "wrong id");
    wrong_id.id = other.device_id();
    let peers = vec![
        pin(&other, "other"),
        pin(&me, "myself"),
        wrong_id,
        pin(&other, "other again"),
    ];
    let usable = usable_peers(&me, peers);
    assert_eq!(usable, vec![pin(&other, "other")]);
}

// ---- Hello checks, with a hand-made peer --------------------------------

/// Connects to `server` as `client` (which the server pins) over a raw
/// QUIC connection, writes `client`'s Hello, and reads the server's.
pub(super) async fn raw_session(
    client: &Identity,
    server: &Identity,
    address: SocketAddr,
    stream_window: Option<u32>,
) -> (
    quinn::Endpoint,
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
) {
    let (endpoint, conn) = raw_connect(client, server, address, stream_window).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&hello_frame(client.device_id(), 1))
        .await
        .unwrap();
    let mut header = [0u8; 6];
    recv.read_exact(&mut header).await.unwrap();
    let len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]);
    let mut body = vec![0; len as usize];
    recv.read_exact(&mut body).await.unwrap();
    assert_eq!(u16::from_be_bytes([header[0], header[1]]), KIND_HELLO);
    (endpoint, conn, send, recv)
}

pub(super) async fn raw_connect(
    client: &Identity,
    server: &Identity,
    address: SocketAddr,
    stream_window: Option<u32>,
) -> (quinn::Endpoint, quinn::Connection) {
    let tls = tls::client_config(
        client,
        Accept::Exactly(server.fingerprint()),
        tls::ALPN_SESSION,
        Arc::default(),
    )
    .unwrap();
    let mut params = session_transport(&timing(), 0);
    if let Some(window) = stream_window {
        params.stream_window = window;
        params.connection_window = window;
    }
    let config = tls::quic_client(tls, Arc::new(params.build().unwrap())).unwrap();
    let endpoint = test_endpoint(None);
    let conn = endpoint
        .connect_with(config, address, tls::SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    (endpoint, conn)
}

pub(super) fn hello_frame(id: DeviceId, major: u16) -> Vec<u8> {
    let mut hello = Hello::new("0.1.0", id, "raw peer", Vec::new());
    hello.proto_major = major;
    encode_frame(&Message::Hello(hello)).unwrap()
}

pub(super) fn app_code(error: &quinn::ConnectionError) -> Option<u64> {
    match error {
        quinn::ConnectionError::ApplicationClosed(close) => Some(close.error_code.into_inner()),
        _ => None,
    }
}

/// Runs a raw peer that writes `first_bytes` on the control stream, then
/// returns the server's close code.
async fn close_code_after(first_bytes: Vec<u8>, write_nothing: bool) -> (Option<u64>, Node) {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
    let (_endpoint, conn) = raw_connect(&a_id, &b_id, b.handle.local_addr(), None).await;
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    if !write_nothing {
        send.write_all(&first_bytes).await.unwrap();
    }
    let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed())
        .await
        .unwrap();
    (app_code(&closed), b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hello_naming_another_device_is_refused() {
    let other = Identity::generate().unwrap();
    let (code, mut b) = close_code_after(hello_frame(other.device_id(), 1), false).await;
    assert_eq!(code, Some(3));
    b.expect_quiet(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_hello_is_refused() {
    let started = Instant::now();
    let (code, mut b) = close_code_after(Vec::new(), true).await;
    assert_eq!(code, Some(3));
    assert!(started.elapsed() >= timing().hello_timeout);
    b.expect_quiet(Duration::from_millis(100)).await;
    // A header that never gets its body is just as late.
    let mut partial = hello_frame(DeviceId::new("x"), 1);
    partial.truncate(10);
    let (code, _) = close_code_after(partial, false).await;
    assert_eq!(code, Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_hello_is_refused_from_its_header() {
    let mut header = KIND_HELLO.to_be_bytes().to_vec();
    header.extend_from_slice(&(64 * 1024 + 1u32).to_be_bytes());
    let (code, _) = close_code_after(header, false).await;
    assert_eq!(code, Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_frame_that_is_not_hello_is_refused() {
    let frame = encode_frame(&Message::Core(heartbeat(1))).unwrap();
    let (code, _) = close_code_after(frame, false).await;
    assert_eq!(code, Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_major_version_mismatch_closes_with_incompatible() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
    let (_endpoint, conn) = raw_connect(&a_id, &b_id, b.handle.local_addr(), None).await;
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    send.write_all(&hello_frame(a_id.device_id(), 2))
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed())
        .await
        .unwrap();
    assert_eq!(app_code(&closed), Some(2));
    let quinn::ConnectionError::ApplicationClosed(close) = closed else {
        unreachable!()
    };
    let reason = String::from_utf8_lossy(&close.reason).into_owned();
    assert!(reason.contains("1.0") && reason.contains("2.0"), "{reason}");
    b.expect_quiet(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undecodable_frame_or_a_second_hello_closes_the_connection() {
    for bad in [
        {
            // A Core frame whose body is not MessagePack.
            let mut frame = KIND_CORE.to_be_bytes().to_vec();
            frame.extend_from_slice(&1u32.to_be_bytes());
            frame.push(0xc1);
            frame
        },
        hello_frame(DeviceId::new("again"), 1),
    ] {
        let a_id = Arc::new(Identity::generate().unwrap());
        let b_id = Arc::new(Identity::generate().unwrap());
        let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
        let (_endpoint, conn, mut send, _recv) =
            raw_session(&a_id, &b_id, b.handle.local_addr(), None).await;
        b.expect_up().await;
        // A good message first, then the bad frame.
        send.write_all(&encode_frame(&Message::Core(heartbeat(5))).unwrap())
            .await
            .unwrap();
        send.write_all(&bad).await.unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .unwrap();
        assert_eq!(app_code(&closed), Some(3));
        b.expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
            .await;
        assert_eq!(
            b.expect_down().await,
            DownReason::Local(CloseCode::Protocol)
        );
    }
}

/// A peer that gives up on the control stream (resets its side, or stops
/// ours) but keeps the connection open. The link reports the peer gone, so
/// it must also close the connection: left open, keep-alives would hold it
/// forever, and the peer would still count it as current and refuse new
/// connections from this device as duplicates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_abandons_the_control_stream_is_closed_not_left_open() {
    for reset_instead_of_stop in [true, false] {
        let a_id = Arc::new(Identity::generate().unwrap());
        let b_id = Arc::new(Identity::generate().unwrap());
        let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
        let (_endpoint, conn, mut send, mut recv) =
            raw_session(&a_id, &b_id, b.handle.local_addr(), None).await;
        b.expect_up().await;
        if reset_instead_of_stop {
            send.reset(0u32.into()).unwrap();
        } else {
            // b notices a stopped stream only when it next writes.
            recv.stop(0u32.into()).unwrap();
            let sender = b.handle.clone();
            let peer = a_id.device_id();
            tokio::spawn(async move {
                while sender.send(&peer, heartbeat(1)).is_ok() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
        let reason = b.expect_down().await;
        let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("b closed the connection it reported as down");
        assert_eq!(app_code(&closed), Some(3), "reset: {reset_instead_of_stop}");
        assert_eq!(reason, DownReason::Local(CloseCode::Protocol));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_frame_kinds_are_skipped() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
    let (_endpoint, _conn, mut send, _recv) =
        raw_session(&a_id, &b_id, b.handle.local_addr(), None).await;
    b.expect_up().await;
    let mut unknown = 99u16.to_be_bytes().to_vec();
    unknown.extend_from_slice(&3u32.to_be_bytes());
    unknown.extend_from_slice(b"new");
    send.write_all(&unknown).await.unwrap();
    send.write_all(&encode_frame(&Message::Core(heartbeat(6))).unwrap())
        .await
        .unwrap();
    let event = b
        .expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert_eq!(
        event,
        NetEvent::Message {
            from: a_id.device_id(),
            msg: heartbeat(6)
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certificates_are_read_again_after_the_handshake() {
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    let conn = a.handle.current_connection(&b.id()).unwrap();
    let identity = conn.peer_identity().unwrap();
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().unwrap();
    assert_eq!(certs.len(), 1);
    assert_eq!(tls::peer_fingerprint(&conn), Some(b.identity.fingerprint()));
    assert!(tls::negotiated_alpn_is(&conn, tls::ALPN_SESSION));
    assert!(!tls::negotiated_alpn_is(&conn, tls::ALPN_PAIRING));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_listener_refuses_pairing_connections() {
    use crate::{PairInfo, PairingCode, PairingTiming, pair_join};
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    // Even a pinned device cannot use `hop-pair/1` on the session port.
    let mut b = node(b_id.clone(), vec![pin(&a_id, "a")]).await;
    let timing = PairingTiming {
        handshake_timeout: Duration::from_millis(800),
        ..PairingTiming::default()
    };
    let result = pair_join(
        a_id,
        PairInfo {
            name: "a".to_owned(),
            os: hop_types::Os::Windows,
        },
        PairingCode::parse("123456").unwrap(),
        &[b.handle.local_addr()],
        timing,
        |_| panic!("never gets this far"),
    )
    .await;
    assert!(result.is_err(), "{result:?}");
    b.expect_quiet(Duration::from_millis(100)).await;
    // The ALPN mismatch stops the handshake before any certificate check.
    assert_eq!(b.handle.verifier_stats().0, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_handle_closes_every_connection() {
    let (mut a, mut b) = pair().await;
    a.expect_up().await;
    b.expect_up().await;
    let Node { handle, events, .. } = a;
    drop(events);
    drop(handle);
    assert!(matches!(
        b.expect_down().await,
        DownReason::Remote { code: 0, .. }
    ));
}
