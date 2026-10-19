//! The messages of a file transfer (kind 3, milestone M5), and their limits.
//!
//! A transfer is three control-stream messages around one unidirectional
//! stream that carries the data (docs/M4-M5-DESIGN.md, "The transfer
//! protocol"):
//!
//! 1. the sender's [`FileMessage::Offer`] lists the files;
//! 2. the receiver's [`FileMessage::Answer`] accepts or refuses them;
//! 3. after an accepted answer, the sender opens a unidirectional stream and
//!    writes the transfer id (8 bytes, big-endian), then for each file in
//!    offer order exactly `size` bytes followed by their SHA-256 (32 bytes),
//!    then finishes the stream;
//! 4. the receiver's [`FileMessage::Outcome`] says how many files it saved.
//!
//! Everything here is version 1 of the `files` capability. Rule 3 of the
//! evolution rules applies: a new variant of [`FileMessage`],
//! [`FileRefusal`] or [`FileFailure`] goes only to a peer that negotiated a
//! later version of `files`.

use serde::{Deserialize, Serialize};

use crate::ProtoError;

/// The most files one [`FileMessage::Offer`] may list: 1,000. A longer list
/// is rejected on encode and on decode.
pub const MAX_OFFER_FILES: usize = 1000;

/// The longest file name, in UTF-8 bytes, an offer may carry: 1,024. Every
/// file system Hop runs on allows far less (255), so a real name always
/// fits; a longer one is rejected on encode and on decode.
pub const MAX_FILE_NAME_BYTES: usize = 1024;

/// A message about one file transfer, on the control stream.
///
/// The `id` is the sender's number for the transfer; the receiver's answer
/// and outcome carry the same one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMessage {
    /// From the sender: these files, in this order, are ready to go.
    Offer {
        /// The transfer's id, unique among the sender's transfers.
        id: u64,
        /// The files, in the order their data follows. At most
        /// [`MAX_OFFER_FILES`].
        files: Vec<FileEntry>,
    },
    /// From the receiver, once per offer: whether the files may come.
    Answer {
        /// The offer's id.
        id: u64,
        /// Whether the sender may open the data stream.
        accepted: bool,
        /// Why the files were turned down; `None` when they were accepted.
        refusal: Option<FileRefusal>,
    },
    /// From the receiver, after the data (or once it gave up on it).
    Outcome {
        /// The transfer's id.
        id: u64,
        /// How many files arrived complete and were saved. On a failure
        /// these are the files before the one that failed.
        saved: u32,
        /// Why the transfer stopped early; `None` when every file was saved.
        failure: Option<FileFailure>,
    },
}

impl FileMessage {
    /// The transfer this message is about.
    pub fn id(&self) -> u64 {
        match self {
            FileMessage::Offer { id, .. }
            | FileMessage::Answer { id, .. }
            | FileMessage::Outcome { id, .. } => *id,
        }
    }
}

/// One file in an [`FileMessage::Offer`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// The file's name as the sender has it, without any folder. At most
    /// [`MAX_FILE_NAME_BYTES`]. The receiver cleans it before saving and
    /// never trusts it as a path. For the app to show; never for the log.
    pub name: String,
    /// The file's length in bytes: exactly this many bytes follow on the
    /// data stream.
    pub size: u64,
}

/// Why a receiver turned an offer down, in its [`FileMessage::Answer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRefusal {
    /// Receiving files is turned off there, or its Received folder cannot be
    /// created.
    Off,
    /// More files than the receiver takes in one transfer.
    TooMany,
    /// A file, or all of them together, is larger than the receiver takes.
    TooLarge,
    /// The receiver is busy with other transfers.
    Busy,
    /// A file name cannot be used there (or the offer named no file).
    BadNames,
}

/// Why a transfer stopped before every file was saved, in its
/// [`FileMessage::Outcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFailure {
    /// The data stream ended before every byte and checksum arrived.
    Incomplete,
    /// A file did not match its SHA-256, or bytes followed the last one.
    Corrupt,
    /// The receiver could not write a file (for example, its disk is full).
    Disk,
    /// The transfer was called off: the sender reset its data stream, or
    /// the receiver stopped for a reason of its own.
    Cancelled,
    /// The data made no progress for the receiver's timeout.
    TimedOut,
}

/// Checks the limits of an offer, so Hop never sends what its peer would
/// refuse and never keeps what it should have refused.
pub(crate) fn check_limits(message: &FileMessage) -> Result<(), ProtoError> {
    let FileMessage::Offer { files, .. } = message else {
        return Ok(());
    };
    let longest_name = files.iter().map(|file| file.name.len()).max().unwrap_or(0);
    if files.len() > MAX_OFFER_FILES || longest_name > MAX_FILE_NAME_BYTES {
        return Err(ProtoError::FileOfferTooLarge {
            files: files.len(),
            longest_name,
        });
    }
    Ok(())
}
