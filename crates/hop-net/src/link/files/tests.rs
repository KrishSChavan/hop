//! Loopback tests of file transfers. Every socket binds 127.0.0.1 and
//! every folder is a fresh one under the system's temporary folder,
//! deleted when the test ends.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hop_proto::{
    Capability, Decoded, FileEntry, FileFailure, FileMessage, FileRefusal, FrameDecoder, Hello,
    Message, caps, encode_frame,
};
use hop_types::DeviceId;
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;

use super::super::tests::{
    Node, heartbeat, key_input, node_with, pair_with, pin, raw_connect, raw_session,
};
use super::{Limits, check_offer};
use crate::{
    DownReason, FilesConfig, Identity, NetConfig, NetEvent, Refusal, SendError, TransferEvent,
    TransferFailure,
};

/// A temporary folder, unique per test and run, deleted at the end even
/// when the test fails.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Scratch {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hop-net-files-{test}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// A new folder `name` in this one, with the files given.
    fn folder(&self, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let dir = self.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (file, content) in files {
            fs::write(dir.join(file), content).unwrap();
        }
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The names in `dir`, sorted; none if it does not exist.
fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The temporary files in `dir`.
fn temporary_files(dir: &Path) -> Vec<String> {
    names_in(dir)
        .into_iter()
        .filter(|name| name.ends_with(hop_transfer::TEMP_SUFFIX))
        .collect()
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Receiving files into `dir`.
fn receiving(dir: &Path) -> impl FnOnce(&mut NetConfig) + use<> {
    let dir = dir.to_path_buf();
    move |config| {
        config.files = Some(FilesConfig { received_dir: dir });
    }
}

/// Receiving files into `dir`, with `limits` changed.
fn receiving_with<L: FnOnce(&mut Limits)>(
    dir: &Path,
    limits: L,
) -> impl FnOnce(&mut NetConfig) + use<L> {
    let receive = receiving(dir);
    move |config| {
        receive(config);
        limits(&mut config.limits);
    }
}

/// The next transfer event on `node`.
async fn expect_transfer(node: &mut Node) -> TransferEvent {
    match node
        .expect("a transfer event", |e| matches!(e, NetEvent::Transfer(_)))
        .await
    {
        NetEvent::Transfer(event) => event,
        _ => unreachable!(),
    }
}

/// `len` bytes that differ from one position to the next.
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i % 251) as u8 ^ seed.wrapping_add((i / 251) as u8))
        .collect()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn sent(to: &Node, id: u64, files: u32, skipped: u32) -> TransferEvent {
    TransferEvent::Sent {
        to: to.id(),
        id,
        files,
        skipped,
    }
}

fn failed(peer: DeviceId, id: u64, outgoing: bool, reason: TransferFailure) -> TransferEvent {
    TransferEvent::Failed {
        peer,
        id,
        outgoing,
        reason,
    }
}

/// Two connected nodes that both receive files, `b` with `edit_b` applied
/// too.
async fn file_pair(scratch: &Scratch, edit_b: impl FnOnce(&mut Limits)) -> (Node, Node, PathBuf) {
    let received = scratch.join("received");
    let (mut a, mut b) = pair_with(
        receiving(&scratch.join("a-received")),
        receiving_with(&received, edit_b),
    )
    .await;
    assert!(a.expect_up().await.supports_files());
    assert!(b.expect_up().await.supports_files());
    (a, b, received)
}

