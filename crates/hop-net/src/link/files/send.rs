//! The sending side of a transfer: plan, offer, answer, data, outcome.

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use hop_proto::{FileEntry, FileMessage, MAX_OFFER_FILES};
use hop_transfer::{Digest, FileReader, OutgoingFile, Piece, plan};
use quinn::SendStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::pace::Pacer;
use super::{CALLED_OFF, ConnFiles, ControlError, outcome_failure, refusal};
use crate::{Refusal, TransferFailure};

/// How much of a file the disk thread reads at a time.
const READ_CHUNK: usize = 64 * 1024;

/// Pieces read ahead of the network, at most `READ_CHUNK` bytes each.
const READ_AHEAD: usize = 8;

/// Sends the files at `paths` as transfer `id`, from the plan to the
/// receiver's outcome. Returns how many files were sent and how many
/// dropped items were skipped.
pub(super) async fn transfer(
    files: &ConnFiles,
    id: u64,
    paths: Vec<PathBuf>,
    mut replies: mpsc::Receiver<FileMessage>,
) -> Result<(u32, u32), TransferFailure> {
    let timeout = files.limits.transfer_timeout;
    let planned = tokio::task::spawn_blocking(move || plan(&paths))
        .await
        .map_err(|_| TransferFailure::Io)?;
    if planned.files.is_empty() {
        return Err(TransferFailure::NothingToSend);
    }
    if planned.files.len() > MAX_OFFER_FILES {
        return Err(TransferFailure::Refused(Refusal::TooMany));
    }
    let count = u32::try_from(planned.files.len()).unwrap_or(u32::MAX);

    let offer = FileMessage::Offer {
        id,
        files: planned
            .files
            .iter()
            .map(|file| FileEntry {
                name: file.name.clone(),
                size: file.size,
            })
            .collect(),
    };
    files.send_control(offer).map_err(|error| match error {
        // A name over the protocol's limit: the receiver could not use it.
        ControlError::Encode => TransferFailure::Refused(Refusal::BadNames),
        ControlError::Gone => TransferFailure::Disconnected,
    })?;
    // The answer may wait for the receiver's folder (on a Mac, for the user
    // to allow the Downloads folder), so it has a longer limit than
    // progress; without one, nothing was sent.
    let answer = match reply(&mut replies, files.limits.answer_timeout).await {
        Err(TransferFailure::TimedOut) => Err(TransferFailure::NoAnswer),
        other => other,
    };
    match answer? {
        FileMessage::Answer { accepted: true, .. } => {}
        FileMessage::Answer {
            refusal: Some(refused),
            ..
        } => return Err(TransferFailure::Refused(refusal(refused))),
        // No reason given, or a reply out of order.
        _ => return Err(TransferFailure::NotSupported),
    }

    let mut stream = match tokio::time::timeout(timeout, files.link.conn.open_uni()).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => return Err(TransferFailure::Disconnected),
        Err(_) => return Err(TransferFailure::TimedOut),
    };
    // Input and clipboard messages on the control stream go first.
    let _ = stream.set_priority(-1);
    let streamed = match write(
        &mut stream,
        Bytes::copy_from_slice(&id.to_be_bytes()),
        timeout,
    )
    .await
    {
        Ok(()) => stream_files(&mut stream, planned.files, files.limits.file_rate, timeout).await,
        Err(failed) => Err(failed),
    };
    match streamed {
        Ok(()) => {
            let _ = stream.finish();
        }
        Err(Stopped::Lost) => return Err(TransferFailure::Disconnected),
        Err(Stopped::Here(failed)) => {
            let _ = stream.reset(CALLED_OFF);
            return Err(failed);
        }
        // The receiver stopped the stream; its outcome says why.
        Err(Stopped::ByReceiver) => {}
    }
    match reply(&mut replies, timeout).await? {
        FileMessage::Outcome {
            failure: None,
            saved,
            ..
        } if saved == count => Ok((count, planned.skipped)),
        FileMessage::Outcome {
            failure: Some(failed),
            ..
        } => Err(outcome_failure(failed)),
        // Fewer files saved than sent, or a reply out of order: the
        // receiver did not take them all.
        _ => Err(TransferFailure::PeerFailed),
    }
}

