//! Clipboard rules (milestone M4): which items are never read, and turning
//! the clipboard's UTF-16 text into Hop's text and back
//! (docs/M4-M5-DESIGN.md, section 4).
//!
//! # Private items
//!
//! Password managers and other apps that copy secrets mark the item with
//! extra clipboard formats, which Windows' own clipboard history and cloud
//! clipboard honour. Hop honours the same ones and never reads such an
//! item's text:
//! - [`EXCLUDE_FROM_MONITORING`] or [`VIEWER_IGNORE`], whatever they hold;
//! - [`CAN_INCLUDE_IN_HISTORY`] or [`CAN_UPLOAD_TO_CLOUD`] holding the
//!   DWORD 0. A mark whose value cannot be read counts as 0: when in doubt,
//!   the text stays on this PC.
//!
//! # Text
//!
//! Windows keeps text as `CF_UNICODETEXT`: UTF-16 in a global memory block,
//! ending in a NUL, with CRLF line endings. The block can be larger than
//! the text (`GlobalSize` rounds up) and, from a careless app, may lack the
//! NUL, so the text runs to the first NUL or the end of the block. Inside
//! Hop, text is UTF-8 with LF line endings. Text longer than the caller's
//! limit is never cut short: it is not read at all. An empty text counts as
//! no text: sending it would only empty the other computer's clipboard.

use hop_platform::{to_crlf, to_lf};

/// Marks a clipboard item as not to be read by clipboard monitors, whatever
/// it holds.
pub const EXCLUDE_FROM_MONITORING: &str = "ExcludeClipboardContentFromMonitorProcessing";

/// The older mark for the same, from before Windows 10's clipboard history.
pub const VIEWER_IGNORE: &str = "Clipboard Viewer Ignore";

/// Holds a DWORD: 0 keeps the item out of Windows' clipboard history.
pub const CAN_INCLUDE_IN_HISTORY: &str = "CanIncludeInClipboardHistory";

/// Holds a DWORD: 0 keeps the item from syncing to the user's other
/// devices through Windows' cloud clipboard.
pub const CAN_UPLOAD_TO_CLOUD: &str = "CanUploadToCloudClipboard";

/// How many times Hop tries to open the clipboard: another app may hold it
/// open for a moment.
pub const OPEN_TRIES: u32 = 5;

/// How long Hop waits between those tries, in milliseconds.
pub const OPEN_RETRY_MS: u64 = 5;

/// A clipboard format that holds a DWORD flag, as found on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    /// Not on the clipboard.
    Absent,
    /// On the clipboard, holding this value.
    Value(u32),
    /// On the clipboard, but its value could not be read.
    Unreadable,
}

impl Flag {
    /// Whether the flag keeps the item private: it holds 0, or an
    /// unreadable value.
    fn refuses(self) -> bool {
        matches!(self, Flag::Value(0) | Flag::Unreadable)
    }
}

/// The DWORD a flag format's data holds: its first four bytes,
/// little-endian, as Windows writes a DWORD.
pub fn dword(data: &[u8]) -> Flag {
    match data.first_chunk::<4>() {
        Some(bytes) => Flag::Value(u32::from_le_bytes(*bytes)),
        None => Flag::Unreadable,
    }
}

/// The marks on the clipboard's current item that can make it private (see
/// the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marks {
    /// [`EXCLUDE_FROM_MONITORING`] is on the clipboard.
    pub exclude_from_monitoring: bool,
    /// [`VIEWER_IGNORE`] is on the clipboard.
    pub viewer_ignore: bool,
    /// [`CAN_INCLUDE_IN_HISTORY`].
    pub history: Flag,
    /// [`CAN_UPLOAD_TO_CLOUD`].
    pub cloud: Flag,
}

impl Marks {
    /// Whether the item's text must never be read.
    pub fn private(&self) -> bool {
        self.exclude_from_monitoring
            || self.viewer_ignore
            || self.history.refuses()
            || self.cloud.refuses()
    }
}

