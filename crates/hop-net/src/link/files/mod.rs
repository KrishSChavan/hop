//! File transfers on a session connection (milestone M5;
//! docs/M4-M5-DESIGN.md, "The transfer protocol"), between two devices
//! that both negotiated `files`.
//!
//! One transfer, with the [`FileMessage`]s on the control stream:
//!
//! 1. The sender plans the dropped items on a blocking thread (regular
//!    files only) and sends an `Offer` with each file's name and size.
//! 2. The receiver checks the offer and answers: the folder exists or can
//!    be created, at most `max_transfer_files` files of at most
//!    `max_file_bytes` each and `max_transfer_bytes` in all, every name
//!    survives the sanitizer, and it is not busy (one transfer running and
//!    at most `max_waiting_transfers` waiting). The first offer of a run is
//!    the first time the folder is touched: it is created then and swept of
//!    leftover temporary files. An offer the folder was ready for only after
//!    `answer_timeout` (on a Mac, the user allowed the Downloads folder
//!    late) gets no answer: the sender has stopped waiting.
//! 3. After an accepted `Answer`, the sender opens one unidirectional
//!    stream at priority −1 and writes the transfer id (8 bytes,
//!    big-endian), then for each file exactly `size` bytes and their
//!    SHA-256 (32 bytes), paced to `file_rate`, then finishes the stream.
//!    Each side sends its transfers one at a time, in the order they were
//!    started.
//! 4. The receiver saves each file through `hop-transfer` on a blocking
//!    thread (a temporary name, checked, then renamed) and sends an
//!    `Outcome`.
//!
//! The receiver allows the peer one unidirectional stream at a time, and
//! only once `files` is negotiated; the transport settings allow none. The
//! answer must come within `answer_timeout` (the receiver may be waiting
//! for its folder), and every stretch of a transfer after it within
//! `transfer_timeout`. An offer that goes unanswered, on either side, is
//! [`TransferFailure::NoAnswer`], as is an accepted one whose data does not
//! start in time. Any failure deletes the temporary files of that
//! transfer, stops its stream and, while the connection is up, sends an
//! `Outcome` with the reason; a file never closes the connection. A
//! connection that ends takes its transfers with it, and those that were
//! running are reported as [`TransferFailure::Disconnected`] before
//! `PeerDown`.
//!
//! All of this runs in the connection's own task, beside its control-stream
//! reader and writer, so it ends with the connection. Disk I/O runs on
//! tokio's blocking threads, fed through bounded channels, never on the
//! two network workers. Logs hold counts and reasons only: never a file
//! name, a byte of a file, or a checksum.
//!
//! Data-stream error codes, for `STOP_SENDING` and `RESET_STREAM`: 0 means
//! the transfer was called off.

mod pace;
mod receive;
mod send;
mod split;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use hop_proto::{FileEntry, FileFailure, FileMessage, FileRefusal, Message, encode_frame};
use hop_types::DeviceId;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use super::{Limits, Link, SendError};
use crate::events::EventQueue;
use crate::{CloseCode, Refusal, TransferEvent, TransferFailure, ratelimit};

/// The error code of a data stream this side calls off.
const CALLED_OFF: quinn::VarInt = quinn::VarInt::from_u32(0);

/// Offers that may wait for an answer on one connection; more are refused
/// as busy at once.
const MAX_PENDING_OFFERS: usize = 4;

/// Receiving files, shared by every connection: the folder, and how many
/// transfers are accepted.
pub(crate) struct Inbox {
    dir: PathBuf,
    /// Held while the folder is checked; true once this run swept it.
    swept: tokio::sync::Mutex<bool>,
    /// Transfers accepted and not finished, on every connection.
    accepted: Mutex<usize>,
    /// The most transfers accepted at once: one running, the rest waiting.
    max_accepted: usize,
}

impl Inbox {
    pub(crate) fn new(dir: PathBuf, limits: &Limits) -> Inbox {
        Inbox {
            dir,
            swept: tokio::sync::Mutex::new(false),
            accepted: Mutex::new(0),
            max_accepted: limits.max_waiting_transfers.saturating_add(1),
        }
    }

