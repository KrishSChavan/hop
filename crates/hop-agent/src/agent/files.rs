//! Files dropped at the edge of the screen, and the words for what happens
//! to them (docs/M4-M5-DESIGN.md, section 5.3).
//!
//! The drop zones are the parts of this computer's screen edges that lead
//! to the other computer ([`drop_zones`]). What happens to the files after
//! that is told in status lines, which only ever count files and give
//! reasons: file names never go in a status line or the log. They reach the
//! app alone, in
//! [`AgentUpdate::FilesReceived`](crate::service::AgentUpdate::FilesReceived).

use std::path::PathBuf;

use hop_core::SharedEdge;
use hop_net::{Refusal, SendError, TransferFailure};
use hop_platform::DropZone;

/// The drop zones along `edges`, the parts of this computer's screen edges
/// from which the cursor crosses to the other computer.
pub(crate) fn drop_zones(edges: Vec<SharedEdge>) -> Vec<DropZone> {
    edges
        .into_iter()
        .map(|edge| DropZone {
            display: edge.frame,
            edge: edge.edge,
            start: edge.start,
            end: edge.end,
        })
        .collect()
}

/// The label on the drop zone: "Drop to send to Windows PC".
pub(crate) fn drop_label(peer: &str) -> String {
    format!("Drop to send to {peer}")
}

/// `n` of something, in words: "1 file", "3 files".
fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// What was dropped, counted: "1 item", "3 items". Items, not files: a
/// dropped folder is only left out later, when the files are read.
fn items(n: u64) -> String {
    count(n, "item", "items")
}

/// "1 file", "3 files".
fn files(n: u64) -> String {
    count(n, "file", "files")
}

