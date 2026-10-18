//! Tests of [`PairSession`] over loopback, each with files of its own (never
//! the real config or key). Nothing is announced on the network: the
//! showing side listens on 127.0.0.1 with discovery off, and the joining
//! side is always given an address, so no multicast is sent.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use hop_config::{Config, Side};
use hop_net::{Identity, PairError, PairInfo, PairedPeer, PairingCode, PairingHost, PairingTiming};
use hop_types::Os;
use zeroize::Zeroizing;

use super::session::{CANCELLED, DECLINED, NO_ANSWER, app_reason};
use super::{PairEvent, PairRole, PairSession, Saved, Setup};
use crate::startup;

/// How long a test waits for any one event.
const EVENT_WAIT: Duration = Duration::from_secs(20);

/// A fresh folder for one test's files.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hop-pairing-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A setup in `dir` on loopback, with discovery off; it says where it
/// listens on `listening`.
fn loopback(
    dir: &Path,
    timing: PairingTiming,
    listening: Option<mpsc::Sender<SocketAddr>>,
) -> impl FnOnce() -> anyhow::Result<Setup> + Send + 'static {
    let dir = dir.to_owned();
    move || {
        let startup = startup::load_from(dir.join("config.toml"), &dir.join("identity.pem"))?;
        let mut setup = Setup::new(startup)?;
        setup.listen_ip = Ipv4Addr::LOCALHOST;
        setup.port = 0;
        setup.advertise = false;
        setup.timing = timing;
        setup.listening = listening;
        Ok(setup)
    }
}

fn next(events: &Receiver<PairEvent>) -> PairEvent {
    events.recv_timeout(EVENT_WAIT).expect("an event in time")
}

/// The events up to [`PairEvent::Finished`], and its result. Checks that
/// nothing follows it.
fn until_finished(events: &Receiver<PairEvent>) -> (Vec<PairEvent>, Result<Saved, String>) {
    let mut before = Vec::new();
    loop {
        match next(events) {
            PairEvent::Finished(result) => {
                assert!(
                    events.recv_timeout(Duration::from_millis(200)).is_err(),
                    "Finished is the last event"
                );
                return (before, result);
            }
            event => before.push(event),
        }
    }
}

