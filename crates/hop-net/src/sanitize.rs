//! Making text from another device safe to show and to log.

/// The longest a peer's name or version may be once cleaned, in
/// characters (docs/M2-DESIGN.md, "`Hello` checks").
pub(crate) const MAX_DISPLAY_CHARS: usize = 64;

/// `text` from a peer or the network, made safe to show in a terminal or
/// write to a log: control characters (line breaks, terminal escape codes)
/// and the invisible characters that reorder text are removed, surrounding
/// spaces are trimmed, and the result is cut to `max_chars` characters.
///
/// Removing the reordering characters stops a name like "PC\u{202E}caM"
/// from being shown as something else, which matters because the pairing
/// prompt asks the user to trust a name.
pub(crate) fn display_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control() && !is_reordering(*c))
        .collect();
    cleaned.trim().chars().take(max_chars).collect()
}

/// `bytes` from the network (a close reason, an mDNS name) as display text:
/// invalid UTF-8 is replaced, then [`display_text`] applies.
pub(crate) fn display_bytes(bytes: &[u8], max_chars: usize) -> String {
    display_text(&String::from_utf8_lossy(bytes), max_chars)
}

/// The Unicode characters that change the display order of text around
/// them (bidirectional marks, embeddings, overrides and isolates), plus the
/// zero-width characters that hide text and the line and paragraph
/// separators, which break lines without being control characters.
fn is_reordering(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_reordering_characters_are_removed() {
        assert_eq!(
            display_text("Krish's\nPC\u{1b}[31m\u{202E}x", 64),
            "Krish'sPC[31mx"
        );
        assert_eq!(display_text("  spaced  ", 64), "spaced");
        assert_eq!(display_text("\u{200B}\u{FEFF}", 64), "");
    }

    #[test]
    fn unicode_line_and_paragraph_separators_are_removed() {
        // Not control characters by category, but line breaks all the same:
        // a viewer that honours them would show a forged second log line.
        assert_eq!(
            display_text("PC\u{2028}WARN fake line\u{2029}x", 64),
            "PCWARN fake linex"
        );
    }

    #[test]
    fn text_is_cut_by_characters_not_bytes() {
        let long = "é".repeat(100);
        let cut = display_text(&long, 64);
        assert_eq!(cut.chars().count(), 64);
        assert_eq!(display_text("abc", 2), "ab");
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        assert_eq!(display_bytes(b"ok\xff\n", 64), "ok\u{FFFD}");
    }
}