/// How many UTF-16 units of a `CF_UNICODETEXT` block (all of it, `GlobalSize`
/// over 2) are text: up to the first NUL, or the whole block if it has
/// none. `None` when the text is certainly longer than `max_bytes` of UTF-8,
/// which is decided without looking past `2 * max_bytes + 1` units, so a
/// huge block costs nothing: the shortest a unit can become is half a byte
/// (a CRLF, two units, becomes one LF).
pub fn text_units(block: &[u16], max_bytes: usize) -> Option<usize> {
    let limit = max_bytes.saturating_mul(2).saturating_add(1);
    match block.iter().take(limit).position(|&unit| unit == 0) {
        Some(nul) => Some(nul),
        None if block.len() < limit => Some(block.len()),
        None => None,
    }
}

/// The text in `units` (the part [`text_units`] found) as Hop keeps it:
/// UTF-8 with LF line endings. `None` if it is empty, or longer than
/// `max_bytes`.
///
/// A lone surrogate, which is not text, becomes U+FFFD.
pub fn decode_text(units: &[u16], max_bytes: usize) -> Option<String> {
    if units.is_empty() {
        return None;
    }
    let text = String::from_utf16_lossy(units);
    // `to_lf` only changes text that has a CR.
    let text = if text.contains('\r') {
        to_lf(&text).into_owned()
    } else {
        text
    };
    (text.len() <= max_bytes).then_some(text)
}