    /// Makes sure the folder exists, creating it if needed, and the first
    /// time also deletes the temporary files an earlier run left. Runs on a
    /// blocking thread; on macOS the first access inside Downloads may wait
    /// for the user to allow it. Returns whether the folder can be used.
    async fn prepare(&self) -> bool {
        let mut swept = self.swept.lock().await;
        let dir = self.dir.clone();
        let sweep = !*swept;
        let prepared = tokio::task::spawn_blocking(move || -> io::Result<usize> {
            std::fs::create_dir_all(&dir)?;
            if sweep {
                hop_transfer::sweep(&dir)
            } else {
                Ok(0)
            }
        })
        .await;
        match prepared {
            Ok(Ok(removed)) => {
                if sweep {
                    *swept = true;
                    if removed > 0 {
                        tracing::info!(
                            removed,
                            "deleted unfinished files an earlier run left in the Received folder"
                        );
                    }
                }
                true
            }
            Ok(Err(error)) => {
                if let Some(suppressed) = ratelimit::allow("received folder unusable") {
                    tracing::warn!(
                        error = %error.kind(),
                        suppressed,
                        "cannot use the Received folder, so files are refused"
                    );
                }
                false
            }
            Err(_) => false,
        }
    }

    /// Takes a place for one more accepted transfer, unless all are taken.
    fn reserve(self: &Arc<Self>) -> Option<Reservation> {
        let mut accepted = lock(&self.accepted);
        if *accepted >= self.max_accepted {
            return None;
        }
        *accepted += 1;
        Some(Reservation(self.clone()))
    }
}

/// One accepted transfer's place in the [`Inbox`], given back when dropped.
struct Reservation(Arc<Inbox>);

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut accepted = lock(&self.0.accepted);
        *accepted = accepted.saturating_sub(1);
    }
}

/// An offer waiting for its answer.
struct Offered {
    id: u64,
    files: Vec<FileEntry>,
    /// When it arrived: the sender waits `answer_timeout` from about then.
    arrived: Instant,
}

/// A transfer waiting to be sent.
struct Outgoing {
    id: u64,
    paths: Vec<PathBuf>,
    /// The receiver's answer and outcome.
    replies: mpsc::Receiver<FileMessage>,
}

/// The work [`ConnFiles::serve`] takes in: offers to answer, and transfers
/// to send in the order they were started.
pub(crate) struct Queues {
    offers: mpsc::Receiver<Offered>,
    outbox: mpsc::UnboundedReceiver<Outgoing>,
}

/// One connection's file transfers, in both directions.
pub(crate) struct ConnFiles {
    peer: DeviceId,
    conn_id: u64,
    link: Arc<Link>,
    queue: Arc<EventQueue>,
    limits: Limits,
    inbox: Arc<Inbox>,
    /// The control stream's send queue. Weak, so that dropping the
    /// connection's `Current` still lets the writer flush and finish.
    control: mpsc::WeakSender<Bytes>,
    /// Offers for [`ConnFiles::serve`] to answer.
    offers: mpsc::Sender<Offered>,
    /// Transfers for [`ConnFiles::serve`] to send. Unbounded: only the
    /// agent adds to it, once per drop of files.
    outbox: mpsc::UnboundedSender<Outgoing>,
    /// Rung when an offer is accepted, so the stream loop re-arms its
    /// timer.
    accepted: Notify,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Set once the connection ended: nothing new starts, and nothing more
    /// is reported.
    ended: bool,
    /// Transfers this side is sending or will send: where the receiver's
    /// answer and outcome for each go.
    sending: HashMap<u64, mpsc::Sender<FileMessage>>,
    /// Transfers this side accepted and has not finished receiving.
    receiving: HashMap<u64, Receiving>,
}

struct Receiving {
    /// The files, until the data stream starts.
    files: Vec<FileEntry>,
    /// When to give up waiting for the data stream; `None` once it started.
    deadline: Option<Instant>,
    _place: Reservation,
}

/// Why a file message could not be queued on the control stream.
#[derive(Debug, PartialEq, Eq)]
enum ControlError {
    /// The message does not encode (a name over the protocol's limit).
    Encode,
    /// The connection is closing or closed.
    Gone,
}

