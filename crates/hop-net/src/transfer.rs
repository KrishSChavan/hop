//! What the agent hears about file transfers (milestone M5;
//! docs/M4-M5-DESIGN.md, "Files to the Received folder").
//!
//! These events are for the agent, not the core: the core never sees
//! files. The app turns them into sentences, so each variant says in plain
//! words what happened. Logs get counts and reasons only, never file names.

use std::path::PathBuf;

use hop_types::DeviceId;

/// What happened to a file transfer, delivered as
/// [`NetEvent::Transfer`](crate::NetEvent::Transfer). Only events from the
/// peer's current connection are delivered.
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
    /// The connection to the other computer closed during the transfer.
    Disconnected,
    /// The other computer did not answer, or the transfer made no progress,
    /// for 30 seconds.
    TimedOut,
    /// A file arrived damaged (it did not match its checksum), so it was not
    /// kept.
    Corrupt,
    /// A file could not be read or written: for example, it changed while it
    /// was being sent, or the disk is full.
    Io,
}

/// Why the receiving computer turned files down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Receiving files is turned off on that computer.
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
