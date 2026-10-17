//! Every key on a US ANSI keyboard survives the trip from HID to each OS and
//! back, and has a W3C name that resolves to it.

use std::collections::BTreeSet;

use hop_keymap::{from_mac, from_name, from_windows_scancode, name, to_mac, to_windows_scancode};
use hop_types::KeyCode;

/// The keys of a full-size US ANSI keyboard that both a Mac and a PC have,
/// as (HID usage, W3C name).
fn shared_keys() -> Vec<(u16, String)> {
    let mut keys = Vec::new();
    for (usage, letter) in (0x04..=0x1D).zip('A'..='Z') {
        keys.push((usage, format!("Key{letter}")));
    }
    // HID numbers the digit row from 1 to 9 and then 0, like the keyboard.
    for (usage, digit) in (0x1E..=0x27).zip([1, 2, 3, 4, 5, 6, 7, 8, 9, 0]) {
        keys.push((usage, format!("Digit{digit}")));
    }
    for (usage, number) in (0x3A..=0x45).zip(1..=12) {
        keys.push((usage, format!("F{number}")));
    }
    let named = [
        (0x28, "Enter"),
        (0x29, "Escape"),
        (0x2A, "Backspace"),
        (0x2B, "Tab"),
        (0x2C, "Space"),
        (0x2D, "Minus"),
        (0x2E, "Equal"),
        (0x2F, "BracketLeft"),
        (0x30, "BracketRight"),
        (0x31, "Backslash"),
        (0x33, "Semicolon"),
        (0x34, "Quote"),
        (0x35, "Backquote"),
        (0x36, "Comma"),
        (0x37, "Period"),
        (0x38, "Slash"),
        (0x39, "CapsLock"),
        (0x49, "Insert"),
        (0x4A, "Home"),
        (0x4B, "PageUp"),
        (0x4C, "Delete"),
        (0x4D, "End"),
        (0x4E, "PageDown"),
        (0x4F, "ArrowRight"),
        (0x50, "ArrowLeft"),
        (0x51, "ArrowDown"),
        (0x52, "ArrowUp"),
        (0x53, "NumLock"),
        (0x54, "NumpadDivide"),
        (0x55, "NumpadMultiply"),
        (0x56, "NumpadSubtract"),
        (0x57, "NumpadAdd"),
        (0x58, "NumpadEnter"),
        (0x59, "Numpad1"),
        (0x5A, "Numpad2"),
        (0x5B, "Numpad3"),
        (0x5C, "Numpad4"),
        (0x5D, "Numpad5"),
        (0x5E, "Numpad6"),
        (0x5F, "Numpad7"),
        (0x60, "Numpad8"),
        (0x61, "Numpad9"),
        (0x62, "Numpad0"),
        (0x63, "NumpadDecimal"),
        (0x65, "ContextMenu"),
        (0xE0, "ControlLeft"),
        (0xE1, "ShiftLeft"),
        (0xE2, "AltLeft"),
        (0xE3, "MetaLeft"),
        (0xE4, "ControlRight"),
        (0xE5, "ShiftRight"),
        (0xE6, "AltRight"),
        (0xE7, "MetaRight"),
    ];
    keys.extend(named.map(|(usage, key)| (usage, key.to_owned())));
    keys
}

/// The keys of a US ANSI PC keyboard that a Mac does not have.
const PC_ONLY: [(u16, &str); 3] = [(0x46, "PrintScreen"), (0x47, "ScrollLock"), (0x48, "Pause")];

fn all_keys() -> Vec<(u16, String)> {
    let mut keys = shared_keys();
    keys.extend(PC_ONLY.map(|(usage, key)| (usage, key.to_owned())));
    keys
}

#[test]
fn the_list_covers_the_whole_keyboard_once() {
    let keys = all_keys();
    // 26 letters, 10 digits, 12 function keys, 11 punctuation keys; Enter,
    // Escape, Backspace, Tab, Space and Caps Lock; 6 editing keys, 4 arrows,
    // 17 keypad keys, the Menu key, 8 modifiers, and Print Screen, Scroll
    // Lock and Pause: the 104 keys of a full-size board.
    assert_eq!(keys.len(), 26 + 10 + 12 + 11 + 6 + 6 + 4 + 17 + 1 + 8 + 3);
    assert_eq!(keys.len(), 104);
    let usages: BTreeSet<u16> = keys.iter().map(|&(usage, _)| usage).collect();
    assert_eq!(usages.len(), keys.len(), "a key is listed twice");
}

#[test]
fn every_shared_key_round_trips_through_macos() {
    for (usage, key) in shared_keys() {
        let code = KeyCode::keyboard(usage);
        let mac = to_mac(code).unwrap_or_else(|| panic!("{key} has no macOS code"));
        assert_eq!(from_mac(mac), Some(code), "{key}");
    }
}

#[test]
fn every_key_round_trips_through_windows() {
    for (usage, key) in all_keys() {
        let code = KeyCode::keyboard(usage);
        let (scancode, extended) =
            to_windows_scancode(code).unwrap_or_else(|| panic!("{key} has no Windows scancode"));
        assert_eq!(
            from_windows_scancode(scancode, extended),
            Some(code),
            "{key}"
        );
    }
}

#[test]
fn every_key_has_its_w3c_name() {
    for (usage, key) in all_keys() {
        let code = KeyCode::keyboard(usage);
        assert_eq!(name(code), Some(key.as_str()));
        assert_eq!(from_name(&key), Some(code));
    }
}

#[test]
fn pc_only_keys_have_no_macos_code() {
    for (usage, key) in PC_ONLY {
        assert_eq!(to_mac(KeyCode::keyboard(usage)), None, "{key}");
    }
}