/// A length as the number the words count.
fn number(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Said when dropped files start on their way: "Sending 3 items to Windows
/// PC…".
pub(crate) fn sending(dropped: usize, peer: &str) -> String {
    format!("Sending {} to {peer}…", items(number(dropped)))
}

/// Said when dropped files could not even start on their way: the other
/// computer is not connected, cannot take files, or the network refused.
pub(crate) fn not_sent(error: &SendError, peer: &str) -> String {
    let why = match error {
        SendError::NotConnected => format!("{peer} is not connected"),
        SendError::NotNegotiated => format!(
            "{peer} cannot receive files (its Hop is older, or receiving files is turned off \
             there)"
        ),
        SendError::Overloaded => format!("the connection to {peer} was overloaded"),
        SendError::Encode(_) => "Hop could not offer them".to_owned(),
    };
    format!("The files you dropped were not sent: {why}.")
}

/// Said when files from the other computer arrived and their folder opens.
pub(crate) fn received(count: usize, peer: &str) -> String {
    match count {
        1 => format!("Received 1 file from {peer}. Its folder opens now."),
        n => format!(
            "Received {} from {peer}. Their folder opens now.",
            files(number(n))
        ),
    }
}

/// Said when files this computer sent all arrived on the other computer,
/// with how many dropped items were left out.
pub(crate) fn sent(sent: u32, skipped: u32, peer: &str) -> String {
    let done = format!("Sent {} to {peer}.", files(u64::from(sent)));
    match skipped {
        0 => done,
        1 => format!("{done} Left out 1 item: a folder, or a file Hop could not read."),
        n => format!(
            "{done} Left out {}: folders, or files Hop could not read.",
            items(u64::from(n))
        ),
    }
}

/// Said when a transfer stopped before it was complete: whether this
/// computer was sending, and why, in plain words.
pub(crate) fn failed(outgoing: bool, reason: &TransferFailure, peer: &str) -> String {
    if outgoing {
        failed_sending(reason, peer)
    } else {
        failed_receiving(reason, peer)
    }
}

fn failed_sending(reason: &TransferFailure, peer: &str) -> String {
    let refused = |why: String| format!("{peer} did not take the files: {why}.");
    let partly = |why: &str| {
        format!(
            "Not all the files reached {peer}: {why}. The ones that arrived complete are kept \
             there."
        )
    };
    match reason {
        TransferFailure::Refused(Refusal::Off) => {
            refused("receiving files is turned off there".to_owned())
        }
        TransferFailure::Refused(Refusal::TooMany) => {
            refused("it takes at most 1,000 files at once".to_owned())
        }
        TransferFailure::Refused(Refusal::TooLarge) => {
            refused("it takes files of up to 4 GB each, and up to 16 GB at once".to_owned())
        }
        TransferFailure::Refused(Refusal::Busy) => refused(
            "it is busy with other files. Drop these again once those have arrived".to_owned(),
        ),
        TransferFailure::Refused(Refusal::BadNames) => {
            refused("a file name cannot be used there".to_owned())
        }
        TransferFailure::NothingToSend => format!(
            "Nothing was sent to {peer}: none of the dropped items is a file Hop can read \
             (folders are not sent)."
        ),
        TransferFailure::NotSupported => format!(
            "Nothing was sent: {peer} cannot receive files (its Hop is older, or receiving files \
             is turned off there)."
        ),
        TransferFailure::Disconnected => partly("the connection closed"),
        TransferFailure::TimedOut => partly("nothing moved for 30 seconds"),
        TransferFailure::Corrupt => partly("a file arrived damaged, so it was not kept"),
        TransferFailure::Io => {
            partly("a file could not be read (it may have changed while it was sent)")
        }
    }
}

fn failed_receiving(reason: &TransferFailure, peer: &str) -> String {
    let refused = |why: &str| format!("Turned down files from {peer}: {why}.");
    let partly = |why: &str| {
        format!(
            "Not all the files from {peer} arrived: {why}. The ones that arrived complete are \
             in the Received folder."
        )
    };
    match reason {
        TransferFailure::Refused(Refusal::Off) => {
            refused("receiving files is turned off on this computer")
        }
        TransferFailure::Refused(Refusal::TooMany) => refused("more than 1,000 files at once"),
        TransferFailure::Refused(Refusal::TooLarge) => {
            refused("a file is over 4 GB, or all of them together are over 16 GB")
        }
        TransferFailure::Refused(Refusal::Busy) => {
            refused("this computer is busy with other files")
        }
        TransferFailure::Refused(Refusal::BadNames) => {
            refused("a file name cannot be used on this computer")
        }
        TransferFailure::Disconnected => partly("the connection closed"),
        TransferFailure::TimedOut => partly("nothing arrived for 30 seconds"),
        TransferFailure::Corrupt => partly("a file arrived damaged, so it was not kept"),
        TransferFailure::Io => partly("a file could not be saved (the disk may be full)"),
        // Only the sending side says these.
        TransferFailure::NothingToSend | TransferFailure::NotSupported => {
            partly("the transfer stopped")
        }
    }
}

/// A short, fixed name for why a transfer stopped, for the log. It never
/// names a file.
pub(crate) fn failure_kind(reason: &TransferFailure) -> &'static str {
    match reason {
        TransferFailure::Refused(Refusal::Off) => "refused: receiving files is off",
        TransferFailure::Refused(Refusal::TooMany) => "refused: too many files",
        TransferFailure::Refused(Refusal::TooLarge) => "refused: too large",
        TransferFailure::Refused(Refusal::Busy) => "refused: busy",
        TransferFailure::Refused(Refusal::BadNames) => "refused: unusable names",
        TransferFailure::NothingToSend => "nothing to send",
        TransferFailure::NotSupported => "not supported",
        TransferFailure::Disconnected => "disconnected",
        TransferFailure::TimedOut => "timed out",
        TransferFailure::Corrupt => "a file arrived damaged",
        TransferFailure::Io => "a file could not be read or written",
    }
}