/// Waits for the session's thread to end after Finished.
fn ends(session: &PairSession) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.is_finished() {
        assert!(Instant::now() < deadline, "the session's thread ended");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The test's own network runtime, for the other computer's side.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn peers(dir: &Path) -> Vec<hop_config::PeerConfig> {
    Config::load(dir.join("config.toml"))
        .expect("config")
        .0
        .peers
}

/// This computer's key, as the session made it in `dir`.
fn own_key(dir: &Path) -> Identity {
    Identity::load_or_create(&dir.join("identity.pem"))
        .expect("key")
        .identity
}

/// A showing session in `dir`: its code and where it listens.
fn show(
    dir: &Path,
    timing: PairingTiming,
) -> (
    PairSession,
    Receiver<PairEvent>,
    Zeroizing<String>,
    SocketAddr,
) {
    let (tx, events) = mpsc::channel();
    let (listening_tx, listening) = mpsc::channel();
    let session = PairSession::spawn(
        PairRole::Show,
        tx,
        loopback(dir, timing, Some(listening_tx)),
    )
    .expect("session");
    let first = next(&events);
    let PairEvent::Code(code) = first else {
        panic!("the code comes first: {first:?}");
    };
    let address = listening.recv_timeout(EVENT_WAIT).expect("listening");
    (session, events, code, address)
}

/// The other computer typing `code`, with hop-net's own joiner.
fn join_with(
    runtime: &tokio::runtime::Runtime,
    code: &str,
    address: SocketAddr,
    timing: PairingTiming,
) -> (
    Arc<Identity>,
    tokio::task::JoinHandle<Result<PairedPeer, PairError>>,
) {
    let key = Arc::new(Identity::generate().expect("key"));
    let code = PairingCode::parse(code).expect("code");
    let me = PairInfo {
        name: "Test PC".to_owned(),
        os: Os::Windows,
    };
    let joiner = key.clone();
    let joining = runtime.spawn(async move {
        hop_net::pair_join(joiner, me, code, &[address], timing, |_| {}).await
    });
    (key, joining)
}

#[test]
fn a_shown_code_comes_first_and_cancel_ends_the_session_at_once() {
    let dir = scratch("cancel");
    let (session, events, code, _) = show(&dir, PairingTiming::default());
    let digits: String = code.chars().filter(|c| *c != ' ').collect();
    assert!(
        code.len() == 7 && digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()),
        "shown as \"482 913\""
    );
    let cancelled = Instant::now();
    session.cancel();
    let (before, result) = until_finished(&events);
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(result, Err(CANCELLED.to_owned()));
    assert!(
        cancelled.elapsed() < Duration::from_secs(3),
        "{:?}",
        cancelled.elapsed()
    );
    ends(&session);
    assert!(peers(&dir).is_empty(), "nothing saved");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cancel_before_the_session_gets_going_shows_no_code() {
    let dir = scratch("cancel-early");
    let (tx, events) = mpsc::channel();
    let session = PairSession::spawn(
        PairRole::Show,
        tx,
        loopback(&dir, PairingTiming::default(), None),
    )
    .expect("session");
    session.cancel();
    let (before, result) = until_finished(&events);
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(result, Err(CANCELLED.to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saying_yes_pairs_the_computer_that_typed_the_code() {
    // Every log line from here on, from every crate and at every level.
    let _ = crate::logging::tests::capture();
    let dir = scratch("yes");
    let (session, events, code, address) = show(&dir, PairingTiming::default());
    let runtime = runtime();
    let (joiner, joining) = join_with(&runtime, &code, address, PairingTiming::default());

    let confirm = next(&events);
    let PairEvent::Confirm {
        name,
        os,
        replaces,
        within,
    } = confirm
    else {
        panic!("the question: {confirm:?}");
    };
    assert_eq!(
        (name.as_str(), os, replaces),
        ("Test PC", Os::Windows, None)
    );
    assert!(within > Duration::from_secs(60) && within <= Duration::from_secs(120));
    session.answer(true);

    let (before, result) = until_finished(&events);
    assert!(before.is_empty(), "{before:?}");
    let saved = result.expect("paired");
    assert_eq!((saved.name.as_str(), saved.os), ("Test PC", Os::Windows));
    assert_eq!(
        saved.side,
        Side::Right,
        "a Mac or a PC puts a PC on its right"
    );
    assert_eq!(saved.path, dir.join("config.toml"));
    let pinned = peers(&dir);
    assert_eq!(pinned.len(), 1);
    assert_eq!(
        pinned[0].pinned_fingerprint(),
        Some(joiner.fingerprint().to_hex().as_str())
    );
    // The other computer heard the yes and learned this computer's key.
    let peer = runtime
        .block_on(joining)
        .expect("task")
        .expect("joiner paired");
    assert_eq!(peer.fingerprint, own_key(&dir).fingerprint());
    ends(&session);
    // The code was shown, and only shown.
    let log = crate::logging::tests::captured_text();
    assert!(!log.contains(code.as_str()), "the log has the code");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saying_no_or_not_answering_saves_nothing() {
    // No: both computers hear it.
    let dir = scratch("no");
    let (session, events, code, address) = show(&dir, PairingTiming::default());
    let runtime = runtime();
    let (_, joining) = join_with(&runtime, &code, address, PairingTiming::default());
    assert!(matches!(next(&events), PairEvent::Confirm { .. }));
    session.answer(false);
    let (_, result) = until_finished(&events);
    assert_eq!(result, Err(DECLINED.to_owned()));
    let joined = runtime.block_on(joining).expect("task");
    assert!(matches!(joined, Err(PairError::Declined)), "{joined:?}");
    assert!(peers(&dir).is_empty());
    ends(&session);
    let _ = std::fs::remove_dir_all(&dir);

    // No answer before the code runs out counts as no. An answer given
    // before the question is asked does not count either.
    let dir = scratch("no-answer");
    let short = PairingTiming {
        code_lifetime: Duration::from_secs(3),
        ..PairingTiming::default()
    };
    let (session, events, code, address) = show(&dir, short);
    session.answer(true);
    let (_, joining) = join_with(&runtime, &code, address, PairingTiming::default());
    let confirm = next(&events);
    let PairEvent::Confirm { within, .. } = confirm else {
        panic!("the question: {confirm:?}");
    };
    assert!(within <= Duration::from_secs(3));
    let (_, result) = until_finished(&events);
    assert_eq!(result, Err(NO_ANSWER.to_owned()));
    assert!(runtime.block_on(joining).expect("task").is_err());
    assert!(peers(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_wrong_code_is_counted_and_the_session_keeps_waiting() {
    let dir = scratch("wrong");
    let (session, events, code, address) = show(&dir, PairingTiming::default());
    let wrong = if code.as_str() == "000 000" {
        "111111"
    } else {
        "000000"
    };
    let runtime = runtime();
    let (_, joining) = join_with(&runtime, wrong, address, PairingTiming::default());
    let joined = runtime.block_on(joining).expect("task");
    assert!(matches!(joined, Err(PairError::WrongCode)), "{joined:?}");
    let event = next(&events);
    assert!(
        matches!(event, PairEvent::WrongCode { failures: 1 }),
        "{event:?}"
    );
    session.cancel();
    let (before, result) = until_finished(&events);
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(result, Err(CANCELLED.to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn joining_pairs_with_the_computer_showing_the_code() {
    let dir = scratch("join");
    let runtime = runtime();
    let host_key = Arc::new(Identity::generate().expect("key"));
    let code = PairingCode::generate().expect("code");
    let shown = Zeroizing::new(code.reveal());
    let host = runtime
        .block_on(PairingHost::bind(
            host_key.clone(),
            PairInfo {
                name: "Test Mac".to_owned(),
                os: Os::MacOs,
            },
            SocketAddr::from(([127, 0, 0, 1], 0)),
            code,
            PairingTiming::default(),
            false,
        ))
        .expect("host");
    let address = host.local_addr();
    let hosting = runtime.spawn(async move {
        let pending = host.wait_for_joiner().await?;
        pending
            .accept(|_| Ok::<(), std::convert::Infallible>(()))
            .await
    });

    let (tx, events) = mpsc::channel();
    let session = PairSession::spawn(
        PairRole::Join {
            code: shown,
            address: Some(address.to_string()),
        },
        tx,
        loopback(&dir, PairingTiming::default(), None),
    )
    .expect("session");
    let (before, result) = until_finished(&events);
    let names: Vec<String> = before.iter().map(|event| format!("{event:?}")).collect();
    assert_eq!(
        names,
        [
            "Connecting",
            "WaitingForConfirmation { host: \"Test Mac\" }"
        ]
    );
    let saved = result.expect("paired");
    assert_eq!((saved.name.as_str(), saved.os), ("Test Mac", Os::MacOs));
    let natural = if Os::current() == Os::MacOs {
        Side::Right
    } else {
        Side::Left
    };
    assert_eq!(saved.side, natural, "a PC puts a Mac on its left");
    assert_eq!(
        peers(&dir)[0].pinned_fingerprint(),
        Some(host_key.fingerprint().to_hex().as_str())
    );
    let joiner = runtime
        .block_on(hosting)
        .expect("task")
        .expect("host paired");
    assert_eq!(joiner.fingerprint, own_key(&dir).fingerprint());
    ends(&session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_wrong_code_typed_here_says_to_check_it_and_click_pair_again() {
    let dir = scratch("join-wrong");
    let runtime = runtime();
    let code = PairingCode::generate().expect("code");
    let wrong = if code.reveal() == "000 000" {
        "111 111"
    } else {
        "000 000"
    };
    let host = runtime
        .block_on(PairingHost::bind(
            Arc::new(Identity::generate().expect("key")),
            PairInfo {
                name: "Test PC".to_owned(),
                os: Os::Windows,
            },
            SocketAddr::from(([127, 0, 0, 1], 0)),
            code,
            PairingTiming::default(),
            false,
        ))
        .expect("host");
    let address = host.local_addr();
    let hosting = runtime.spawn(async move { host.wait_for_joiner().await.map(drop) });

    let (tx, events) = mpsc::channel();
    let session = PairSession::spawn(
        PairRole::Join {
            code: Zeroizing::new(wrong.to_owned()),
            address: Some(address.to_string()),
        },
        tx,
        loopback(&dir, PairingTiming::default(), None),
    )
    .expect("session");
    let (_, result) = until_finished(&events);
    let reason = result.expect_err("not paired");
    assert_eq!(reason, app_reason(&PairError::WrongCode, true));
    assert!(reason.contains("click Pair again"), "{reason}");
    assert!(peers(&dir).is_empty());
    ends(&session);
    hosting.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_apps_reasons_never_send_the_user_to_the_terminal() {
    let errors = [
        PairError::InvalidCode,
        PairError::Random,
        PairError::Bind {
            addr: SocketAddr::from(([0, 0, 0, 0], 47801)),
            source: std::io::Error::from(std::io::ErrorKind::AddrInUse),
        },
        PairError::Setup("no TLS".to_owned()),
        PairError::Expired,
        PairError::TooManyAttempts,
        PairError::WrongCode,
        PairError::Busy,
        PairError::Declined,
        PairError::Unreachable,
        PairError::NoAddress,
        PairError::Protocol("a bad frame".to_owned()),
        PairError::Connection("timed out".to_owned()),
        // hop-net's own connection texts, which the app passes on.
        PairError::Connection("the other laptop stopped answering".to_owned()),
        PairError::Connection("the pairing listener stopped".to_owned()),
        PairError::Connection(
            "this laptop saved the pairing, but the other laptop may not have received the \
             confirmation; if it reports a failure, run hop pair again on both laptops"
                .to_owned(),
        ),
        PairError::Save("disk full".to_owned()),
        PairError::SelfPairing,
        PairError::Discovery("mDNS: no interface".to_owned()),
    ];
    for error in &errors {
        for joining in [false, true] {
            let reason = app_reason(error, joining);
            assert!(!reason.contains("laptop"), "{error:?}: {reason}");
            // A port in use may well be hop pair's, in a terminal.
            if !matches!(error, PairError::Bind { .. }) {
                assert!(!reason.contains("hop pair"), "{error:?}: {reason}");
            }
        }
    }
    let taken = app_reason(&errors[2], false);
    assert!(
        taken.contains("0.0.0.0:47801") && taken.contains("another copy of Hop"),
        "{taken}"
    );
}

#[test]
fn cancelling_while_the_other_computer_asks_tells_it_so_it_saves_nothing() {
    // This computer typed the code and waits for the yes; the other one
    // (hop-net's own host here) is asking its user.
    let dir = scratch("cancel-join");
    let runtime = runtime();
    let host_key = Arc::new(Identity::generate().expect("key"));
    let code = PairingCode::generate().expect("code");
    let shown = Zeroizing::new(code.reveal());
    let host = runtime
        .block_on(PairingHost::bind(
            host_key,
            PairInfo {
                name: "Test Mac".to_owned(),
                os: Os::MacOs,
            },
            SocketAddr::from(([127, 0, 0, 1], 0)),
            code,
            PairingTiming::default(),
            false,
        ))
        .expect("host");
    let address = host.local_addr();
    let asking = runtime.spawn(host.wait_for_joiner());
    let (tx, events) = mpsc::channel();
    let session = PairSession::spawn(
        PairRole::Join {
            code: shown,
            address: Some(address.to_string()),
        },
        tx,
        loopback(&dir, PairingTiming::default(), None),
    )
    .expect("session");
    loop {
        if let PairEvent::WaitingForConfirmation { .. } = next(&events) {
            break;
        }
    }
    let pending = runtime
        .block_on(asking)
        .expect("task")
        .expect("the host is asking");
    session.cancel();
    let (_, result) = until_finished(&events);
    assert_eq!(result, Err(CANCELLED.to_owned()));
    ends(&session);
    // The cancel's close has left this computer by the time the session
    // ends; loopback delivers it at once.
    std::thread::sleep(Duration::from_millis(100));
    let mut saved = false;
    let accepted = runtime.block_on(pending.accept(|_| {
        saved = true;
        Ok::<(), std::convert::Infallible>(())
    }));
    assert!(accepted.is_err(), "{accepted:?}");
    assert!(!saved, "a yes after the cancel saves nothing there");
    assert!(peers(&dir).is_empty(), "nor here");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn closing_while_this_computer_asks_tells_the_other_one_no() {
    let dir = scratch("cancel-ask");
    let (session, events, code, address) = show(&dir, PairingTiming::default());
    let runtime = runtime();
    let (_, joining) = join_with(&runtime, &code, address, PairingTiming::default());
    assert!(matches!(next(&events), PairEvent::Confirm { .. }));
    // The window closes (or Cancel is clicked) while the question is open.
    session.cancel();
    let (_, result) = until_finished(&events);
    assert_eq!(result, Err(CANCELLED.to_owned()));
    let started = Instant::now();
    let joined = runtime.block_on(joining).expect("task");
    assert!(matches!(joined, Err(PairError::Declined)), "{joined:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the other computer hears at once, not after a timeout"
    );
    assert!(peers(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bad_code_or_address_ends_the_session_with_a_sentence() {
    let dir = scratch("bad");
    for (code, address, expected) in [
        (
            "48291",
            Some("127.0.0.1:9"),
            "the pairing code must be 6 digits",
        ),
        (
            "482 913",
            Some("[::1]:47801"),
            "IPv6 addresses are not supported",
        ),
    ] {
        let (tx, events) = mpsc::channel();
        let _session = PairSession::spawn(
            PairRole::Join {
                code: Zeroizing::new(code.to_owned()),
                address: address.map(str::to_owned),
            },
            tx,
            loopback(&dir, PairingTiming::default(), None),
        )
        .expect("session");
        let (before, result) = until_finished(&events);
        assert!(before.is_empty(), "{before:?}");
        let error = result.expect_err("not paired");
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains(code), "the code is never repeated: {error}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_apps_fields_are_trimmed_and_an_empty_address_means_look_for_it() {
    let tidied = super::session::tidy(PairRole::Join {
        code: Zeroizing::new(" 482 913\n".to_owned()),
        address: Some("  ".to_owned()),
    });
    let PairRole::Join { code, address } = tidied else {
        panic!("still joining");
    };
    assert_eq!((code.as_str(), address), ("482 913", None));
    let PairRole::Join { address, .. } = super::session::tidy(PairRole::Join {
        code: Zeroizing::new("482913".to_owned()),
        address: Some(" 192.168.1.20 ".to_owned()),
    }) else {
        panic!("still joining");
    };
    assert_eq!(address.as_deref(), Some("192.168.1.20"));
}

#[test]
fn debug_output_never_shows_the_code() {
    let role = PairRole::Join {
        code: Zeroizing::new("482913".to_owned()),
        address: Some("192.168.1.20".to_owned()),
    };
    let text = format!("{role:?}");
    assert!(
        !text.contains("482913") && text.contains("192.168.1.20"),
        "{text}"
    );
    let event = PairEvent::Code(Zeroizing::new("482 913".to_owned()));
    let text = format!("{event:?}");
    assert!(!text.contains("482") && !text.contains("913"), "{text}");
}
