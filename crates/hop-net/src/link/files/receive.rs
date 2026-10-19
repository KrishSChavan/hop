//! The receiving side of a transfer: answering offers, and saving the
//! files of each data stream through `hop-transfer` on a blocking thread.

use std::convert::Infallible;
use std::path::{Path, PathBuf};

use hop_proto::{FileEntry, FileFailure, FileMessage, FileRefusal};
use hop_transfer::{IncomingFile, ReceiveError};
use quinn::RecvStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::split::{Piece, Splitter};
use super::{CALLED_OFF, ConnFiles, Offered, Receiving, check_offer, failure};
use crate::{TransferEvent, ratelimit};

/// How much the stream reader takes from quinn at a time.
const READ_CHUNK: usize = 64 * 1024;

/// Pieces waiting for the disk thread, at most `READ_CHUNK` bytes each.
const WRITE_AHEAD: usize = 8;

/// How a data stream ended.
enum Received {
    /// Every file was saved, in offer order.
    Saved(Vec<PathBuf>),
    /// It stopped early. The first `saved` files were kept.
    Failed { saved: u32, failure: FileFailure },
    /// The connection went away; [`ConnFiles::end`] reports it.
    Lost,
}

impl ConnFiles {
    /// Answers each offer in turn.
    pub(super) async fn answer_offers(&self, mut offers: mpsc::Receiver<Offered>) -> Infallible {
        while let Some(Offered { id, files }) = offers.recv().await {
            let count = files.len();
            match self.accept(id, files).await {
                Ok(()) => {
                    tracing::debug!(id = %self.peer, files = count, "accepted files");
                    let _ = self.send_control(FileMessage::Answer {
                        id,
                        accepted: true,
                        refusal: None,
                    });
                }
                Err(refused) => self.refuse(id, refused),
            }
        }
        // `self` holds the sender, so the queue never closes.
        std::future::pending().await
    }

    /// Checks an offer and, if it passes, records it as accepted.
    async fn accept(&self, id: u64, files: Vec<FileEntry>) -> Result<(), FileRefusal> {
        check_offer(&files, &self.limits)?;
        let place = self.inbox.reserve().ok_or(FileRefusal::Busy)?;
        if !self.inbox.prepare().await {
            return Err(FileRefusal::Off);
        }
        {
            let mut state = self.lock_state();
            // A reused id is the peer's mistake; the first transfer keeps it.
            if state.ended || state.receiving.contains_key(&id) {
                return Err(FileRefusal::Busy);
            }
            state.receiving.insert(
                id,
                Receiving {
                    files,
                    deadline: Some(Instant::now() + self.limits.transfer_timeout),
                    _place: place,
                },
            );
        }
        self.accepted.notify_one();
        Ok(())
    }