/// The names of `files`, for the app to show (never for the log).
pub(crate) fn names(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use hop_types::{Edge, Rect};

    use super::*;

    #[test]
    fn each_edge_that_leads_to_the_peer_becomes_a_zone() {
        let laptop = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let screen = Rect::new(1512.0, -200.0, 1920.0, 1080.0);
        let edges = vec![
            SharedEdge {
                display: 1,
                frame: laptop,
                edge: Edge::Right,
                start: 10.0,
                end: 972.0,
            },
            SharedEdge {
                display: 2,
                frame: screen,
                edge: Edge::Top,
                start: 1522.0,
                end: 3422.0,
            },
        ];
        assert_eq!(
            drop_zones(edges),
            [
                DropZone {
                    display: laptop,
                    edge: Edge::Right,
                    start: 10.0,
                    end: 972.0,
                },
                DropZone {
                    display: screen,
                    edge: Edge::Top,
                    start: 1522.0,
                    end: 3422.0,
                },
            ]
        );
        assert!(drop_zones(Vec::new()).is_empty());
    }

    /// docs/INSTALL.md quotes this label.
    #[test]
    fn the_zone_says_where_the_files_go() {
        assert_eq!(drop_label("Windows PC"), "Drop to send to Windows PC");
    }

    #[test]
    fn sending_counts_what_was_dropped() {
        assert_eq!(sending(1, "Windows PC"), "Sending 1 item to Windows PC…");
        assert_eq!(sending(3, "Windows PC"), "Sending 3 items to Windows PC…");
        assert_eq!(
            not_sent(&SendError::NotConnected, "Windows PC"),
            "The files you dropped were not sent: Windows PC is not connected."
        );
        assert_eq!(
            not_sent(&SendError::NotNegotiated, "Windows PC"),
            "The files you dropped were not sent: Windows PC cannot receive files (its Hop is \
             older, or receiving files is turned off there)."
        );
        assert!(not_sent(&SendError::Overloaded, "Windows PC").contains("overloaded"));
    }

    #[test]
    fn arrivals_and_deliveries_are_counted() {
        assert_eq!(
            received(1, "Mac"),
            "Received 1 file from Mac. Its folder opens now."
        );
        assert_eq!(
            received(3, "Mac"),
            "Received 3 files from Mac. Their folder opens now."
        );
        assert_eq!(sent(1, 0, "Windows PC"), "Sent 1 file to Windows PC.");
        assert_eq!(
            sent(2, 1, "Windows PC"),
            "Sent 2 files to Windows PC. Left out 1 item: a folder, or a file Hop could not read."
        );
        assert_eq!(
            sent(2, 3, "Windows PC"),
            "Sent 2 files to Windows PC. Left out 3 items: folders, or files Hop could not read."
        );
    }

    #[test]
    fn every_failure_has_its_own_plain_words() {
        let reasons = [
            TransferFailure::Refused(Refusal::Off),
            TransferFailure::Refused(Refusal::TooMany),
            TransferFailure::Refused(Refusal::TooLarge),
            TransferFailure::Refused(Refusal::Busy),
            TransferFailure::Refused(Refusal::BadNames),
            TransferFailure::NothingToSend,
            TransferFailure::NotSupported,
            TransferFailure::Disconnected,
            TransferFailure::TimedOut,
            TransferFailure::Corrupt,
            TransferFailure::Io,
        ];
        let sending: Vec<String> = reasons
            .iter()
            .map(|reason| failed(true, reason, "Windows PC"))
            .collect();
        for (reason, line) in reasons.iter().zip(&sending) {
            assert!(line.contains("Windows PC"), "{reason:?}: {line}");
            assert!(line.ends_with('.'), "{reason:?}: {line}");
        }
        let distinct: std::collections::BTreeSet<&String> = sending.iter().collect();
        assert_eq!(distinct.len(), reasons.len(), "{sending:#?}");
        assert_eq!(
            failed(true, &TransferFailure::Refused(Refusal::Off), "Windows PC"),
            "Windows PC did not take the files: receiving files is turned off there."
        );
        assert_eq!(
            failed(true, &TransferFailure::Disconnected, "Windows PC"),
            "Not all the files reached Windows PC: the connection closed. The ones that arrived \
             complete are kept there."
        );
        assert_eq!(
            failed(true, &TransferFailure::NothingToSend, "Windows PC"),
            "Nothing was sent to Windows PC: none of the dropped items is a file Hop can read \
             (folders are not sent)."
        );

        // The receiving side says it in its own words.
        assert_eq!(
            failed(false, &TransferFailure::Refused(Refusal::Busy), "Mac"),
            "Turned down files from Mac: this computer is busy with other files."
        );
        assert_eq!(
            failed(false, &TransferFailure::Corrupt, "Mac"),
            "Not all the files from Mac arrived: a file arrived damaged, so it was not kept. The \
             ones that arrived complete are in the Received folder."
        );
        for reason in &reasons {
            let line = failed(false, reason, "Mac");
            assert!(line.contains("Mac") && line.ends_with('.'), "{line}");
        }

        // The log gets a fixed name for each reason.
        let kinds: std::collections::BTreeSet<&str> = reasons.iter().map(failure_kind).collect();
        assert_eq!(kinds.len(), reasons.len(), "{kinds:?}");
    }

    #[test]
    fn names_are_the_last_part_of_each_path() {
        let folder = PathBuf::from("Downloads").join("Hop Received");
        let files = [folder.join("notes.txt"), folder.join("photo (2).jpg")];
        assert_eq!(names(&files), ["notes.txt", "photo (2).jpg"]);
        assert!(names(&[]).is_empty());
    }
}
