//! The keys the self-test types, and how it names shortcuts to the user.

use hop_types::{InputEvent, KeyCode, Os};

/// The A key (USB HID usage 0x04), for select-all.
pub(crate) const KEY_A: KeyCode = KeyCode::keyboard(0x04);
/// The C key (USB HID usage 0x06), for the copy shortcut of step 2.
pub(crate) const KEY_C: KeyCode = KeyCode::keyboard(0x06);

/// The key, and whether Shift must be held, that types `c` with a US
/// keyboard layout. `None` for anything but letters, digits and space, which
/// is all the self-test types.
pub fn key_for_char(c: char) -> Option<(KeyCode, bool)> {
    // USB HID keyboard usages: A..Z are 0x04..0x1D, 1..9 are 0x1E..0x26,
    // 0 is 0x27 and Space is 0x2C. The patterns guarantee `c` is ASCII, so
    // `as u8` keeps its value.
    let usage = match c {
        'a'..='z' => 0x04 + u16::from(c as u8 - b'a'),
        'A'..='Z' => 0x04 + u16::from(c as u8 - b'A'),
        '1'..='9' => 0x1E + u16::from(c as u8 - b'1'),
        '0' => 0x27,
        ' ' => 0x2C,
        _ => return None,
    };
    Some((KeyCode::keyboard(usage), c.is_ascii_uppercase()))
}

/// The modifier of everyday shortcuts such as copy and select all:
/// Command on macOS, Control elsewhere.
pub fn command_key(os: Os) -> KeyCode {
    match os {
        Os::MacOs => KeyCode::LEFT_META,
        Os::Windows | Os::Linux => KeyCode::LEFT_CONTROL,
    }
}

/// How the user writes the shortcut for `letter` on `os`, such as "Cmd+C".
pub(crate) fn shortcut_name(os: Os, letter: char) -> String {
    match os {
        Os::MacOs => format!("Cmd+{letter}"),
        Os::Windows | Os::Linux => format!("Ctrl+{letter}"),
    }
}

/// The emergency chord ([`hop_core::Chord::default`]) as the user types it
/// on `os`.
pub(crate) fn chord_name(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Ctrl+Option+Shift+Esc",
        Os::Windows | Os::Linux => "Ctrl+Alt+Shift+Esc",
    }
}

fn key(code: KeyCode, pressed: bool) -> InputEvent {
    InputEvent::Key {
        code,
        pressed,
        repeat: false,
    }
}

/// The key events that type `text`, with Shift held around each capital.
/// `None` if `text` has a character [`key_for_char`] does not know.
pub(crate) fn typing_events(text: &str) -> Option<Vec<InputEvent>> {
    let mut events = Vec::new();
    for c in text.chars() {
        let (code, shift) = key_for_char(c)?;
        if shift {
            events.push(key(KeyCode::LEFT_SHIFT, true));
        }
        events.push(key(code, true));
        events.push(key(code, false));
        if shift {
            events.push(key(KeyCode::LEFT_SHIFT, false));
        }
    }
    Some(events)
}

/// `modifier` held around one press of `letter`.
pub(crate) fn shortcut_events(modifier: KeyCode, letter: KeyCode) -> Vec<InputEvent> {
    vec![
        key(modifier, true),
        key(letter, true),
        key(letter, false),
        key(modifier, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_space_map_to_their_hid_usages() {
        assert_eq!(key_for_char('a'), Some((KeyCode::keyboard(0x04), false)));
        assert_eq!(key_for_char('z'), Some((KeyCode::keyboard(0x1D), false)));
        assert_eq!(key_for_char('H'), Some((KeyCode::keyboard(0x0B), true)));
        assert_eq!(key_for_char('1'), Some((KeyCode::keyboard(0x1E), false)));
        assert_eq!(key_for_char('9'), Some((KeyCode::keyboard(0x26), false)));
        assert_eq!(key_for_char('0'), Some((KeyCode::keyboard(0x27), false)));
        assert_eq!(key_for_char(' '), Some((KeyCode::keyboard(0x2C), false)));
        assert_eq!(key_for_char('a').map(|k| k.0), Some(KEY_A));
        assert_eq!(key_for_char('c').map(|k| k.0), Some(KEY_C));
    }

    #[test]
    fn other_characters_are_not_typed() {
        for c in ['!', '\n', '\t', '-', '.', 'é', 'ß', '€'] {
            assert_eq!(key_for_char(c), None, "{c:?}");
        }
        assert_eq!(typing_events("ok!"), None);
    }

    #[test]
    fn the_helper_agrees_with_the_key_table_and_both_oses_can_type_every_key() {
        let chars = ('a'..='z').chain('A'..='Z').chain('0'..='9').chain([' ']);
        for c in chars {
            let (code, shift) = key_for_char(c).unwrap();
            let name = match c {
                ' ' => "Space".to_string(),
                '0'..='9' => format!("Digit{c}"),
                _ => format!("Key{}", c.to_ascii_uppercase()),
            };
            assert_eq!(hop_keymap::from_name(&name), Some(code), "{c:?}");
            assert_eq!(shift, c.is_ascii_uppercase(), "{c:?}");
            // The injectors skip keys with no code on their OS, so a gap here
            // would silently drop characters from the typing test.
            assert!(hop_keymap::to_mac(code).is_some(), "{c:?}");
            assert!(hop_keymap::to_windows_scancode(code).is_some(), "{c:?}");
        }
        for code in [
            command_key(Os::MacOs),
            command_key(Os::Windows),
            KeyCode::LEFT_SHIFT,
        ] {
            assert!(hop_keymap::to_mac(code).is_some());
            assert!(hop_keymap::to_windows_scancode(code).is_some());
        }
    }

    #[test]
    fn typing_wraps_capitals_in_shift() {
        let events = typing_events("Hi 1").unwrap();
        let shift_down = key(KeyCode::LEFT_SHIFT, true);
        let shift_up = key(KeyCode::LEFT_SHIFT, false);
        let h = KeyCode::keyboard(0x0B);
        let i = KeyCode::keyboard(0x0C);
        let space = KeyCode::keyboard(0x2C);
        let one = KeyCode::keyboard(0x1E);
        assert_eq!(
            events,
            vec![
                shift_down,
                key(h, true),
                key(h, false),
                shift_up,
                key(i, true),
                key(i, false),
                key(space, true),
                key(space, false),
                key(one, true),
                key(one, false),
            ]
        );
        // One capital, eleven other characters: 4 + 11 * 2 events.
        assert_eq!(typing_events("Hop test 123").unwrap().len(), 26);
    }

    #[test]
    fn shortcuts_use_the_platform_modifier() {
        assert_eq!(command_key(Os::MacOs), KeyCode::LEFT_META);
        assert_eq!(command_key(Os::Windows), KeyCode::LEFT_CONTROL);
        assert_eq!(shortcut_name(Os::MacOs, 'C'), "Cmd+C");
        assert_eq!(shortcut_name(Os::Windows, 'A'), "Ctrl+A");
        assert_eq!(chord_name(Os::MacOs), "Ctrl+Option+Shift+Esc");
        assert_eq!(chord_name(Os::Windows), "Ctrl+Alt+Shift+Esc");
        assert_eq!(
            shortcut_events(KeyCode::LEFT_META, KEY_A),
            vec![
                key(KeyCode::LEFT_META, true),
                key(KEY_A, true),
                key(KEY_A, false),
                key(KeyCode::LEFT_META, false),
            ]
        );
    }
}