// ---- Between two nodes ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_arrive_intact_with_clean_names_that_never_clobber() {
    let scratch = Scratch::new("intact");
    let report = content(300_000, 1);
    let notes = content(70_000, 2);
    // A zero-width space is allowed on both systems and removed on arrival.
    let source = scratch.folder(
        "source",
        &[
            ("report.pdf", &report),
            ("empty.txt", b""),
            ("notes\u{200B}.txt", &notes),
        ],
    );
    fs::create_dir(source.join("folder")).unwrap();
    let received = scratch.folder("received", &[("report.pdf", b"already here")]);
    let (mut a, mut b) =
        pair_with(receiving(&scratch.join("a-received")), receiving(&received)).await;
    a.expect_up().await;
    b.expect_up().await;

    let paths = vec![
        source.join("report.pdf"),
        source.join("folder"),
        source.join("missing.txt"),
        source.join("empty.txt"),
        source.join("notes\u{200B}.txt"),
    ];
    let id = a.handle.send_files(&b.id(), paths).unwrap();
    assert_eq!(expect_transfer(&mut a).await, sent(&b, id, 3, 2));
    let saved = [
        received.join("report (2).pdf"),
        received.join("empty.txt"),
        received.join("notes.txt"),
    ];
    assert_eq!(
        expect_transfer(&mut b).await,
        TransferEvent::Received {
            from: a.id(),
            id,
            files: saved.to_vec()
        }
    );
    assert_eq!(fs::read(&saved[0]).unwrap(), report);
    assert_eq!(fs::read(&saved[1]).unwrap(), b"");
    assert_eq!(fs::read(&saved[2]).unwrap(), notes);
    // On Windows each one carries the Mark of the Web, as a download does.
    #[cfg(windows)]
    for path in &saved {
        let mut stream = path.clone().into_os_string();
        stream.push(":Zone.Identifier");
        assert_eq!(
            fs::read_to_string(stream).unwrap(),
            "[ZoneTransfer]\r\nZoneId=3\r\n"
        );
    }
    assert_eq!(
        fs::read(received.join("report.pdf")).unwrap(),
        b"already here"
    );
    assert_eq!(
        names_in(&received),
        ["empty.txt", "notes.txt", "report (2).pdf", "report.pdf"]
    );
    // The sources are untouched, and nothing reached a's own folder.
    assert_eq!(fs::read(source.join("report.pdf")).unwrap(), report);
    assert!(!scratch.join("a-received").exists());

    // The same files again, the other way round too.
    let again = a
        .handle
        .send_files(&b.id(), vec![source.join("report.pdf")])
        .unwrap();
    assert!(again > id);
    assert_eq!(expect_transfer(&mut a).await, sent(&b, again, 1, 0));
    let TransferEvent::Received { files, .. } = expect_transfer(&mut b).await else {
        panic!("received");
    };
    assert_eq!(files, [received.join("report (3).pdf")]);
    let back = b
        .handle
        .send_files(&a.id(), vec![received.join("notes.txt")])
        .unwrap();
    assert_eq!(expect_transfer(&mut b).await, sent(&a, back, 1, 0));
    let TransferEvent::Received { files, .. } = expect_transfer(&mut a).await else {
        panic!("received");
    };
    assert_eq!(files, [scratch.join("a-received").join("notes.txt")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_received_folder_is_untouched_until_the_first_offer() {
    let scratch = Scratch::new("untouched");
    let source = scratch.folder("source", &[("a.txt", b"alpha")]);
    let (mut a, mut b, received) = file_pair(&scratch, |_| {}).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!received.exists(), "connecting created the folder");
    let id = a
        .handle
        .send_files(&b.id(), vec![source.join("a.txt")])
        .unwrap();
    assert_eq!(expect_transfer(&mut a).await, sent(&b, id, 1, 0));
    expect_transfer(&mut b).await;
    assert_eq!(names_in(&received), ["a.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leftovers_from_an_earlier_run_are_swept_at_the_first_offer() {
    let scratch = Scratch::new("sweep");
    let source = scratch.folder("source", &[("new.txt", b"new")]);
    let received = scratch.folder(
        "received",
        &[(".old.pdf.hop-part", b"half a file"), ("keep.txt", b"keep")],
    );
    let (mut a, mut b) =
        pair_with(receiving(&scratch.join("a-received")), receiving(&received)).await;
    a.expect_up().await;
    b.expect_up().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        names_in(&received),
        [".old.pdf.hop-part", "keep.txt"],
        "not before the first offer"
    );
    let id = a
        .handle
        .send_files(&b.id(), vec![source.join("new.txt")])
        .unwrap();
    assert_eq!(expect_transfer(&mut a).await, sent(&b, id, 1, 0));
    assert_eq!(names_in(&received), ["keep.txt", "new.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfers_queue_up_one_after_the_other() {
    let scratch = Scratch::new("queue");
    let first = content(500_000, 3);
    let source = scratch.folder("source", &[("one.bin", &first), ("two.txt", b"two")]);
    let (mut a, mut b, received) = file_pair(&scratch, |_| {}).await;
    let one = a
        .handle
        .send_files(&b.id(), vec![source.join("one.bin")])
        .unwrap();
    let two = a
        .handle
        .send_files(&b.id(), vec![source.join("two.txt")])
        .unwrap();
    assert_eq!(expect_transfer(&mut a).await, sent(&b, one, 1, 0));
    assert_eq!(expect_transfer(&mut a).await, sent(&b, two, 1, 0));
    for (id, name) in [(one, "one.bin"), (two, "two.txt")] {
        assert_eq!(
            expect_transfer(&mut b).await,
            TransferEvent::Received {
                from: a.id(),
                id,
                files: vec![received.join(name)]
            }
        );
    }
    assert_eq!(fs::read(received.join("one.bin")).unwrap(), first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sending_needs_a_connected_peer_that_negotiated_files() {
    let scratch = Scratch::new("negotiated");
    let source = scratch.folder("source", &[("a.txt", b"alpha")]);
    // Only `a` receives files, so `files` is not negotiated.
    let (mut a, mut b) = pair_with(receiving(&scratch.join("a-received")), |_| {}).await;
    assert!(!a.expect_up().await.supports_files());
    assert!(!b.expect_up().await.supports_files());
    let paths = vec![source.join("a.txt")];
    assert!(matches!(
        a.handle.send_files(&b.id(), paths.clone()),
        Err(SendError::NotNegotiated)
    ));
    assert!(matches!(
        b.handle.send_files(&a.id(), paths.clone()),
        Err(SendError::NotNegotiated)
    ));
    assert!(matches!(
        a.handle.send_files(&DeviceId::new("nobody"), paths),
        Err(SendError::NotConnected)
    ));
    a.expect_quiet(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_sendable_fails_without_an_offer() {
    let scratch = Scratch::new("nothing");
    let source = scratch.folder("source", &[]);
    fs::create_dir(source.join("folder")).unwrap();
    let (mut a, mut b, received) = file_pair(&scratch, |_| {}).await;
    let id = a
        .handle
        .send_files(&b.id(), vec![source.join("folder"), source.join("gone")])
        .unwrap();
    assert_eq!(
        expect_transfer(&mut a).await,
        failed(b.id(), id, true, TransferFailure::NothingToSend)
    );
    b.expect_quiet(Duration::from_millis(200)).await;
    assert!(!received.exists(), "no offer reached b");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_files_than_one_offer_can_list_are_refused_before_sending() {
    let scratch = Scratch::new("too-many-here");
    let source = scratch.folder("source", &[]);
    let paths: Vec<PathBuf> = (0..=hop_proto::MAX_OFFER_FILES)
        .map(|index| {
            let path = source.join(format!("{index}.txt"));
            fs::write(&path, b"").unwrap();
            path
        })
        .collect();
    let (mut a, mut b, received) = file_pair(&scratch, |_| {}).await;
    let id = a.handle.send_files(&b.id(), paths).unwrap();
    assert_eq!(
        expect_transfer(&mut a).await,
        failed(b.id(), id, true, TransferFailure::Refused(Refusal::TooMany))
    );
    b.expect_quiet(Duration::from_millis(200)).await;
    assert!(!received.exists(), "no offer reached b");
}

/// Sends `files` from a fresh pair whose receiver has `limits` (and folder
/// `received`, if given), and returns why the sender failed.
async fn refusal_for(
    test: &str,
    files: &[(&str, &[u8])],
    received: Option<&dyn Fn(&Scratch) -> PathBuf>,
    limits: impl FnOnce(&mut Limits),
) -> TransferFailure {
    let scratch = Scratch::new(test);
    let source = scratch.folder("source", files);
    let received = received.map_or_else(|| scratch.join("received"), |make| make(&scratch));
    let (mut a, mut b) = pair_with(
        receiving(&scratch.join("a-received")),
        receiving_with(&received, limits),
    )
    .await;
    a.expect_up().await;
    b.expect_up().await;
    let paths = files.iter().map(|(name, _)| source.join(name)).collect();
    let id = a.handle.send_files(&b.id(), paths).unwrap();
    let TransferEvent::Failed {
        id: failed_id,
        outgoing: true,
        reason,
        ..
    } = expect_transfer(&mut a).await
    else {
        panic!("a refusal: {:?}", a.log);
    };
    assert_eq!(failed_id, id);
    // The receiving side tells its own agent only about a cause that is
    // its own to fix: a Received folder it cannot use.
    if reason == TransferFailure::Refused(Refusal::Off) {
        assert_eq!(
            expect_transfer(&mut b).await,
            failed(a.id(), id, false, TransferFailure::Refused(Refusal::Off))
        );
    }
    b.expect_quiet(Duration::from_millis(100)).await;
    assert!(names_in(&received).is_empty());
    reason
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_receiver_refuses_what_it_cannot_take() {
    let refused = |refusal| TransferFailure::Refused(refusal);
    let one: &[(&str, &[u8])] = &[("a.txt", b"0123456789A")];
    // A folder that cannot be created: a file stands where it should be.
    let blocked: &dyn Fn(&Scratch) -> PathBuf = &|scratch| {
        fs::write(scratch.join("blocker"), b"a file").unwrap();
        scratch.join("blocker").join("Received")
    };
    assert_eq!(
        refusal_for("off", one, Some(blocked), |_| {}).await,
        refused(Refusal::Off)
    );
    let three: &[(&str, &[u8])] = &[("a", b"1"), ("b", b"2"), ("c", b"3")];
    assert_eq!(
        refusal_for("too-many", three, None, |limits| limits
            .max_transfer_files =
            2)
        .await,
        refused(Refusal::TooMany)
    );
    assert_eq!(
        refusal_for("too-large", one, None, |limits| limits.max_file_bytes = 10).await,
        refused(Refusal::TooLarge)
    );
    let two: &[(&str, &[u8])] = &[("a", b"123456"), ("b", b"123456")];
    assert_eq!(
        refusal_for("too-large-in-all", two, None, |limits| {
            limits.max_transfer_bytes = 10;
        })
        .await,
        refused(Refusal::TooLarge)
    );
}

#[test]
fn offers_are_checked_against_the_limits() {
    let limits = Limits {
        max_transfer_files: 3,
        max_file_bytes: 100,
        max_transfer_bytes: 150,
        ..Limits::default()
    };
    let entry = |name: &str, size| FileEntry {
        name: name.to_owned(),
        size,
    };
    assert_eq!(
        check_offer(&[entry("a", 100), entry("b", 50)], &limits),
        Ok(())
    );
    assert_eq!(
        check_offer(
            &[entry("a", 1), entry("b", 1), entry("c", 1), entry("d", 1)],
            &limits
        ),
        Err(FileRefusal::TooMany)
    );
    assert_eq!(
        check_offer(&[entry("a", 101)], &limits),
        Err(FileRefusal::TooLarge)
    );
    assert_eq!(
        check_offer(&[entry("a", 100), entry("b", 51)], &limits),
        Err(FileRefusal::TooLarge)
    );
    let huge = Limits {
        max_file_bytes: u64::MAX,
        max_transfer_bytes: u64::MAX,
        ..limits.clone()
    };
    assert_eq!(
        check_offer(&[entry("a", u64::MAX), entry("b", 1)], &huge),
        Err(FileRefusal::TooLarge),
        "the total overflows"
    );
    assert_eq!(check_offer(&[], &limits), Err(FileRefusal::BadNames));
    assert_eq!(
        check_offer(&[entry("ok", 1), entry("..", 1)], &limits),
        Err(FileRefusal::BadNames)
    );
    // Names that only need cleaning are fine: the receiver cleans them.
    assert_eq!(check_offer(&[entry("../CON.txt", 1)], &limits), Ok(()));
}

/// Input keeps arriving promptly while a large file is sent: the file's
/// stream is paced, comes after the control stream, and has flow-control
/// room of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_keeps_flowing_during_a_large_transfer() {
    let scratch = Scratch::new("input");
    let big = content(16 * 1024 * 1024, 4);
    let source = scratch.folder("source", &[("big.bin", &big)]);
    let (mut a, mut b, received) = file_pair(&scratch, |_| {}).await;
    let id = a
        .handle
        .send_files(&b.id(), vec![source.join("big.bin")])
        .unwrap();
    wait_until("the data to flow", || {
        !temporary_files(&received).is_empty()
    })
    .await;

    let typed: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let typing = tokio::spawn({
        let (sender, to, typed) = (a.handle.clone(), b.id(), typed.clone());
        async move {
            for seq in 0..60 {
                typed.lock().unwrap().push(Instant::now());
                sender.send(&to, key_input(seq)).unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });
    let mut arrived = Vec::new();
    let mut keys_before_the_file = None;
    while arrived.len() < 60 || keys_before_the_file.is_none() {
        match b.next(Duration::from_secs(10)).await {
            Some(NetEvent::Message { msg, .. }) => {
                arrived.push((msg.seq().unwrap(), Instant::now()))
            }
            Some(NetEvent::Transfer(TransferEvent::Received { files, .. })) => {
                assert_eq!(files, [received.join("big.bin")]);
                keys_before_the_file = Some(arrived.len());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    typing.await.unwrap();
    let seqs: Vec<u64> = arrived.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, (0..60).collect::<Vec<u64>>());
    assert!(
        keys_before_the_file.unwrap() > 0,
        "the keys were typed while the file was arriving"
    );
    let typed = typed.lock().unwrap().clone();
    let slowest = arrived
        .iter()
        .map(|(seq, at)| at.duration_since(typed[*seq as usize]))
        .max()
        .unwrap();
    assert!(
        slowest < Duration::from_millis(500),
        "a key took {slowest:?}"
    );
    assert_eq!(expect_transfer(&mut a).await, sent(&b, id, 1, 0));
    assert_eq!(fs::read(received.join("big.bin")).unwrap(), big);
    // Nobody was overloaded.
    a.expect_quiet(Duration::from_millis(100)).await;
    b.expect_quiet(Duration::ZERO).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_mid_transfer_is_reported_on_both_sides_and_cleaned_up() {
    let scratch = Scratch::new("disconnect");
    let source = scratch.folder("source", &[("big.bin", &content(4 * 1024 * 1024, 5))]);
    let received = scratch.join("received");
    let slow = |config: &mut NetConfig| {
        receiving(&scratch.join("a-received"))(config);
        config.limits.file_rate = 256 * 1024;
    };
    let (mut a, mut b) = pair_with(slow, receiving(&received)).await;
    a.expect_up().await;
    b.expect_up().await;
    let id = a
        .handle
        .send_files(&b.id(), vec![source.join("big.bin")])
        .unwrap();
    // Queued behind the first: it never starts, and ends all the same.
    let queued = a
        .handle
        .send_files(&b.id(), vec![source.join("big.bin")])
        .unwrap();
    wait_until("the data to flow", || {
        !temporary_files(&received).is_empty()
    })
    .await;
    assert!(b.handle.disconnect(&a.id()));
    let (a_id, b_id) = (a.id(), b.id());
    for id in [id, queued] {
        assert_eq!(
            expect_transfer(&mut a).await,
            failed(b_id.clone(), id, true, TransferFailure::Disconnected)
        );
    }
    a.expect_down().await;
    assert_eq!(
        expect_transfer(&mut b).await,
        failed(a_id, id, false, TransferFailure::Disconnected)
    );
    b.expect_down().await;
    // The reports came last, right before each side's PeerDown.
    for node in [&a, &b] {
        let kinds: Vec<&str> = node
            .log
            .iter()
            .map(|event| match event {
                NetEvent::PeerUp(_) => "up",
                NetEvent::PeerDown { .. } => "down",
                NetEvent::Message { .. } => "message",
                NetEvent::Transfer(_) => "transfer",
            })
            .collect();
        assert!(kinds.ends_with(&["transfer", "down"]), "{kinds:?}");
    }
    wait_until("the temporary file to go", || {
        names_in(&received).is_empty()
    })
    .await;
}

/// The sender reads each file as it goes and checks its length: one cut
/// short while it is sent fails the transfer on both sides, cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_changes_while_it_is_sent_fails_cleanly() {
    let scratch = Scratch::new("changed");
    let source = scratch.folder("source", &[("log.txt", &content(4 * 1024 * 1024, 6))]);
    let received = scratch.join("received");
    let slow = |config: &mut NetConfig| {
        receiving(&scratch.join("a-received"))(config);
        config.limits.file_rate = 256 * 1024;
    };
    let (mut a, mut b) = pair_with(slow, receiving(&received)).await;
    a.expect_up().await;
    b.expect_up().await;
    let id = a
        .handle
        .send_files(&b.id(), vec![source.join("log.txt")])
        .unwrap();
    wait_until("the data to flow", || {
        !temporary_files(&received).is_empty()
    })
    .await;
    fs::OpenOptions::new()
        .write(true)
        .open(source.join("log.txt"))
        .unwrap()
        .set_len(1000)
        .unwrap();
    // The sender's own failure; the receiver hears that the sender stopped.
    assert_eq!(
        expect_transfer(&mut a).await,
        failed(b.id(), id, true, TransferFailure::Io)
    );
    assert_eq!(
        expect_transfer(&mut b).await,
        failed(a.id(), id, false, TransferFailure::PeerFailed)
    );
    wait_until("the temporary file to go", || {
        names_in(&received).is_empty()
    })
    .await;
    // The connection is fine.
    a.handle.send(&b.id(), heartbeat(1)).unwrap();
    b.expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
}

// ---- With a hand-made peer ----------------------------------------------

/// A peer that speaks the protocol by hand, to send what Hop itself never
/// would.
struct RawPeer {
    _endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    decoder: FrameDecoder,
}

impl RawPeer {
    /// Connects to `node`, which pins `identity`, and finishes the `Hello`
    /// exchange (advertising every capability).
    async fn connect(identity: &Identity, node: &mut Node) -> RawPeer {
        let (endpoint, conn, send, recv) =
            raw_session(identity, &node.identity, node.handle.local_addr(), None).await;
        assert!(node.expect_up().await.supports_files());
        RawPeer {
            _endpoint: endpoint,
            conn,
            send,
            recv,
            decoder: FrameDecoder::new(),
        }
    }

    async fn write(&mut self, message: Message) {
        self.send
            .write_all(&encode_frame(&message).unwrap())
            .await
            .unwrap();
    }

    async fn offer(&mut self, id: u64, files: &[(&str, &[u8])]) {
        let files = files
            .iter()
            .map(|(name, content)| FileEntry {
                name: (*name).to_owned(),
                size: content.len() as u64,
            })
            .collect();
        self.write(Message::File(FileMessage::Offer { id, files }))
            .await;
    }

    /// The node's next file message, or `None` if none comes within
    /// `wait`.
    async fn reply_within(&mut self, wait: Duration) -> Option<FileMessage> {
        let deadline = Instant::now() + wait;
        loop {
            while let Some(frame) = self.decoder.pop() {
                if let Ok(Decoded::Message(Message::File(message))) = frame {
                    return Some(message);
                }
            }
            let chunk = tokio::time::timeout_at(deadline, self.recv.read_chunk(64 * 1024, true))
                .await
                .ok()?
                .unwrap()
                .expect("the node's control stream is open");
            self.decoder.push(&chunk.bytes).unwrap();
        }
    }

    async fn reply(&mut self) -> FileMessage {
        self.reply_within(Duration::from_secs(10))
            .await
            .expect("a reply from the node")
    }

    /// Opens the data stream of transfer `id` and writes the id.
    async fn data(&self, id: u64) -> quinn::SendStream {
        let mut stream = tokio::time::timeout(Duration::from_secs(5), self.conn.open_uni())
            .await
            .expect("stream credit")
            .unwrap();
        stream.write_all(&id.to_be_bytes()).await.unwrap();
        stream
    }

    /// The node's next offer: its id and files.
    async fn offered(&mut self) -> (u64, Vec<FileEntry>) {
        match self.reply().await {
            FileMessage::Offer { id, files } => (id, files),
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    /// Accepts the node's offer `id` and takes its data stream: the bytes
    /// of the `files` it offered, each checked against its checksum. Then
    /// tells the node they were saved.
    async fn take(&mut self, id: u64, files: &[FileEntry]) -> Vec<Vec<u8>> {
        // Only a receiver allows a data stream; the transport allows none.
        self.conn
            .set_max_concurrent_uni_streams(quinn::VarInt::from_u32(1));
        self.write(Message::File(accepted(id))).await;
        let mut stream = tokio::time::timeout(Duration::from_secs(5), self.conn.accept_uni())
            .await
            .expect("the node opens its data stream")
            .unwrap();
        let mut stream_id = [0u8; 8];
        stream.read_exact(&mut stream_id).await.unwrap();
        assert_eq!(u64::from_be_bytes(stream_id), id);
        let mut contents = Vec::new();
        for file in files {
            let mut content = vec![0; usize::try_from(file.size).unwrap()];
            stream.read_exact(&mut content).await.unwrap();
            let mut digest = [0u8; 32];
            stream.read_exact(&mut digest).await.unwrap();
            assert_eq!(digest, sha256(&content));
            contents.push(content);
        }
        let saved = u32::try_from(files.len()).unwrap();
        self.write(Message::File(outcome(id, saved, None))).await;
        contents
    }

    /// Sends `files` as transfer `id` with the right checksums, after the
    /// node accepted them.
    async fn send_all(&mut self, id: u64, files: &[(&str, &[u8])]) {
        self.offer(id, files).await;
        assert_eq!(self.reply().await, accepted(id));
        let mut stream = self.data(id).await;
        for (_, content) in files {
            stream.write_all(content).await.unwrap();
            stream.write_all(&sha256(content)).await.unwrap();
        }
        stream.finish().unwrap();
    }
}

fn accepted(id: u64) -> FileMessage {
    FileMessage::Answer {
        id,
        accepted: true,
        refusal: None,
    }
}

fn refused(id: u64, refusal: FileRefusal) -> FileMessage {
    FileMessage::Answer {
        id,
        accepted: false,
        refusal: Some(refusal),
    }
}

fn outcome(id: u64, saved: u32, failure: Option<FileFailure>) -> FileMessage {
    FileMessage::Outcome { id, saved, failure }
}

/// A node receiving into `received` (with `limits` changed), and a raw
/// peer connected to it.
async fn raw_pair(received: &Path, limits: impl FnOnce(&mut Limits)) -> (Node, RawPeer, DeviceId) {
    let raw_id = Arc::new(Identity::generate().unwrap());
    let node_id = Arc::new(Identity::generate().unwrap());
    let mut node = node_with(
        node_id,
        vec![pin(&raw_id, "raw")],
        receiving_with(received, limits),
    )
    .await;
    let raw = RawPeer::connect(&raw_id, &mut node).await;
    (node, raw, raw_id.device_id())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_names_are_cleaned_and_stay_in_the_folder() {
    let scratch = Scratch::new("hostile");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) = raw_pair(&received, |_| {}).await;
    let files: [(&str, &[u8]); 5] = [
        ("../evil.txt", b"one"),
        ("a/b\\c.txt", b"two"),
        ("CON.txt", b"three"),
        ("x\u{202E}y.txt", b"four"),
        ("dots... ", b"five"),
    ];
    raw.send_all(1, &files).await;
    assert_eq!(raw.reply().await, outcome(1, 5, None));
    let saved: Vec<PathBuf> = ["evil.txt", "c.txt", "_CON.txt", "xy.txt", "dots"]
        .iter()
        .map(|name| received.join(name))
        .collect();
    assert_eq!(
        expect_transfer(&mut node).await,
        TransferEvent::Received {
            from: raw_id,
            id: 1,
            files: saved.clone()
        }
    );
    for (path, (_, content)) in saved.iter().zip(files) {
        assert_eq!(fs::read(path).unwrap(), content);
    }
    assert_eq!(
        names_in(&scratch.0),
        ["received"],
        "nothing outside the folder"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_names_and_empty_offers_are_refused() {
    let scratch = Scratch::new("bad-names");
    let received = scratch.join("received");
    let (mut node, mut raw, _) = raw_pair(&received, |_| {}).await;
    raw.offer(1, &[("fine.txt", b"x"), ("..", b"y")]).await;
    assert_eq!(raw.reply().await, refused(1, FileRefusal::BadNames));
    raw.offer(2, &[]).await;
    assert_eq!(raw.reply().await, refused(2, FileRefusal::BadNames));
    node.expect_quiet(Duration::from_millis(100)).await;
    assert!(!received.exists(), "refused before the folder was needed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_waiting_room_is_busy() {
    let scratch = Scratch::new("busy");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) =
        raw_pair(&received, |limits| limits.max_waiting_transfers = 0).await;
    raw.offer(1, &[("first.txt", b"1")]).await;
    assert_eq!(raw.reply().await, accepted(1));
    raw.offer(2, &[("second.txt", b"2")]).await;
    assert_eq!(raw.reply().await, refused(2, FileRefusal::Busy));
    // Once the first is done, there is room again.
    let mut stream = raw.data(1).await;
    stream.write_all(b"1").await.unwrap();
    stream.write_all(&sha256(b"1")).await.unwrap();
    stream.finish().unwrap();
    assert_eq!(raw.reply().await, outcome(1, 1, None));
    assert_eq!(
        expect_transfer(&mut node).await,
        TransferEvent::Received {
            from: raw_id,
            id: 1,
            files: vec![received.join("first.txt")]
        }
    );
    raw.send_all(3, &[("third.txt", b"3")]).await;
    assert_eq!(raw.reply().await, outcome(3, 1, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_checksum_keeps_only_the_files_before_it() {
    let scratch = Scratch::new("checksum");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) = raw_pair(&received, |_| {}).await;
    raw.offer(1, &[("good.txt", b"good"), ("bad.txt", b"bad")])
        .await;
    assert_eq!(raw.reply().await, accepted(1));
    let mut stream = raw.data(1).await;
    stream.write_all(b"good").await.unwrap();
    stream.write_all(&sha256(b"good")).await.unwrap();
    stream.write_all(b"bad").await.unwrap();
    stream.write_all(&sha256(b"bad!")).await.unwrap();
    stream.finish().unwrap();
    assert_eq!(raw.reply().await, outcome(1, 1, Some(FileFailure::Corrupt)));
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, 1, false, TransferFailure::Corrupt)
    );
    assert_eq!(names_in(&received), ["good.txt"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytes_after_the_last_checksum_save_nothing_more() {
    let scratch = Scratch::new("extra");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) = raw_pair(&received, |_| {}).await;
    raw.offer(1, &[("only.txt", b"only")]).await;
    assert_eq!(raw.reply().await, accepted(1));
    let mut stream = raw.data(1).await;
    stream.write_all(b"only").await.unwrap();
    stream.write_all(&sha256(b"only")).await.unwrap();
    stream.write_all(b"and more").await.unwrap();
    stream.finish().unwrap();
    assert_eq!(raw.reply().await, outcome(1, 0, Some(FileFailure::Corrupt)));
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, 1, false, TransferFailure::Corrupt)
    );
    assert!(names_in(&received).is_empty(), "{:?}", names_in(&received));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_ends_early_or_is_reset_saves_nothing() {
    for reset in [false, true] {
        let scratch = Scratch::new("early");
        let received = scratch.join("received");
        let (mut node, mut raw, raw_id) = raw_pair(&received, |_| {}).await;
        raw.offer(7, &[("half.bin", &[1u8; 10])]).await;
        assert_eq!(raw.reply().await, accepted(7));
        let mut stream = raw.data(7).await;
        stream.write_all(&[1u8; 5]).await.unwrap();
        wait_until("the temporary file", || {
            !temporary_files(&received).is_empty()
        })
        .await;
        // Either way the sender stopped, not this side.
        let (failure, reason) = if reset {
            stream.reset(0u32.into()).unwrap();
            (FileFailure::Cancelled, TransferFailure::PeerFailed)
        } else {
            stream.finish().unwrap();
            (FileFailure::Incomplete, TransferFailure::PeerFailed)
        };
        assert_eq!(raw.reply().await, outcome(7, 0, Some(failure)));
        assert_eq!(
            expect_transfer(&mut node).await,
            failed(raw_id, 7, false, reason)
        );
        wait_until("the temporary file to go", || {
            names_in(&received).is_empty()
        })
        .await;
    }
}

/// A file this side cannot save is its own failure: it tells the sender
/// the disk failed, and its own agent that a file could not be saved here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_cannot_be_saved_here_is_this_sides_failure() {
    let scratch = Scratch::new("disk");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) = raw_pair(&received, |_| {}).await;
    raw.offer(2, &[("big.bin", &[7u8; 100])]).await;
    assert_eq!(raw.reply().await, accepted(2));
    // The folder goes away before the data comes: nothing can be saved.
    fs::remove_dir_all(&received).unwrap();
    let mut stream = raw.data(2).await;
    let _ = stream.write_all(&[7u8; 100]).await;
    let _ = stream.write_all(&sha256(&[7u8; 100])).await;
    let _ = stream.finish();
    assert_eq!(raw.reply().await, outcome(2, 0, Some(FileFailure::Disk)));
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, 2, false, TransferFailure::Io)
    );
}

/// The receiver could not save a file: the sender says that the other
/// computer failed, not that a file here could not be read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_receiver_that_cannot_save_is_the_other_computers_failure() {
    let scratch = Scratch::new("their-disk");
    let source = scratch.folder("source", &[("video.mov", &content(300_000, 7))]);
    let (mut node, mut raw, raw_id) = raw_pair(&scratch.join("received"), |_| {}).await;
    let id = node
        .handle
        .send_files(&raw_id, vec![source.join("video.mov")])
        .unwrap();
    let (offered, _) = raw.offered().await;
    assert_eq!(offered, id);
    raw.conn
        .set_max_concurrent_uni_streams(quinn::VarInt::from_u32(1));
    raw.write(Message::File(accepted(id))).await;
    let mut stream = tokio::time::timeout(Duration::from_secs(5), raw.conn.accept_uni())
        .await
        .expect("the node opens its data stream")
        .unwrap();
    let mut start = [0u8; 1024];
    stream.read_exact(&mut start).await.unwrap();
    // Its disk is full, say.
    stream.stop(0u32.into()).unwrap();
    raw.write(Message::File(outcome(id, 0, Some(FileFailure::Disk))))
        .await;
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, id, true, TransferFailure::PeerFailed)
    );
}

#[test]
fn each_side_keeps_its_own_failures_apart_from_the_other_computers() {
    use super::{outcome_failure, receiving_failure};
    // Receiving: a file this side could not save is its own; data that
    // broke off is the sender's.
    assert_eq!(receiving_failure(FileFailure::Disk), TransferFailure::Io);
    for broke_off in [FileFailure::Cancelled, FileFailure::Incomplete] {
        assert_eq!(
            receiving_failure(broke_off),
            TransferFailure::PeerFailed,
            "{broke_off:?}"
        );
    }
    // Sending: whatever the receiver's outcome blames is the receiver's.
    for theirs in [
        FileFailure::Disk,
        FileFailure::Cancelled,
        FileFailure::Incomplete,
    ] {
        assert_eq!(
            outcome_failure(theirs),
            TransferFailure::PeerFailed,
            "{theirs:?}"
        );
    }
    // Damage and silence read the same from both sides.
    for (failure, reason) in [
        (FileFailure::Corrupt, TransferFailure::Corrupt),
        (FileFailure::TimedOut, TransferFailure::TimedOut),
    ] {
        assert_eq!(receiving_failure(failure), reason);
        assert_eq!(outcome_failure(failure), reason);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_for_no_accepted_offer_is_stopped() {
    let scratch = Scratch::new("unknown-stream");
    let received = scratch.join("received");
    let (mut node, mut raw, _) = raw_pair(&received, |_| {}).await;
    let stream = raw.data(99).await;
    let stopped = tokio::time::timeout(Duration::from_secs(5), stream.stopped())
        .await
        .expect("the node stops it");
    assert_eq!(stopped.unwrap(), Some(0u32.into()));
    drop(stream);
    node.expect_quiet(Duration::from_millis(100)).await;
    // The stream's place is free again for a real transfer.
    raw.send_all(1, &[("real.txt", b"real")]).await;
    assert_eq!(raw.reply().await, outcome(1, 1, None));
}

/// The sender stopped waiting for the answer (it came too late, say): the
/// receiver tells it the transfer timed out, and tells its own agent that
/// the sender did not go on with it, rather than that data stopped coming.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_offer_whose_data_never_comes_is_reported_as_unanswered() {
    let scratch = Scratch::new("no-data");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) = raw_pair(&received, |limits| {
        limits.transfer_timeout = Duration::from_millis(300);
    })
    .await;
    let started = Instant::now();
    raw.offer(3, &[("late.txt", b"late")]).await;
    assert_eq!(raw.reply().await, accepted(3));
    assert_eq!(
        raw.reply().await,
        outcome(3, 0, Some(FileFailure::TimedOut))
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, 3, false, TransferFailure::NoAnswer)
    );
}

/// A folder that was ready only after the sender stopped waiting for the
/// answer (the user allowed the Downloads folder late, say): no answer
/// goes, since it would be dropped, and the receiver says at once that the
/// files did not come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offer_ready_only_after_the_sender_stopped_waiting_is_not_answered() {
    let scratch = Scratch::new("late");
    let received = scratch.join("received");
    // No wait at all: every offer is ready too late.
    let (mut node, mut raw, raw_id) =
        raw_pair(&received, |limits| limits.answer_timeout = Duration::ZERO).await;
    raw.offer(5, &[("late.txt", b"late")]).await;
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, 5, false, TransferFailure::NoAnswer)
    );
    assert_eq!(raw.reply_within(Duration::from_millis(300)).await, None);
    assert!(names_in(&received).is_empty());
}

/// The receiver may answer only once its folder is ready (on a Mac, once
/// the user allowed the Downloads folder): the sender waits longer for the
/// answer than it lets a transfer go without progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_may_take_longer_than_progress_may_stall() {
    let scratch = Scratch::new("slow-answer");
    let source = scratch.folder("source", &[("photo.jpg", b"a photo")]);
    let (mut node, mut raw, raw_id) = raw_pair(&scratch.join("received"), |limits| {
        limits.transfer_timeout = Duration::from_millis(500);
        limits.answer_timeout = Duration::from_secs(10);
    })
    .await;
    let id = node
        .handle
        .send_files(&raw_id, vec![source.join("photo.jpg")])
        .unwrap();
    let (offered, files) = raw.offered().await;
    assert_eq!(offered, id);
    // Well over the time a transfer may go without progress.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(raw.take(id, &files).await, [b"a photo".to_vec()]);
    assert_eq!(
        expect_transfer(&mut node).await,
        TransferEvent::Sent {
            to: raw_id,
            id,
            files: 1,
            skipped: 0
        }
    );
}

/// An offer the receiver never answers sends nothing, and says so: it is
/// not a transfer that stopped moving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offer_nobody_answers_fails_as_unanswered_and_sends_nothing() {
    let scratch = Scratch::new("no-answer");
    let source = scratch.folder("source", &[("photo.jpg", b"a photo")]);
    let (mut node, mut raw, raw_id) = raw_pair(&scratch.join("received"), |limits| {
        limits.answer_timeout = Duration::from_millis(300);
    })
    .await;
    // Allowed, but never opened: the node waits for an answer first.
    raw.conn
        .set_max_concurrent_uni_streams(quinn::VarInt::from_u32(1));
    let started = Instant::now();
    let id = node
        .handle
        .send_files(&raw_id, vec![source.join("photo.jpg")])
        .unwrap();
    let (offered, _) = raw.offered().await;
    assert_eq!(offered, id);
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, id, true, TransferFailure::NoAnswer)
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), raw.conn.accept_uni())
            .await
            .is_err(),
        "no data stream"
    );
    // A late answer changes nothing.
    raw.write(Message::File(accepted(id))).await;
    node.expect_quiet(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_leaves_mid_transfer_leaves_nothing_behind() {
    let scratch = Scratch::new("raw-leaves");
    let received = scratch.join("received");
    let (mut node, mut raw, raw_id) = raw_pair(&received, |_| {}).await;
    raw.offer(1, &[("big.bin", &[0u8; 1024 * 1024])]).await;
    assert_eq!(raw.reply().await, accepted(1));
    let mut stream = raw.data(1).await;
    stream.write_all(&[0u8; 100 * 1024]).await.unwrap();
    wait_until("the temporary file", || {
        !temporary_files(&received).is_empty()
    })
    .await;
    raw.conn.close(0u32.into(), b"bye");
    assert_eq!(
        expect_transfer(&mut node).await,
        failed(raw_id, 1, false, TransferFailure::Disconnected)
    );
    // The peer's close, or, on a loaded machine where the node misses it,
    // the reset the peer's endpoint answers its next packets with.
    let down = node.expect_down().await;
    assert!(
        matches!(
            down,
            DownReason::Remote { code: 0, .. } | DownReason::Transport(_)
        ),
        "{down:?}"
    );
    wait_until("the temporary file to go", || {
        names_in(&received).is_empty()
    })
    .await;
}

/// A connection replaced by a newer one (here, the same peer dialling
/// again) takes its transfers with it, and the agent hears about them
/// before the new connection's `PeerUp`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_connection_reports_its_transfers_before_the_new_peer_up() {
    let scratch = Scratch::new("replaced");
    let received = scratch.join("received");
    let raw_id = Arc::new(Identity::generate().unwrap());
    let node_id = Arc::new(Identity::generate().unwrap());
    let mut node = node_with(
        node_id.clone(),
        vec![pin(&raw_id, "raw")],
        receiving(&received),
    )
    .await;
    let mut raw = RawPeer::connect(&raw_id, &mut node).await;
    raw.offer(4, &[("big.bin", &[0u8; 1024 * 1024])]).await;
    assert_eq!(raw.reply().await, accepted(4));
    let mut stream = raw.data(4).await;
    stream.write_all(&[0u8; 100 * 1024]).await.unwrap();
    wait_until("the temporary file", || {
        !temporary_files(&received).is_empty()
    })
    .await;
    node.log.clear();
    let (_endpoint, _conn, _send, _recv) =
        raw_session(&raw_id, &node_id, node.handle.local_addr(), None).await;
    let up = node.expect_up().await;
    assert_eq!(up.peer, raw_id.device_id());
    assert_eq!(
        node.log[0],
        NetEvent::Transfer(failed(
            raw_id.device_id(),
            4,
            false,
            TransferFailure::Disconnected
        )),
        "{:?}",
        node.log
    );
    assert_eq!(node.log.len(), 2, "{:?}", node.log);
    wait_until("the temporary file to go", || {
        names_in(&received).is_empty()
    })
    .await;
    // The old connection's close is not reported: the new one is current.
    node.expect_quiet(Duration::from_millis(300)).await;
    drop(raw);
}

/// A device that receives files, and a raw peer whose `Hello` lists only
/// `input`: `files` is not negotiated.
async fn peer_without_files(
    received: &Path,
) -> (Node, quinn::Connection, quinn::Endpoint, quinn::SendStream) {
    let raw_id = Arc::new(Identity::generate().unwrap());
    let node_id = Arc::new(Identity::generate().unwrap());
    let mut node = node_with(
        node_id.clone(),
        vec![pin(&raw_id, "raw")],
        receiving(received),
    )
    .await;
    let (endpoint, conn) = raw_connect(&raw_id, &node_id, node.handle.local_addr(), None).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut hello = Hello::new("0.1.0", raw_id.device_id(), "old peer", Vec::new());
    hello.capabilities = vec![Capability::new(caps::INPUT, 1)];
    send.write_all(&encode_frame(&Message::Hello(hello)).unwrap())
        .await
        .unwrap();
    let mut header = [0u8; 6];
    recv.read_exact(&mut header).await.unwrap();
    assert!(!node.expect_up().await.supports_files());
    (node, conn, endpoint, send)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_did_not_negotiate_files_gets_no_stream_or_answer() {
    let scratch = Scratch::new("old-peer");
    let received = scratch.join("received");
    let (mut node, conn, _endpoint, mut send) = peer_without_files(&received).await;
    // No unidirectional stream credit, ever.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), conn.open_uni())
            .await
            .is_err(),
        "the node allowed a data stream"
    );
    // An offer anyway is dropped, and what follows still arrives.
    let offer = FileMessage::Offer {
        id: 1,
        files: vec![FileEntry {
            name: "x".to_owned(),
            size: 1,
        }],
    };
    send.write_all(&encode_frame(&Message::File(offer)).unwrap())
        .await
        .unwrap();
    send.write_all(&encode_frame(&Message::Core(heartbeat(2))).unwrap())
        .await
        .unwrap();
    let event = node
        .expect("Heartbeat", |e| matches!(e, NetEvent::Message { .. }))
        .await;
    assert!(matches!(event, NetEvent::Message { .. }));
    node.expect_quiet(Duration::from_millis(200)).await;
    assert!(!received.exists());
}
