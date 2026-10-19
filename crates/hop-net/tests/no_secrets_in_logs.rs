//! Runs pairing and a session with every log event captured, at every
//! level and from every crate (quinn's included), and checks that no
//! pairing code, key code, clipboard text, file name or file content
//! appears in them.
//!
//! This is its own test binary because it installs a process-wide log
//! subscriber. Everything binds 127.0.0.1; nothing is advertised, and the
//! files live in a temporary folder.

use std::convert::Infallible;
use std::fmt::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hop_net::{
    FilesConfig, Identity, NetConfig, NetEvent, PairError, PairInfo, PairingCode, PairingHost,
    PairingTiming, PinnedPeer, TransferEvent, pair_join,
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
const SECRET_FILE_NAME: &str = "tax-return-krish-2026.pdf";
const SECRET_FILE_STEM: &str = "tax-return-krish-2026";
const SECRET_CONTENTS: &str = "account 4417 1234 5678 9113";

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

/// A session with the clipboard and files on: input, clipboard text and a
/// file travel from `a` to `b`. `folder` holds the file and both Received
/// folders.
async fn run_session(folder: &Path) {
    let a_id = Arc::new(Identity::generate().unwrap());
    let b_id = Arc::new(Identity::generate().unwrap());
    let source = folder.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join(SECRET_FILE_NAME), SECRET_CONTENTS).unwrap();
    let config = |name: &str, peer: &Identity, received: PathBuf| {
        let mut config = NetConfig::new(0, name, "0.1.0");
        config.bind = loopback();
        config.discovery = false;
        config.peers = vec![PinnedPeer {
            id: peer.device_id(),
            fingerprint: peer.fingerprint(),
            name: "peer".to_owned(),
            address: None,
        }];
        config.clipboard = true;
        config.files = Some(FilesConfig {
            received_dir: received,
        });
        config
    };
    let (b, b_events) = hop_net::start(b_id.clone(), config("b", &a_id, folder.join("b-in")))
        .await
        .unwrap();
    let (a, a_events) = hop_net::start(a_id.clone(), config("a", &b_id, folder.join("a-in")))
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
    // Both sides turned the clipboard on, so it travels; it must still
    // never be logged.
    let clipboard = CoreMessage::ClipboardText {
        generation: 1,
        text: SECRET_TEXT.to_owned(),
    };
    a.send(&b_id.device_id(), clipboard.clone()).unwrap();
    let id = a
        .send_files(&b_id.device_id(), vec![source.join(SECRET_FILE_NAME)])
        .unwrap();
    let mut messages = Vec::new();
    let mut saved = None;
    while messages.len() < 2 || saved.is_none() {
        match b_events.try_recv() {
            Some(NetEvent::Message { msg, .. }) => messages.push(msg),
            Some(NetEvent::Transfer(TransferEvent::Received { files, .. })) => saved = Some(files),
            Some(_) => {}
            None => {
                assert!(tokio::time::Instant::now() < deadline, "nothing arrived");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    assert_eq!(messages, [input, clipboard]);
    let saved = saved.unwrap();
    assert_eq!(saved, [folder.join("b-in").join(SECRET_FILE_NAME)]);
    assert_eq!(std::fs::read_to_string(&saved[0]).unwrap(), SECRET_CONTENTS);
    loop {
        match a_events.try_recv() {
            Some(NetEvent::Transfer(event)) => {
                assert!(
                    matches!(event, TransferEvent::Sent { id: sent, files: 1, .. } if sent == id)
                );
                break;
            }
            Some(_) => {}
            None => {
                assert!(tokio::time::Instant::now() < deadline, "not sent");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn no_code_key_clipboard_text_or_file_reaches_the_logs() {
    tracing::subscriber::set_global_default(Recorder).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let folder = std::env::temp_dir().join(format!("hop-net-no-secrets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    runtime.block_on(async {
        pair_once(RIGHT_CODE, RIGHT_CODE).await.unwrap();
        assert!(matches!(
            pair_once(RIGHT_CODE, WRONG_CODE).await,
            Err(PairError::WrongCode)
        ));
        run_session(&folder).await;
    });
    let _ = std::fs::remove_dir_all(&folder);

    let logs = CAPTURE.text.lock().unwrap().clone();
    assert!(logs.contains("connected"), "the capture works:\n{logs}");
    assert!(logs.contains("pairing attempt failed"), "{logs}");
    assert!(logs.contains("received files"), "{logs}");
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
        SECRET_FILE_STEM.to_owned(),
        SECRET_CONTENTS.to_owned(),
        // The file's checksum, as hex either way round.
        digest_hex(SECRET_CONTENTS.as_bytes()),
        digest_hex(SECRET_CONTENTS.as_bytes()).to_uppercase(),
    ];
    for secret in forbidden {
        assert!(!logs.contains(&secret), "{secret:?} appears in the logs");
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