impl ConnFiles {
    /// The transfers of a connection that just became current, and the
    /// work [`ConnFiles::serve`] takes in.
    pub(crate) fn new(
        peer: DeviceId,
        conn_id: u64,
        link: Arc<Link>,
        queue: Arc<EventQueue>,
        limits: Limits,
        inbox: Arc<Inbox>,
        control: &mpsc::Sender<Bytes>,
    ) -> (Arc<ConnFiles>, Queues) {
        let (offers, offers_rx) = mpsc::channel(MAX_PENDING_OFFERS);
        let (outbox, outbox_rx) = mpsc::unbounded_channel();
        let files = Arc::new(ConnFiles {
            peer,
            conn_id,
            link,
            queue,
            limits,
            inbox,
            control: control.downgrade(),
            offers,
            outbox,
            accepted: Notify::new(),
            state: Mutex::new(State::default()),
        });
        let queues = Queues {
            offers: offers_rx,
            outbox: outbox_rx,
        };
        (files, queues)
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Handles a file message from the control stream. Never waits, so the
    /// reader is never held up.
    pub(crate) fn on_message(&self, message: FileMessage) {
        match message {
            FileMessage::Offer { id, files } => {
                let offered = Offered {
                    id,
                    files,
                    arrived: Instant::now(),
                };
                if let Err(full) = self.offers.try_send(offered) {
                    let id = match full {
                        mpsc::error::TrySendError::Full(offer)
                        | mpsc::error::TrySendError::Closed(offer) => offer.id,
                    };
                    self.refuse(id, FileRefusal::Busy);
                }
            }
            FileMessage::Answer { id, .. } | FileMessage::Outcome { id, .. } => {
                let replies = self.lock_state().sending.get(&id).cloned();
                // A reply about a transfer that already ended here is
                // dropped; so are more replies than a transfer has.
                if let Some(replies) = replies {
                    let _ = replies.try_send(message);
                }
            }
        }
    }

    /// Queues the files at `paths` to be sent as transfer `id`, after the
    /// transfers started before it. Never waits.
    pub(crate) fn start_sending(&self, id: u64, paths: Vec<PathBuf>) -> Result<(), SendError> {
        let (replies, replies_rx) = mpsc::channel(2);
        // Held until the transfer is registered, so its replies find it.
        let mut state = self.lock_state();
        if state.ended {
            return Err(SendError::NotConnected);
        }
        let job = Outgoing {
            id,
            paths,
            replies: replies_rx,
        };
        self.outbox.send(job).map_err(|_| SendError::NotConnected)?;
        state.sending.insert(id, replies);
        Ok(())
    }

    /// Answers offers, receives data streams and sends the queued
    /// transfers, until the connection ends. It never returns: the
    /// connection's other tasks decide when it ends, dropping this, and
    /// [`ConnFiles::end`] then reports what had not finished.
    pub(crate) async fn serve(self: Arc<Self>, queues: Queues) -> Infallible {
        let (never, _, _) = tokio::join!(
            self.answer_offers(queues.offers),
            self.receive_streams(),
            self.send_queued(queues.outbox),
        );
        never
    }

    /// Sends the queued transfers one after the other.
    async fn send_queued(&self, mut outbox: mpsc::UnboundedReceiver<Outgoing>) -> Infallible {
        while let Some(Outgoing { id, paths, replies }) = outbox.recv().await {
            // Ended meanwhile (the connection was replaced): reported then.
            if !self.lock_state().sending.contains_key(&id) {
                continue;
            }
            let result = send::transfer(self, id, paths, replies).await;
            self.finish_sending(id, result);
        }
        // `self` holds the sender, so the queue never closes.
        std::future::pending().await
    }

    /// Ends every transfer of this connection, which has closed, reporting
    /// each one that had not finished (or not started) as disconnected.
    /// Call it once the connection's tasks have stopped and before its
    /// `PeerDown`, so the reports come first.
    pub(crate) fn end(&self) {
        let queued = {
            let mut state = self.lock_state();
            if state.ended {
                return;
            }
            let reports = self.disconnected(&mut state);
            let mut queued = false;
            for report in reports {
                queued |= self.report(report);
            }
            queued
        };
        if queued {
            self.queue.wake();
        }
    }

    /// Ends every transfer of this connection, which a newer one replaces:
    /// `announce` gets the reports for the transfers that had not finished
    /// and queues them, then the newer connection's `PeerUp`, in one step,
    /// so they come before it. Nothing changes if `announce` returns false.
    /// Called with the link's table locked; the caller wakes the agent.
    pub(crate) fn end_replaced(&self, announce: impl FnOnce(Vec<TransferEvent>) -> bool) -> bool {
        let mut state = self.lock_state();
        if state.ended {
            return announce(Vec::new());
        }
        let reports = self.disconnected_reports(&state);
        if !announce(reports) {
            return false;
        }
        self.disconnected(&mut state);
        true
    }

    /// Marks the connection as ended and takes every unfinished transfer
    /// out of the state (giving back their places in the inbox), returning
    /// their reports.
    fn disconnected(&self, state: &mut State) -> Vec<TransferEvent> {
        let reports = self.disconnected_reports(state);
        state.ended = true;
        state.sending.clear();
        state.receiving.clear();
        reports
    }

    /// A `Disconnected` failure for each unfinished transfer: the sent ones
    /// first, each kind in id order.
    fn disconnected_reports(&self, state: &State) -> Vec<TransferEvent> {
        let mut sending: Vec<u64> = state.sending.keys().copied().collect();
        let mut receiving: Vec<u64> = state.receiving.keys().copied().collect();
        sending.sort_unstable();
        receiving.sort_unstable();
        let report = |id, outgoing| TransferEvent::Failed {
            peer: self.peer.clone(),
            id,
            outgoing,
            reason: TransferFailure::Disconnected,
        };
        sending
            .into_iter()
            .map(|id| report(id, true))
            .chain(receiving.into_iter().map(|id| report(id, false)))
            .collect()
    }

    /// Reports how a transfer this side sent ended, unless the connection
    /// ended first (and reported it).
    fn finish_sending(&self, id: u64, result: Result<(u32, u32), TransferFailure>) {
        let queued = {
            let mut state = self.lock_state();
            if state.sending.remove(&id).is_none() {
                return;
            }
            match result {
                Ok((files, skipped)) => {
                    tracing::info!(id = %self.peer, files, skipped, "sent files");
                    self.report(TransferEvent::Sent {
                        to: self.peer.clone(),
                        id,
                        files,
                        skipped,
                    })
                }
                Err(reason) => {
                    tracing::info!(
                        id = %self.peer,
                        reason = describe(&reason),
                        "sending files failed"
                    );
                    self.report_failure(id, true, reason)
                }
            }
        };
        if queued {
            self.queue.wake();
        }
    }

    fn report_failure(&self, id: u64, outgoing: bool, reason: TransferFailure) -> bool {
        self.report(TransferEvent::Failed {
            peer: self.peer.clone(),
            id,
            outgoing,
            reason,
        })
    }

    /// Queues `event` for the agent; true if it was queued. Called with the
    /// state lock held, so a transfer is reported exactly once; the caller
    /// wakes the agent after letting go of it. A report that finds the
    /// queue full is still queued, and closes the connection as for a
    /// reliable message.
    fn report(&self, event: TransferEvent) -> bool {
        match self.queue.transfer(&self.peer, self.conn_id, event) {
            Some(full) => {
                if full {
                    self.link
                        .close(CloseCode::Overloaded, "the agent is not keeping up");
                }
                true
            }
            None => false,
        }
    }

    /// Queues `message` on the control stream. A full queue closes the
    /// connection with `OVERLOADED`, as for any reliable message.
    fn send_control(&self, message: FileMessage) -> Result<(), ControlError> {
        let frame = encode_frame(&Message::File(message)).map_err(|_| ControlError::Encode)?;
        let control = self.control.upgrade().ok_or(ControlError::Gone)?;
        match control.try_send(Bytes::from(frame)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.link
                    .close(CloseCode::Overloaded, "the send queue is full");
                Err(ControlError::Gone)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(ControlError::Gone),
        }
    }

    fn refuse(&self, id: u64, refusal: FileRefusal) {
        if let Some(suppressed) = ratelimit::allow("files refused") {
            tracing::info!(
                id = %self.peer,
                reason = refusal_text(refusal),
                suppressed,
                "turned down files from the other device"
            );
        }
        let _ = self.send_control(FileMessage::Answer {
            id,
            accepted: false,
            refusal: Some(refusal),
        });
    }
}

/// Whether an offer is within this side's limits: the checks that need
/// neither the disk nor the other transfers.
pub(super) fn check_offer(files: &[FileEntry], limits: &Limits) -> Result<(), FileRefusal> {
    if files.len() > limits.max_transfer_files {
        return Err(FileRefusal::TooMany);
    }
    let mut total: u64 = 0;
    for file in files {
        total = total
            .checked_add(file.size)
            .filter(|total| {
                file.size <= limits.max_file_bytes && *total <= limits.max_transfer_bytes
            })
            .ok_or(FileRefusal::TooLarge)?;
    }
    if files.is_empty()
        || files
            .iter()
            .any(|file| hop_transfer::sanitize(&file.name).is_none())
    {
        return Err(FileRefusal::BadNames);
    }
    Ok(())
}

/// The agent's word for a refusal from the wire.
pub(super) fn refusal(refusal: FileRefusal) -> Refusal {
    match refusal {
        FileRefusal::Off => Refusal::Off,
        FileRefusal::TooMany => Refusal::TooMany,
        FileRefusal::TooLarge => Refusal::TooLarge,
        FileRefusal::Busy => Refusal::Busy,
        FileRefusal::BadNames => Refusal::BadNames,
    }
}

/// The receiving side's word for why it stopped: its own failure (a file
/// it could not save) kept apart from the sender's (the data broke off).
pub(super) fn receiving_failure(failure: FileFailure) -> TransferFailure {
    match failure {
        FileFailure::Corrupt => TransferFailure::Corrupt,
        FileFailure::TimedOut => TransferFailure::TimedOut,
        FileFailure::Disk => TransferFailure::Io,
        // The sender reset the stream, or ended it early.
        FileFailure::Incomplete | FileFailure::Cancelled => TransferFailure::PeerFailed,
    }
}

/// The sending side's word for the failure the receiver's outcome gives:
/// the receiver could not go on (a file it could not save, or data it says
/// broke off), which is not this side's own failure.
pub(super) fn outcome_failure(failure: FileFailure) -> TransferFailure {
    match failure {
        FileFailure::Corrupt => TransferFailure::Corrupt,
        FileFailure::TimedOut => TransferFailure::TimedOut,
        FileFailure::Disk | FileFailure::Incomplete | FileFailure::Cancelled => {
            TransferFailure::PeerFailed
        }
    }
}

/// A refusal, for the log.
fn refusal_text(refusal: FileRefusal) -> &'static str {
    match refusal {
        FileRefusal::Off => "the Received folder cannot be used",
        FileRefusal::TooMany => "too many files",
        FileRefusal::TooLarge => "too large",
        FileRefusal::Busy => "busy",
        FileRefusal::BadNames => "a file name cannot be used",
    }
}

/// A failure, for the log.
fn describe(reason: &TransferFailure) -> &'static str {
    match reason {
        TransferFailure::Refused(refused) => match refused {
            Refusal::Off => "refused: the Received folder there cannot be used",
            Refusal::TooMany => "refused: too many files",
            Refusal::TooLarge => "refused: too large",
            Refusal::Busy => "refused: busy",
            Refusal::BadNames => "refused: a file name cannot be used there",
        },
        TransferFailure::NothingToSend => "nothing that could be sent",
        TransferFailure::NotSupported => "the other device cannot receive files",
        TransferFailure::NoAnswer => "no answer to the offer",
        TransferFailure::Disconnected => "disconnected",
        TransferFailure::TimedOut => "timed out",
        TransferFailure::Corrupt => "a file arrived damaged",
        TransferFailure::Io => "a file could not be read here",
        TransferFailure::PeerFailed => "the other device could not save a file",
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic is fatal for the agent; a poisoned lock only happens on the
    // way down, and the data is still consistent enough to finish.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