/// `text` as `CF_UNICODETEXT` holds it: CRLF line endings, UTF-16, and a
/// final NUL.
pub fn encode_text(text: &str) -> Vec<u16> {
    let mut units: Vec<u16> = to_crlf(text).encode_utf16().collect();
    units.push(0);
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Marks {
        /// An item with none of the marks.
        const NONE: Marks = Marks {
            exclude_from_monitoring: false,
            viewer_ignore: false,
            history: Flag::Absent,
            cloud: Flag::Absent,
        };
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// What reading a block gives, start to end.
    fn read(block: &[u16], max_bytes: usize) -> Option<String> {
        let len = text_units(block, max_bytes)?;
        decode_text(&block[..len], max_bytes)
    }

    #[test]
    fn an_item_without_marks_is_read() {
        assert!(!Marks::NONE.private());
        // Apps that mark an item as fine for the history and the cloud.
        let allowed = Marks {
            history: Flag::Value(1),
            cloud: Flag::Value(1),
            ..Marks::NONE
        };
        assert!(!allowed.private());
    }

    #[test]
    fn password_manager_items_are_never_read() {
        for marks in [
            Marks {
                exclude_from_monitoring: true,
                ..Marks::NONE
            },
            Marks {
                viewer_ignore: true,
                ..Marks::NONE
            },
            Marks {
                history: Flag::Value(0),
                ..Marks::NONE
            },
            Marks {
                cloud: Flag::Value(0),
                ..Marks::NONE
            },
            // Allowed in one place and refused in the other is refused.
            Marks {
                history: Flag::Value(1),
                cloud: Flag::Value(0),
                ..Marks::NONE
            },
            // The exclusion wins whatever the flags say.
            Marks {
                exclude_from_monitoring: true,
                history: Flag::Value(1),
                cloud: Flag::Value(1),
                viewer_ignore: false,
            },
        ] {
            assert!(marks.private(), "{marks:?}");
        }
    }

    #[test]
    fn an_unreadable_flag_counts_as_refused() {
        assert!(
            Marks {
                history: Flag::Unreadable,
                ..Marks::NONE
            }
            .private()
        );
        assert!(
            Marks {
                cloud: Flag::Unreadable,
                history: Flag::Value(1),
                ..Marks::NONE
            }
            .private()
        );
    }

    #[test]
    fn flags_hold_a_little_endian_dword() {
        assert_eq!(dword(&[0, 0, 0, 0]), Flag::Value(0));
        assert_eq!(dword(&[1, 0, 0, 0]), Flag::Value(1));
        assert_eq!(dword(&[0, 0, 0, 1]), Flag::Value(0x0100_0000));
        // GlobalSize rounds up: bytes after the DWORD are padding.
        assert_eq!(dword(&[1, 0, 0, 0, 0xAA, 0xBB]), Flag::Value(1));
        // Too short to hold one.
        assert_eq!(dword(&[1, 0, 0]), Flag::Unreadable);
        assert_eq!(dword(&[]), Flag::Unreadable);
    }

    #[test]
    fn text_runs_to_the_first_nul() {
        let mut block = wide("first line\r\nsecond");
        block.push(0);
        // Padding after the NUL, as GlobalSize's rounding leaves it.
        block.extend([0x41, 0x42, 0]);
        assert_eq!(read(&block, 1024).as_deref(), Some("first line\nsecond"));
        // A block without the NUL ends at its end.
        assert_eq!(read(&wide("no nul"), 1024).as_deref(), Some("no nul"));
        // An empty text is no text, whatever follows its NUL.
        assert_eq!(text_units(&[0, 0x41], 1024), Some(0));
        assert_eq!(read(&[0, 0x41], 1024), None);
        assert_eq!(read(&[], 1024), None);
    }

    #[test]
    fn line_endings_become_lf_and_text_is_utf8() {
        let mut block = wide("caf\u{e9} \u{1F600}\r\nold mac\rend\n");
        block.push(0);
        assert_eq!(
            read(&block, 1024).as_deref(),
            Some("caf\u{e9} \u{1F600}\nold mac\nend\n")
        );
        // A lone surrogate is not text: it becomes U+FFFD.
        assert_eq!(
            read(&[0x61, 0xD800, 0x62, 0], 1024).as_deref(),
            Some("a\u{FFFD}b")
        );
    }

    #[test]
    fn text_over_the_limit_is_not_read_never_cut_short() {
        // "é" is 2 bytes of UTF-8: 3 of them are 6 bytes.
        let block = [0xE9, 0xE9, 0xE9, 0];
        assert_eq!(read(&block, 6).as_deref(), Some("\u{e9}\u{e9}\u{e9}"));
        assert_eq!(read(&block, 5), None);
        // The limit counts the text after CRLF became LF.
        let mut crlf = wide("a\r\nb");
        crlf.push(0);
        assert_eq!(read(&crlf, 3).as_deref(), Some("a\nb"));
        assert_eq!(read(&crlf, 2), None);
        assert_eq!(read(&[0x41, 0], 0), None);
        assert_eq!(read(&[0], 0), None);
    }

    #[test]
    fn a_huge_block_is_refused_without_decoding_it() {
        // Every unit could be half of a CRLF, so up to 2 * max units may
        // still fit; one more cannot.
        let max = 4;
        let pairs = wide("\r\n\r\n\r\n\r\n");
        assert_eq!(text_units(&pairs, max), Some(8));
        assert_eq!(read(&pairs, max).as_deref(), Some("\n\n\n\n"));
        let mut longer = pairs.clone();
        longer.push(0x41);
        assert_eq!(text_units(&longer, max), None);
        // A NUL after the limit does not help.
        longer.push(0);
        assert_eq!(text_units(&longer, max), None);
        // 64 MiB without a NUL is refused at once for a 1 MiB limit.
        let huge = vec![0x41_u16; 32 << 20];
        assert_eq!(text_units(&huge, 1 << 20), None);
        // A NUL right at the limit still ends the text in time.
        let mut edge = vec![0x41_u16; 2 * max];
        edge.push(0);
        assert_eq!(text_units(&edge, max), Some(2 * max));
        assert_eq!(text_units(&[], usize::MAX), Some(0));
    }

    #[test]
    fn text_is_written_with_crlf_and_a_final_nul() {
        assert_eq!(encode_text(""), [0]);
        assert_eq!(encode_text("a\nb"), [0x61, 0x0D, 0x0A, 0x62, 0]);
        // CRLF already there is not doubled; a lone CR becomes CRLF.
        assert_eq!(encode_text("a\r\nb\rc"), wide("a\r\nb\r\nc\0"));
        assert_eq!(encode_text("\u{1F600}"), [0xD83D, 0xDE00, 0]);
    }

    #[test]
    fn text_survives_the_round_trip() {
        // Written as is, though reading it back gives no text.
        assert_eq!(read(&encode_text(""), 0), None);
        for text in [
            "plain",
            "two\nlines\n",
            "caf\u{e9} \u{1F600} \u{4E2D}\u{6587}",
            "\n\n\n",
            "tab\tand trailing space ",
        ] {
            let block = encode_text(text);
            assert_eq!(block.last(), Some(&0));
            assert_eq!(read(&block, text.len()).as_deref(), Some(text), "{text:?}");
            assert_eq!(read(&block, text.len() - 1), None, "{text:?}");
        }
    }
}