    /// Takes each data stream the peer opens, one at a time (it may only
    /// have one open), and gives up on accepted transfers whose stream does
    /// not start in time.
    pub(super) async fn receive_streams(&self) -> Infallible {
        loop {
            let deadline = self.next_deadline();
            let accepted = self.accepted.notified();
            tokio::select! {
                stream = self.link.conn.accept_uni() => match stream {
                    Ok(stream) => {
                        self.receive_stream(stream).await;
                        // Transfers waiting behind that one get a full
                        // timeout again.
                        self.restart_waiting();
                    }
                    // The connection is gone: the control stream's reader
                    // reports it, after delivering what already arrived.
                    Err(_) => return std::future::pending().await,
                },
                () = sleep_until(deadline) => self.expire_waiting(),
                () = accepted => {}
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.lock_state()
            .receiving
            .values()
            .filter_map(|transfer| transfer.deadline)
            .min()
    }

    fn restart_waiting(&self) {
        let deadline = Instant::now() + self.limits.transfer_timeout;
        for transfer in self.lock_state().receiving.values_mut() {
            if transfer.deadline.is_some() {
                transfer.deadline = Some(deadline);
            }
        }
    }

    /// Fails the accepted transfers whose data stream did not start in time.
    fn expire_waiting(&self) {
        let now = Instant::now();
        let queued = {
            let mut state = self.lock_state();
            let mut expired: Vec<u64> = state
                .receiving
                .iter()
                .filter(|(_, transfer)| transfer.deadline.is_some_and(|deadline| deadline <= now))
                .map(|(id, _)| *id)
                .collect();
            expired.sort_unstable();
            let mut queued = false;
            for id in expired {
                state.receiving.remove(&id);
                queued |= self.settle(id, Err((0, FileFailure::TimedOut)));
            }
            queued
        };
        if queued {
            self.queue.wake();
        }
    }

    /// Receives one data stream: its transfer id, then the files.
    async fn receive_stream(&self, mut stream: RecvStream) {
        let mut id = [0u8; 8];
        let read = tokio::time::timeout(self.limits.transfer_timeout, stream.read_exact(&mut id));
        if !matches!(read.await, Ok(Ok(()))) {
            let _ = stream.stop(CALLED_OFF);
            return;
        }
        let id = u64::from_be_bytes(id);
        let Some(files) = self.start_receiving(id) else {
            if let Some(suppressed) = ratelimit::allow("unexpected file stream") {
                tracing::warn!(
                    id = %self.peer,
                    suppressed,
                    "refused a file stream that matches no accepted offer"
                );
            }
            let _ = stream.stop(CALLED_OFF);
            return;
        };
        let result = match receive_files(&self.inbox.dir, files, &mut stream, &self.limits).await {
            Received::Saved(paths) => Ok(paths),
            Received::Failed { saved, failure } => {
                let _ = stream.stop(CALLED_OFF);
                Err((saved, failure))
            }
            // The connection is gone: `end` reports the transfer.
            Received::Lost => return,
        };
        let queued = {
            let mut state = self.lock_state();
            // Gone if the connection ended meanwhile; it was reported then.
            if state.receiving.remove(&id).is_none() {
                return;
            }
            self.settle(id, result)
        };
        if queued {
            self.queue.wake();
        }
    }

    /// Marks accepted transfer `id` as started and returns its files, if
    /// it is waiting for its stream.
    fn start_receiving(&self, id: u64) -> Option<Vec<FileEntry>> {
        let mut state = self.lock_state();
        let transfer = state.receiving.get_mut(&id)?;
        transfer.deadline.take()?;
        Some(std::mem::take(&mut transfer.files))
    }

    /// Sends the outcome of received transfer `id`, already taken out of
    /// the state, and reports it to the agent: the saved files, or how many
    /// were saved before it failed and why. Called with the state lock held;
    /// true if the agent has an event to read.
    fn settle(&self, id: u64, result: Result<Vec<PathBuf>, (u32, FileFailure)>) -> bool {
        match result {
            Ok(files) => {
                let saved = u32::try_from(files.len()).unwrap_or(u32::MAX);
                tracing::info!(id = %self.peer, files = saved, "received files");
                let _ = self.send_control(FileMessage::Outcome {
                    id,
                    saved,
                    failure: None,
                });
                self.report(TransferEvent::Received {
                    from: self.peer.clone(),
                    id,
                    files,
                })
            }
            Err((saved, failed)) => {
                tracing::info!(
                    id = %self.peer,
                    saved,
                    reason = failure_text(failed),
                    "receiving files failed"
                );
                let _ = self.send_control(FileMessage::Outcome {
                    id,
                    saved,
                    failure: Some(failed),
                });
                self.report_failure(id, false, failure(failed))
            }
        }
    }
}

/// Sleeps until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// A failure, for the log.
fn failure_text(failure: FileFailure) -> &'static str {
    match failure {
        FileFailure::Incomplete => "the data ended early",
        FileFailure::Corrupt => "a file did not match its checksum",
        FileFailure::Disk => "a file could not be written",
        FileFailure::Cancelled => "the other device called it off",
        FileFailure::TimedOut => "no progress in time",
    }
}

/// Reads the files of one data stream (after its id) and saves them in
/// `dir`, writing on a blocking thread.
///
/// The last file is saved only once the stream has ended, so a stream that
/// goes on past it saves nothing more.
async fn receive_files(
    dir: &Path,
    files: Vec<FileEntry>,
    stream: &mut RecvStream,
    limits: &super::Limits,
) -> Received {
    let progress = limits.transfer_timeout;
    let mut splitter = Splitter::new(files.iter().map(|file| file.size).collect());
    let (pieces, pieces_rx) = mpsc::channel(WRITE_AHEAD);
    let writer = tokio::task::spawn_blocking({
        let dir = dir.to_path_buf();
        move || write_files(&dir, &files, pieces_rx)
    });
    let mut out = Vec::new();
    let mut last = None;
    let stopped = 'read: loop {
        let chunk = match tokio::time::timeout(progress, stream.read_chunk(READ_CHUNK, true)).await
        {
            Err(_) => break Some(FileFailure::TimedOut),
            Ok(Err(quinn::ReadError::ConnectionLost(_))) => return Received::Lost,
            // Reset by the sender, which gave up.
            Ok(Err(_)) => break Some(FileFailure::Cancelled),
            Ok(Ok(None)) => {
                if !splitter.is_done() {
                    break Some(FileFailure::Incomplete);
                }
                // The end, right after the last checksum: now the last file
                // may be saved. If the disk thread has stopped, it says why
                // below.
                if let Some(end) = last.take() {
                    let _ = send_piece(&pieces, end, progress).await;
                }
                break None;
            }
            Ok(Ok(Some(chunk))) => chunk.bytes,
        };
        out.clear();
        if splitter.push(chunk, &mut out).is_err() {
            break Some(FileFailure::Corrupt);
        }
        if splitter.is_done() && last.is_none() {
            last = out.pop();
        }
        for piece in out.drain(..) {
            if !send_piece(&pieces, piece, progress).await {
                // The disk thread stopped: it says why below.
                break 'read None;
            }
        }
    };
    drop(pieces);
    let written = match writer.await {
        Ok(written) => written,
        Err(_) => Written {
            saved: Vec::new(),
            failure: Some(FileFailure::Disk),
            complete: false,
        },
    };
    let saved = u32::try_from(written.saved.len()).unwrap_or(u32::MAX);
    match (stopped, written.failure) {
        (Some(failure), _) | (None, Some(failure)) => Received::Failed { saved, failure },
        (None, None) if written.complete => Received::Saved(written.saved),
        // The disk thread ended without an error before the last file: it
        // cannot, but nothing is reported as saved that was not.
        (None, None) => Received::Failed {
            saved,
            failure: FileFailure::Disk,
        },
    }
}

