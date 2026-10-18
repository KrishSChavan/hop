//! Runs pairing and a session with every log event captured, at every
//! level and from every crate (quinn's included), and checks that no
//! pairing code, key code or clipboard text appears in them.
//!
//! This is its own test binary because it installs a process-wide log
//! subscriber. Everything binds 127.0.0.1; nothing is advertised.

use std::convert::Infallible;
use std::fmt::{self, Write as _};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hop_net::{
    Identity, NetConfig, NetEvent, PairError, PairInfo, PairingCode, PairingHost, PairingTiming,
    PinnedPeer, pair_join,
};
use hop_types::{CoreMessage, InputEvent, KeyCode, MotionMark, Os};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Every log line, as text.
struct Capture {
    text: Mutex<String>,
    next_span: AtomicU64,
}

static CAPTURE: Capture = Capture {
    text: Mutex::new(String::new()),
    next_span: AtomicU64::new(1),
};

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={value}", field.name());
    }
}

struct Recorder;

impl Subscriber for Recorder {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        let mut line = format!("span {}", span.metadata().name());
        span.record(&mut Fields(&mut line));
        CAPTURE.text.lock().unwrap().push_str(&(line + "\n"));
        Id::from_u64(CAPTURE.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, values: &Record<'_>) {
        let mut line = String::from("record");
        values.record(&mut Fields(&mut line));
        CAPTURE.text.lock().unwrap().push_str(&(line + "\n"));
    }

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut line = format!("{} {}", metadata.level(), metadata.target());
        event.record(&mut Fields(&mut line));
        CAPTURE.text.lock().unwrap().push_str(&(line + "\n"));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

const RIGHT_CODE: &str = "907315";
const WRONG_CODE: &str = "246813";
/// The usage of the Z key, and how a `KeyCode` holding it would print.
const KEY: u16 = 0x1D;
const SECRET_TEXT: &str = "correct horse battery staple";

fn timing() -> PairingTiming {
    PairingTiming {
        code_lifetime: Duration::from_secs(20),
        exchange_timeout: Duration::from_secs(3),
        handshake_timeout: Duration::from_secs(2),
        verdict_timeout: Duration::from_secs(10),
        max_attempts: 3,
    }
}

fn info(name: &str, os: Os) -> PairInfo {
    PairInfo {
        name: name.to_owned(),
        os,
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn pair_once(code: &str, typed: &str) -> Result<(), PairError> {
    let host_id = Arc::new(Identity::generate().unwrap());
    let joiner_id = Arc::new(Identity::generate().unwrap());
    let host = PairingHost::bind(
        host_id,
        info("Mac", Os::MacOs),
        loopback(),
        PairingCode::parse(code).unwrap(),
        timing(),
        false,
    )
    .await
    .unwrap();
    let address = host.local_addr();
    let host_task = tokio::spawn(async move {
        let pending = host.wait_for_joiner().await?;
        pending
            .accept(|_| Ok::<(), Infallible>(()))
            .await
            .map(|_| ())
    });
    let joined = pair_join(
        joiner_id,
        info("PC", Os::Windows),
        PairingCode::parse(typed).unwrap(),
        &[address],
        timing(),
        |_| {},
    )
    .await;
    if joined.is_ok() {
        host_task.await.unwrap()?;
    } else {
        host_task.abort();
    }
    joined.map(|_| ())
}

async fn run_session() {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let config = |name: &str, peer: &Identity| {
        let mut config = NetConfig::new(0, name, "0.1.0");
        config.bind = loopback();
        config.discovery = false;
        config.peers = vec![PinnedPeer {
            id: peer.device_id(),
            fingerprint: peer.fingerprint(),
            name: "peer".to_owned(),
            address: None,
        }];
        config
    };
    let (b, b_events) = hop_net::start(b_id.clone(), config("b", &a_id))
        .await
        .unwrap();
    let (a, _a_events) = hop_net::start(a_id.clone(), config("a", &b_id))
        .await
        .unwrap();
    a.set_peer_address(&b_id.device_id(), Some(b.local_addr().to_string()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while a.connected_peers().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no connection");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let key = |pressed| InputEvent::Key {
        code: KeyCode::keyboard(KEY),
        pressed,
        repeat: false,
    };
    let input = CoreMessage::Input {
        seq: 1,
        mark: MotionMark::default(),
        events: vec![key(true), key(false)],
    };
    a.send(&b_id.device_id(), input.clone()).unwrap();
    // Not negotiated in M2, so it is refused; it must not be logged either.
    let clipboard = CoreMessage::ClipboardText {
        generation: 1,
        text: SECRET_TEXT.to_owned(),
    };
    assert!(a.send(&b_id.device_id(), clipboard).is_err());
    loop {
        match b_events.try_recv() {
            Some(NetEvent::Message { msg, .. }) => {
                assert_eq!(msg, input);
                break;
            }
            Some(_) => {}
            None => {
                assert!(tokio::time::Instant::now() < deadline, "no message");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn no_code_key_or_clipboard_text_reaches_the_logs() {
    tracing::subscriber::set_global_default(Recorder).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        pair_once(RIGHT_CODE, RIGHT_CODE).await.unwrap();
        assert!(matches!(
            pair_once(RIGHT_CODE, WRONG_CODE).await,
            Err(PairError::WrongCode)
        ));
        run_session().await;
    });

    let logs = CAPTURE.text.lock().unwrap().clone();
    assert!(logs.contains("connected"), "the capture works:\n{logs}");
    assert!(logs.contains("pairing attempt failed"), "{logs}");
    let key_code = KeyCode::keyboard(KEY).0;
    let forbidden = [
        RIGHT_CODE.to_owned(),
        format!("{} {}", &RIGHT_CODE[..3], &RIGHT_CODE[3..]),
        WRONG_CODE.to_owned(),
        format!("{} {}", &WRONG_CODE[..3], &WRONG_CODE[3..]),
        key_code.to_string(),
        format!("{key_code:x}"),
        format!("KeyCode({key_code})"),
        SECRET_TEXT.to_owned(),
    ];
    for secret in forbidden {
        assert!(!logs.contains(&secret), "{secret:?} appears in the logs");
    }
}