/// The receiver's next reply about this transfer, within `timeout`.
async fn reply(
    replies: &mut mpsc::Receiver<FileMessage>,
    timeout: Duration,
) -> Result<FileMessage, TransferFailure> {
    match tokio::time::timeout(timeout, replies.recv()).await {
        Ok(Some(reply)) => Ok(reply),
        // The connection ended; `end` reports the transfer.
        Ok(None) => Err(TransferFailure::Disconnected),
        Err(_) => Err(TransferFailure::TimedOut),
    }
}

/// Why the data stream stopped before its end.
#[derive(Debug)]
enum Stopped {
    /// This side failed (a file changed or could not be read, or the
    /// receiver made no progress): call the stream off.
    Here(TransferFailure),
    /// The receiver stopped reading the stream.
    ByReceiver,
    /// The connection went away.
    Lost,
}

/// What the disk thread hands over.
enum Outgoing {
    Data(Bytes),
    Digest(Digest),
    /// A file could not be read to the end as planned.
    Failed,
}

/// Writes each file's bytes and checksum, read on a blocking thread and
/// paced to `rate` bytes per second.
async fn stream_files(
    stream: &mut SendStream,
    planned: Vec<OutgoingFile>,
    rate: u64,
    timeout: Duration,
) -> Result<(), Stopped> {
    let (pieces, mut pieces_rx) = mpsc::channel(READ_AHEAD);
    // Not awaited: it stops by itself once the pieces are not wanted.
    drop(tokio::task::spawn_blocking(move || {
        read_files(&planned, &pieces);
    }));
    let mut pacer = Pacer::new(rate);
    loop {
        let piece = match tokio::time::timeout(timeout, pieces_rx.recv()).await {
            Ok(Some(piece)) => piece,
            // Every file was read.
            Ok(None) => return Ok(()),
            Err(_) => return Err(Stopped::Here(TransferFailure::TimedOut)),
        };
        match piece {
            Outgoing::Data(bytes) => {
                let wait = pacer.delay(Instant::now(), bytes.len());
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                write(stream, bytes, timeout).await?;
            }
            Outgoing::Digest(digest) => {
                write(stream, Bytes::copy_from_slice(&digest), timeout).await?;
            }
            Outgoing::Failed => return Err(Stopped::Here(TransferFailure::Io)),
        }
    }
}

/// Writes `bytes`, failing if the receiver takes none of it for `timeout`.
async fn write(stream: &mut SendStream, bytes: Bytes, timeout: Duration) -> Result<(), Stopped> {
    match tokio::time::timeout(timeout, stream.write_chunk(bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(quinn::WriteError::Stopped(_))) => Err(Stopped::ByReceiver),
        Ok(Err(_)) => Err(Stopped::Lost),
        Err(_) => Err(Stopped::Here(TransferFailure::TimedOut)),
    }
}

/// The disk thread: reads every planned file in order. Stops at the first
/// file that cannot be read as planned, or when the pieces are not wanted
/// any more.
fn read_files(planned: &[OutgoingFile], pieces: &mpsc::Sender<Outgoing>) {
    for file in planned {
        let Ok(mut reader) = FileReader::open(file) else {
            let _ = pieces.blocking_send(Outgoing::Failed);
            return;
        };
        loop {
            let piece = match reader.next(READ_CHUNK) {
                Ok(Piece::Data(bytes)) => Outgoing::Data(Bytes::from(bytes)),
                Ok(Piece::Done(digest)) => Outgoing::Digest(digest),
                Err(_) => {
                    let _ = pieces.blocking_send(Outgoing::Failed);
                    return;
                }
            };
            let last = matches!(piece, Outgoing::Digest(_));
            if pieces.blocking_send(piece).is_err() {
                return;
            }
            if last {
                break;
            }
        }
    }
}