/// Hands `piece` to the disk thread, waiting at most `progress`. False if
/// the thread has stopped or does not keep up.
async fn send_piece(
    pieces: &mpsc::Sender<Piece>,
    piece: Piece,
    progress: std::time::Duration,
) -> bool {
    matches!(
        tokio::time::timeout(progress, pieces.send(piece)).await,
        Ok(Ok(()))
    )
}

/// What the disk thread saved.
struct Written {
    /// The files saved, in order.
    saved: Vec<PathBuf>,
    /// Why it stopped early, if it did.
    failure: Option<FileFailure>,
    /// Whether every file was saved.
    complete: bool,
}

/// The disk thread: saves the pieces of `files`, in order, in `dir`. Ends
/// at the first error, or when the pieces stop; a file that was not
/// complete by then is deleted when its writer is dropped.
fn write_files(dir: &Path, files: &[FileEntry], mut pieces: mpsc::Receiver<Piece>) -> Written {
    let mut saved = Vec::new();
    let mut current: Option<IncomingFile> = None;
    let fail = |saved: Vec<PathBuf>, error: ReceiveError| Written {
        saved,
        failure: Some(match error {
            ReceiveError::Corrupt | ReceiveError::TooLong | ReceiveError::TooShort { .. } => {
                FileFailure::Corrupt
            }
            ReceiveError::BadName | ReceiveError::Io(_) => FileFailure::Disk,
        }),
        complete: false,
    };
    while let Some(piece) = pieces.blocking_recv() {
        let Some(entry) = files.get(saved.len()) else {
            // The splitter never sends more than the offer listed.
            return fail(saved, ReceiveError::TooLong);
        };
        let file = match current.take() {
            Some(file) => file,
            None => match IncomingFile::create(dir, &entry.name, entry.size) {
                Ok(file) => file,
                Err(error) => return fail(saved, error),
            },
        };
        match piece {
            Piece::Data(bytes) => {
                let mut file = file;
                if let Err(error) = file.write(&bytes) {
                    return fail(saved, error);
                }
                current = Some(file);
            }
            Piece::End(digest) => match file.finish(&digest) {
                Ok(path) => saved.push(path),
                Err(error) => return fail(saved, error),
            },
        }
    }
    let complete = saved.len() == files.len();
    Written {
        saved,
        failure: None,
        complete,
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use sha2::{Digest as _, Sha256};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hop-net-write-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, size: u64) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            size,
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Runs the disk thread on `pieces` as a test thread would.
    fn write(dir: &Path, files: &[FileEntry], pieces: Vec<Piece>) -> Written {
        let (tx, rx) = mpsc::channel(pieces.len().max(1));
        for piece in pieces {
            tx.try_send(piece).unwrap();
        }
        drop(tx);
        write_files(dir, files, rx)
    }

    #[test]
    fn the_disk_thread_saves_files_in_order() {
        let dir = scratch("order");
        let files = [entry("a.txt", 3), entry("empty", 0), entry("a.txt", 2)];
        let written = write(
            &dir,
            &files,
            vec![
                Piece::Data(Bytes::from_static(b"ab")),
                Piece::Data(Bytes::from_static(b"c")),
                Piece::End(Sha256::digest(b"abc").into()),
                Piece::End(Sha256::digest(b"").into()),
                Piece::Data(Bytes::from_static(b"de")),
                Piece::End(Sha256::digest(b"de").into()),
            ],
        );
        assert!(written.complete && written.failure.is_none());
        assert_eq!(
            written.saved,
            [dir.join("a.txt"), dir.join("empty"), dir.join("a (2).txt")]
        );
        assert_eq!(std::fs::read(dir.join("a (2).txt")).unwrap(), b"de");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_disk_thread_keeps_what_checked_out_and_deletes_the_rest() {
        let dir = scratch("partial");
        let files = [entry("good", 1), entry("bad", 1), entry("never", 1)];
        let written = write(
            &dir,
            &files,
            vec![
                Piece::Data(Bytes::from_static(b"g")),
                Piece::End(Sha256::digest(b"g").into()),
                Piece::Data(Bytes::from_static(b"b")),
                Piece::End(Sha256::digest(b"x").into()),
            ],
        );
        assert_eq!(written.failure, Some(FileFailure::Corrupt));
        assert_eq!(written.saved, [dir.join("good")]);
        assert_eq!(names(&dir), ["good"]);

        // Pieces that stop in the middle of a file: it is deleted.
        let written = write(
            &dir,
            &[entry("cut", 4)],
            vec![Piece::Data(Bytes::from_static(b"cu"))],
        );
        assert!(!written.complete && written.failure.is_none());
        assert_eq!(names(&dir), ["good"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_written_is_a_disk_failure() {
        let dir = scratch("disk");
        let missing = dir.join("missing");
        let written = write(
            &missing,
            &[entry("x", 1)],
            vec![Piece::Data(Bytes::from_static(b"x"))],
        );
        assert_eq!(written.failure, Some(FileFailure::Disk));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
