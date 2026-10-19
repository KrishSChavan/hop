//! What the agent hears about file transfers (milestone M5;
//! docs/M4-M5-DESIGN.md, "Files to the Received folder").
//!
//! These events are for the agent, not the core: the core never sees
//! files. The app turns them into sentences, so each variant says in plain
//! words what happened. Logs get counts and reasons only, never file names.

use std::path::PathBuf;

use hop_types::DeviceId;

/// How a file transfer ended, delivered as
/// [`NetEvent::Transfer`](crate::NetEvent::Transfer): exactly one per
/// transfer, before its connection's `PeerDown` or the `PeerUp` of the
/// connection that replaces it. Only a shutdown ends transfers without one.
///
/// An offer the receiving computer did not take ends there with a `Failed`
/// too, but only when the cause is on that computer, so the user can see
/// it where it can be fixed: its Received folder cannot be used
/// ([`Refusal::Off`]), or it was ready only after the sender stopped
/// waiting ([`TransferFailure::NoAnswer`]). Other refusals are told on the
/// sending side alone.
#[derive(Clone, Debug, PartialEq)]
pub enum TransferEvent {
    /// Files from the other computer arrived complete and were saved in the
    /// Received folder.
    Received {
        /// The computer that sent them.
        from: DeviceId,
        /// The transfer's id, as the sender numbered it.
        id: u64,
        /// Where each file was saved, under its final name, in the order
        /// they were sent. For the app to show; never for the log.
        files: Vec<PathBuf>,
    },
    /// Files this computer sent arrived complete on the other computer.
    Sent {
        /// The computer that received them.
        to: DeviceId,
        /// The id [`NetHandle::send_files`](crate::NetHandle::send_files)
        /// returned.
        id: u64,
        /// How many files were sent.
        files: u32,
        /// How many dropped items were left out: folders, and files that
        /// could not be read.
        skipped: u32,
    },
    /// A transfer stopped before it was complete. Files that had arrived
    /// complete by then stay in the Received folder; nothing of the file
    /// that was arriving, or of the files after it, is kept.
    Failed {
        /// The other computer.
        peer: DeviceId,
        /// The transfer's id.
        id: u64,
        /// Whether this computer was the one sending.
        outgoing: bool,
        /// Why it stopped.
        reason: TransferFailure,
    },
}

/// Why a transfer stopped before it was complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferFailure {
    /// The receiving computer turned the files down before any were sent.
    Refused(Refusal),
    /// None of the dropped items was a file that could be read (folders are
    /// not sent).
    NothingToSend,
    /// The other computer cannot receive files: its Hop is too old, or
    /// receiving files is turned off there.
    NotSupported,
    /// The offer was not answered in time, so no file moved. Sending: the
    /// other computer did not answer within
    /// [`Limits::answer_timeout`](crate::Limits::answer_timeout) (2
    /// minutes); on a Mac, the first files wait until the user allows Hop
    /// to use the Downloads folder. Receiving: this computer's folder was
    /// ready only after the other computer had stopped waiting, so no
    /// answer went; or this computer accepted the files but they did not
    /// start coming within 30 seconds, most likely because the answer came
    /// too late.
    NoAnswer,
    /// The connection to the other computer closed during the transfer.
    Disconnected,
    /// The transfer made no progress for 30 seconds, or the receiver's
    /// outcome did not come within 30 seconds of the data.
    TimedOut,
    /// A file arrived damaged (it did not match its checksum), so it was not
    /// kept.
    Corrupt,
    /// This computer could not go on: sending, a file could not be read
    /// (it changed while it was being sent, say); receiving, a file could
    /// not be saved (the disk is full, say).
    Io,
    /// The other computer could not go on: sending, it could not save a
    /// file (its disk is full, say); receiving, it stopped sending (a file
    /// there changed while it was being sent, say). Its own report says
    /// why.
    PeerFailed,
}

/// Why the receiving computer turned files down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// That computer cannot save files in its Received folder: the folder
    /// cannot be created or used. On a Mac, Hop may not be allowed to use
    /// the Downloads folder (System Settings > Privacy & Security > Files &
    /// Folders). (A computer with receiving files turned off never refuses:
    /// it does not offer `files` at all, so nothing is offered to it.)
    Off,
    /// More files than one transfer can carry (1,000).
    TooMany,
    /// A file is larger than 4 GiB, or all of them together are larger than
    /// 16 GiB.
    TooLarge,
    /// That computer is busy with other transfers; try again once they are
    /// done.
    Busy,
    /// A file name cannot be used on that computer.
    BadNames,
}
