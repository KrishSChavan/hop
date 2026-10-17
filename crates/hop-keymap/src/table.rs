//! The key table: one row per physical key, giving its USB HID usage, its W3C
//! UI Events `code` name, its macOS virtual keycode and its Windows scancode.
//!
//! # Sources
//!
//! - macOS: the `kVK_*` constants in Carbon's `HIToolbox/Events.h` (the name of
//!   each constant is in the comment at the end of its row).
//! - Windows and names: Chromium's `keycode_converter_data.inc`, which is also
//!   the data behind the `keycode` crate. A test checks every row against that
//!   crate.
//!
//! The table lives here instead of being read from the `keycode` crate at run
//! time because that crate looks keys up with a `match` whose arms come from a
//! `HashSet`, so for codes shared by several keys (for example the "no macOS
//! key" placeholder) the answer can change from one build to the next. A
//! table we own is deterministic and easy to audit.
//!
//! # Windows notation
//!
//! Scancodes are Set 1 make codes, written the way Microsoft documents them:
//! `0xE0xx` is the two-byte sequence "E0 xx", which Windows reports as
//! scancode `xx` with the extended flag set (`LLKHF_EXTENDED` in a low-level
//! hook, `KEYEVENTF_EXTENDEDKEY` when injecting). Two Windows quirks matter:
//! Num Lock is reported as extended `0x45`, and Pause as plain `0x45`.
//!
//! The crate documentation (lib.rs) lists the keys that map to `None` and why.

use hop_types::KeyCode;

/// One physical key and its codes on each OS.
struct Key {
    code: KeyCode,
    /// W3C UI Events `code` name, such as `"KeyA"`.
    name: &'static str,
    /// macOS virtual keycode (`kVK_*`).
    mac: Option<u16>,
    /// Windows Set 1 scancode, `0xE0xx` for extended keys.
    win: Option<u16>,
}

const fn kb(usage: u16, name: &'static str, mac: Option<u16>, win: Option<u16>) -> Key {
    Key {
        code: KeyCode::keyboard(usage),
        name,
        mac,
        win,
    }
}

const fn cons(usage: u16, name: &'static str, mac: Option<u16>, win: Option<u16>) -> Key {
    Key {
        code: KeyCode::consumer(usage),
        name,
        mac,
        win,
    }
}

/// Every key Hop knows, sorted by [`KeyCode`] so lookups by code can use a
/// binary search. Tests check the order and that no code repeats in any
/// column.
#[rustfmt::skip]
static KEYS: &[Key] = &[
    // Keyboard page (0x07): letters.
    //  HID   W3C name               macOS        Windows
    kb(0x04, "KeyA",               Some(0x00), Some(0x001E)), // kVK_ANSI_A
    kb(0x05, "KeyB",               Some(0x0B), Some(0x0030)), // kVK_ANSI_B
    kb(0x06, "KeyC",               Some(0x08), Some(0x002E)), // kVK_ANSI_C
    kb(0x07, "KeyD",               Some(0x02), Some(0x0020)), // kVK_ANSI_D
    kb(0x08, "KeyE",               Some(0x0E), Some(0x0012)), // kVK_ANSI_E
    kb(0x09, "KeyF",               Some(0x03), Some(0x0021)), // kVK_ANSI_F
    kb(0x0A, "KeyG",               Some(0x05), Some(0x0022)), // kVK_ANSI_G
    kb(0x0B, "KeyH",               Some(0x04), Some(0x0023)), // kVK_ANSI_H
    kb(0x0C, "KeyI",               Some(0x22), Some(0x0017)), // kVK_ANSI_I
    kb(0x0D, "KeyJ",               Some(0x26), Some(0x0024)), // kVK_ANSI_J
    kb(0x0E, "KeyK",               Some(0x28), Some(0x0025)), // kVK_ANSI_K
    kb(0x0F, "KeyL",               Some(0x25), Some(0x0026)), // kVK_ANSI_L
    kb(0x10, "KeyM",               Some(0x2E), Some(0x0032)), // kVK_ANSI_M
    kb(0x11, "KeyN",               Some(0x2D), Some(0x0031)), // kVK_ANSI_N
    kb(0x12, "KeyO",               Some(0x1F), Some(0x0018)), // kVK_ANSI_O
    kb(0x13, "KeyP",               Some(0x23), Some(0x0019)), // kVK_ANSI_P
    kb(0x14, "KeyQ",               Some(0x0C), Some(0x0010)), // kVK_ANSI_Q
    kb(0x15, "KeyR",               Some(0x0F), Some(0x0013)), // kVK_ANSI_R
    kb(0x16, "KeyS",               Some(0x01), Some(0x001F)), // kVK_ANSI_S
    kb(0x17, "KeyT",               Some(0x11), Some(0x0014)), // kVK_ANSI_T
    kb(0x18, "KeyU",               Some(0x20), Some(0x0016)), // kVK_ANSI_U
    kb(0x19, "KeyV",               Some(0x09), Some(0x002F)), // kVK_ANSI_V
    kb(0x1A, "KeyW",               Some(0x0D), Some(0x0011)), // kVK_ANSI_W
    kb(0x1B, "KeyX",               Some(0x07), Some(0x002D)), // kVK_ANSI_X
    kb(0x1C, "KeyY",               Some(0x10), Some(0x0015)), // kVK_ANSI_Y
    kb(0x1D, "KeyZ",               Some(0x06), Some(0x002C)), // kVK_ANSI_Z
    // Digits.
    kb(0x1E, "Digit1",             Some(0x12), Some(0x0002)), // kVK_ANSI_1
    kb(0x1F, "Digit2",             Some(0x13), Some(0x0003)), // kVK_ANSI_2
    kb(0x20, "Digit3",             Some(0x14), Some(0x0004)), // kVK_ANSI_3
    kb(0x21, "Digit4",             Some(0x15), Some(0x0005)), // kVK_ANSI_4
    kb(0x22, "Digit5",             Some(0x17), Some(0x0006)), // kVK_ANSI_5
    kb(0x23, "Digit6",             Some(0x16), Some(0x0007)), // kVK_ANSI_6
    kb(0x24, "Digit7",             Some(0x1A), Some(0x0008)), // kVK_ANSI_7
    kb(0x25, "Digit8",             Some(0x1C), Some(0x0009)), // kVK_ANSI_8
    kb(0x26, "Digit9",             Some(0x19), Some(0x000A)), // kVK_ANSI_9
    kb(0x27, "Digit0",             Some(0x1D), Some(0x000B)), // kVK_ANSI_0
    // Editing keys and punctuation.
    kb(0x28, "Enter",              Some(0x24), Some(0x001C)), // kVK_Return
    kb(0x29, "Escape",             Some(0x35), Some(0x0001)), // kVK_Escape
    kb(0x2A, "Backspace",          Some(0x33), Some(0x000E)), // kVK_Delete
    kb(0x2B, "Tab",                Some(0x30), Some(0x000F)), // kVK_Tab
    kb(0x2C, "Space",              Some(0x31), Some(0x0039)), // kVK_Space
    kb(0x2D, "Minus",              Some(0x1B), Some(0x000C)), // kVK_ANSI_Minus
    kb(0x2E, "Equal",              Some(0x18), Some(0x000D)), // kVK_ANSI_Equal
    kb(0x2F, "BracketLeft",        Some(0x21), Some(0x001A)), // kVK_ANSI_LeftBracket
    kb(0x30, "BracketRight",       Some(0x1E), Some(0x001B)), // kVK_ANSI_RightBracket
    kb(0x31, "Backslash",          Some(0x2A), Some(0x002B)), // kVK_ANSI_Backslash
    kb(0x33, "Semicolon",          Some(0x29), Some(0x0027)), // kVK_ANSI_Semicolon
    kb(0x34, "Quote",              Some(0x27), Some(0x0028)), // kVK_ANSI_Quote
    kb(0x35, "Backquote",          Some(0x32), Some(0x0029)), // kVK_ANSI_Grave
    kb(0x36, "Comma",              Some(0x2B), Some(0x0033)), // kVK_ANSI_Comma
    kb(0x37, "Period",             Some(0x2F), Some(0x0034)), // kVK_ANSI_Period
    kb(0x38, "Slash",              Some(0x2C), Some(0x0035)), // kVK_ANSI_Slash
    kb(0x39, "CapsLock",           Some(0x39), Some(0x003A)), // kVK_CapsLock
    // Function keys.
    kb(0x3A, "F1",                 Some(0x7A), Some(0x003B)), // kVK_F1
    kb(0x3B, "F2",                 Some(0x78), Some(0x003C)), // kVK_F2
    kb(0x3C, "F3",                 Some(0x63), Some(0x003D)), // kVK_F3
    kb(0x3D, "F4",                 Some(0x76), Some(0x003E)), // kVK_F4
    kb(0x3E, "F5",                 Some(0x60), Some(0x003F)), // kVK_F5
    kb(0x3F, "F6",                 Some(0x61), Some(0x0040)), // kVK_F6
    kb(0x40, "F7",                 Some(0x62), Some(0x0041)), // kVK_F7
    kb(0x41, "F8",                 Some(0x64), Some(0x0042)), // kVK_F8
    kb(0x42, "F9",                 Some(0x65), Some(0x0043)), // kVK_F9
    kb(0x43, "F10",                Some(0x6D), Some(0x0044)), // kVK_F10
    kb(0x44, "F11",                Some(0x67), Some(0x0057)), // kVK_F11
    kb(0x45, "F12",                Some(0x6F), Some(0x0058)), // kVK_F12
    kb(0x46, "PrintScreen",        None,       Some(0xE037)),
    kb(0x47, "ScrollLock",         None,       Some(0x0046)),
    kb(0x48, "Pause",              None,       Some(0x0045)),
    // Navigation. A Mac's Help key sits where a PC keyboard has Insert.
    kb(0x49, "Insert",             Some(0x72), Some(0xE052)), // kVK_Help
    kb(0x4A, "Home",               Some(0x73), Some(0xE047)), // kVK_Home
    kb(0x4B, "PageUp",             Some(0x74), Some(0xE049)), // kVK_PageUp
    kb(0x4C, "Delete",             Some(0x75), Some(0xE053)), // kVK_ForwardDelete
    kb(0x4D, "End",                Some(0x77), Some(0xE04F)), // kVK_End
    kb(0x4E, "PageDown",           Some(0x79), Some(0xE051)), // kVK_PageDown
    kb(0x4F, "ArrowRight",         Some(0x7C), Some(0xE04D)), // kVK_RightArrow
    kb(0x50, "ArrowLeft",          Some(0x7B), Some(0xE04B)), // kVK_LeftArrow
    kb(0x51, "ArrowDown",          Some(0x7D), Some(0xE050)), // kVK_DownArrow
    kb(0x52, "ArrowUp",            Some(0x7E), Some(0xE048)), // kVK_UpArrow
    // Keypad. A Mac keypad has Clear where a PC keypad has Num Lock; the W3C
    // spec names both "NumLock".
    kb(0x53, "NumLock",            Some(0x47), Some(0xE045)), // kVK_ANSI_KeypadClear
    kb(0x54, "NumpadDivide",       Some(0x4B), Some(0xE035)), // kVK_ANSI_KeypadDivide
    kb(0x55, "NumpadMultiply",     Some(0x43), Some(0x0037)), // kVK_ANSI_KeypadMultiply
    kb(0x56, "NumpadSubtract",     Some(0x4E), Some(0x004A)), // kVK_ANSI_KeypadMinus
    kb(0x57, "NumpadAdd",          Some(0x45), Some(0x004E)), // kVK_ANSI_KeypadPlus
    kb(0x58, "NumpadEnter",        Some(0x4C), Some(0xE01C)), // kVK_ANSI_KeypadEnter
    kb(0x59, "Numpad1",            Some(0x53), Some(0x004F)), // kVK_ANSI_Keypad1
    kb(0x5A, "Numpad2",            Some(0x54), Some(0x0050)), // kVK_ANSI_Keypad2
    kb(0x5B, "Numpad3",            Some(0x55), Some(0x0051)), // kVK_ANSI_Keypad3
    kb(0x5C, "Numpad4",            Some(0x56), Some(0x004B)), // kVK_ANSI_Keypad4
    kb(0x5D, "Numpad5",            Some(0x57), Some(0x004C)), // kVK_ANSI_Keypad5
    kb(0x5E, "Numpad6",            Some(0x58), Some(0x004D)), // kVK_ANSI_Keypad6
    kb(0x5F, "Numpad7",            Some(0x59), Some(0x0047)), // kVK_ANSI_Keypad7
    kb(0x60, "Numpad8",            Some(0x5B), Some(0x0048)), // kVK_ANSI_Keypad8
    kb(0x61, "Numpad9",            Some(0x5C), Some(0x0049)), // kVK_ANSI_Keypad9
    kb(0x62, "Numpad0",            Some(0x52), Some(0x0052)), // kVK_ANSI_Keypad0
    kb(0x63, "NumpadDecimal",      Some(0x41), Some(0x0053)), // kVK_ANSI_KeypadDecimal
    // The extra key next to Left Shift on ISO keyboards.
    kb(0x64, "IntlBackslash",      Some(0x0A), Some(0x0056)), // kVK_ISO_Section
    kb(0x65, "ContextMenu",        Some(0x6E), Some(0xE05D)), // kVK_ContextualMenu
    kb(0x67, "NumpadEqual",        Some(0x51), Some(0x0059)), // kVK_ANSI_KeypadEquals
    kb(0x68, "F13",                Some(0x69), Some(0x0064)), // kVK_F13
    kb(0x69, "F14",                Some(0x6B), Some(0x0065)), // kVK_F14
    kb(0x6A, "F15",                Some(0x71), Some(0x0066)), // kVK_F15
    kb(0x6B, "F16",                Some(0x6A), Some(0x0067)), // kVK_F16
    kb(0x6C, "F17",                Some(0x40), Some(0x0068)), // kVK_F17
    kb(0x6D, "F18",                Some(0x4F), Some(0x0069)), // kVK_F18
    kb(0x6E, "F19",                Some(0x50), Some(0x006A)), // kVK_F19
    kb(0x6F, "F20",                Some(0x5A), Some(0x006B)), // kVK_F20
    kb(0x70, "F21",                None,       Some(0x006C)),
    kb(0x71, "F22",                None,       Some(0x006D)),
    kb(0x72, "F23",                None,       Some(0x006E)),
    kb(0x73, "F24",                None,       Some(0x0076)),
    // Japanese (JIS) keys.
    kb(0x85, "NumpadComma",        Some(0x5F), Some(0x007E)), // kVK_JIS_KeypadComma
    kb(0x87, "IntlRo",             Some(0x5E), Some(0x0073)), // kVK_JIS_Underscore
    kb(0x88, "KanaMode",           Some(0x68), Some(0x0070)), // kVK_JIS_Kana
    kb(0x89, "IntlYen",            Some(0x5D), Some(0x007D)), // kVK_JIS_Yen
    kb(0x8A, "Convert",            None,       Some(0x0079)),
    kb(0x8B, "NonConvert",         None,       Some(0x007B)),
    // Modifiers. Meta is Command on a Mac keyboard and the Windows key on a PC
    // keyboard; Alt is Option on a Mac keyboard.
    kb(0xE0, "ControlLeft",        Some(0x3B), Some(0x001D)), // kVK_Control
    kb(0xE1, "ShiftLeft",          Some(0x38), Some(0x002A)), // kVK_Shift
    kb(0xE2, "AltLeft",            Some(0x3A), Some(0x0038)), // kVK_Option
    kb(0xE3, "MetaLeft",           Some(0x37), Some(0xE05B)), // kVK_Command
    kb(0xE4, "ControlRight",       Some(0x3E), Some(0xE01D)), // kVK_RightControl
    kb(0xE5, "ShiftRight",         Some(0x3C), Some(0x0036)), // kVK_RightShift
    kb(0xE6, "AltRight",           Some(0x3D), Some(0xE038)), // kVK_RightOption
    kb(0xE7, "MetaRight",          Some(0x36), Some(0xE05C)), // kVK_RightCommand
    // Consumer page (0x0C): media and volume keys.
    cons(0xB5, "MediaTrackNext",     None,       Some(0xE019)),
    cons(0xB6, "MediaTrackPrevious", None,       Some(0xE010)),
    cons(0xB7, "MediaStop",          None,       Some(0xE024)),
    cons(0xCD, "MediaPlayPause",     None,       Some(0xE022)),
    cons(0xE2, "AudioVolumeMute",    Some(0x4A), Some(0xE020)), // kVK_Mute
    cons(0xE9, "AudioVolumeUp",      Some(0x48), Some(0xE030)), // kVK_VolumeUp
    cons(0xEA, "AudioVolumeDown",    Some(0x49), Some(0xE02E)), // kVK_VolumeDown
];

/// The marker for an extended (E0-prefixed) scancode in the table's notation.
const EXTENDED: u16 = 0xE000;

fn by_code(code: KeyCode) -> Option<&'static Key> {
    let index = KEYS.binary_search_by_key(&code, |key| key.code).ok()?;
    KEYS.get(index)
}

/// The key with macOS virtual keycode `keycode` (a `kVK_*` value), or `None`
/// if Hop does not know it.
pub fn from_mac(keycode: u16) -> Option<KeyCode> {
    KEYS.iter()
        .find(|key| key.mac == Some(keycode))
        .map(|key| key.code)
}

/// The macOS virtual keycode for `code`, or `None` if a Mac has no such key.
pub fn to_mac(code: KeyCode) -> Option<u16> {
    by_code(code)?.mac
}

/// The key with Windows Set 1 scancode `scancode`, or `None` if Hop does not
/// know it.
///
/// `scancode` is the make code without any prefix, as in
/// `KBDLLHOOKSTRUCT::scanCode`, and `extended` says whether Windows flagged
/// the key as extended (an "E0" key, such as the arrows or Right Control).
pub fn from_windows_scancode(scancode: u16, extended: bool) -> Option<KeyCode> {
    // Anything wider than a byte is not a make code. Rejecting it here also
    // keeps a value like 0xE01D from posing as the table's own notation.
    if scancode > 0xFF {
        return None;
    }
    let wanted = if extended {
        EXTENDED | scancode
    } else {
        scancode
    };
    KEYS.iter()
        .find(|key| key.win == Some(wanted))
        .map(|key| key.code)
}

/// The Windows Set 1 scancode for `code` and whether it is an extended key,
/// or `None` if Windows has no such key.
///
/// The pair is the one Windows reports in keyboard messages and low-level
/// hooks, so it is the exact inverse of [`from_windows_scancode`]. For Num
/// Lock and Pause, Windows reports something other than what the keyboard
/// sends (Pause really sends "E1 1D 45", which a scancode cannot express), so
/// injecting those two may need their virtual-key codes instead.
pub fn to_windows_scancode(code: KeyCode) -> Option<(u16, bool)> {
    let win = by_code(code)?.win?;
    Some((win & 0x00FF, (win & 0xFF00) == EXTENDED))
}

/// The W3C UI Events `code` name of `code`, such as `"KeyA"`, `"MetaLeft"` or
/// `"Escape"`, or `None` if Hop does not know the key.
///
/// See <https://www.w3.org/TR/uievents-code/>.
pub fn name(code: KeyCode) -> Option<&'static str> {
    by_code(code).map(|key| key.name)
}

/// The key named `name` (a W3C UI Events `code` name, as returned by [`name`]),
/// or `None` if no key has that name.
///
/// Letter case is ignored, so a hand-edited config file may write `"escape"`
/// for `"Escape"`; no two names differ only in case.
pub fn from_name(name: &str) -> Option<KeyCode> {
    KEYS.iter()
        .find(|key| key.name.eq_ignore_ascii_case(name))
        .map(|key| key.code)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn table_is_sorted_by_code_without_repeats() {
        for pair in KEYS.windows(2) {
            assert!(
                pair[0].code < pair[1].code,
                "{} must come before {}",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn no_code_or_name_repeats_in_any_column() {
        let mut macs = BTreeSet::new();
        let mut wins = BTreeSet::new();
        let mut names = BTreeSet::new();
        for key in KEYS {
            if let Some(mac) = key.mac {
                assert!(macs.insert(mac), "macOS code {mac:#04x} repeats");
            }
            if let Some(win) = key.win {
                assert!(wins.insert(win), "Windows code {win:#06x} repeats");
            }
            assert!(
                names.insert(key.name.to_ascii_lowercase()),
                "name {} repeats",
                key.name
            );
        }
    }

    #[test]
    fn every_key_exists_on_some_os() {
        for key in KEYS {
            assert!(
                key.mac.is_some() || key.win.is_some(),
                "{} maps nowhere",
                key.name
            );
        }
    }

    #[test]
    fn codes_are_well_formed() {
        for key in KEYS {
            let page = key.code.page();
            assert!(
                page == KeyCode::KEYBOARD_PAGE || page == KeyCode::CONSUMER_PAGE,
                "{} is on page {page:#x}",
                key.name
            );
            if let Some(mac) = key.mac {
                // Every kVK_* constant fits in 7 bits.
                assert!(mac < 0x80, "{} has macOS code {mac:#x}", key.name);
            }
            if let Some(win) = key.win {
                let prefix = win & 0xFF00;
                assert!(
                    prefix == 0 || prefix == EXTENDED,
                    "{} has prefix {prefix:#x}",
                    key.name
                );
                // Set 1 make codes have the top bit clear; it marks a break.
                assert!(win & 0x00FF < 0x80, "{} is a break code", key.name);
            }
        }
    }

    #[test]
    fn every_row_round_trips_through_each_lookup() {
        for key in KEYS {
            assert_eq!(name(key.code), Some(key.name));
            assert_eq!(from_name(key.name), Some(key.code));
            if let Some(mac) = key.mac {
                assert_eq!(to_mac(key.code), Some(mac));
                assert_eq!(from_mac(mac), Some(key.code));
            }
            if let Some((scancode, extended)) = to_windows_scancode(key.code) {
                assert_eq!(from_windows_scancode(scancode, extended), Some(key.code));
            }
        }
    }

    /// Chromium's table, as packaged by the `keycode` crate, agrees with ours
    /// on every key we have. Chromium files the volume keys under the keyboard
    /// page, so those three are compared with Chromium's keyboard-page rows.
    #[test]
    fn matches_chromium() {
        for key in KEYS {
            let (page, usage) = match (key.code.page(), key.code.usage()) {
                (KeyCode::CONSUMER_PAGE, 0xE2) => (KeyCode::KEYBOARD_PAGE, 0x7F),
                (KeyCode::CONSUMER_PAGE, 0xE9) => (KeyCode::KEYBOARD_PAGE, 0x80),
                (KeyCode::CONSUMER_PAGE, 0xEA) => (KeyCode::KEYBOARD_PAGE, 0x81),
                other => other,
            };
            let Ok(chromium) = keycode::KeyMap::from_usb_code(page, usage) else {
                panic!("Chromium has no key {page:#x}:{usage:#x} ({})", key.name);
            };
            // Chromium writes "no key" as 0xFFFF for macOS and 0 for Windows.
            assert_eq!(key.mac.unwrap_or(0xFFFF), chromium.mac, "{}", key.name);
            assert_eq!(key.win.unwrap_or(0), chromium.win, "{}", key.name);
            let chromium_name = chromium.code.map(|code| code.to_string());
            assert_eq!(chromium_name.as_deref(), Some(key.name));
        }
    }

    #[test]
    fn every_known_macos_code_maps_back_to_itself() {
        for mac in 0..=u16::MAX {
            if let Some(code) = from_mac(mac) {
                assert_eq!(to_mac(code), Some(mac), "macOS code {mac:#x}");
            }
        }
    }

    #[test]
    fn every_known_windows_code_maps_back_to_itself() {
        for scancode in 0..=u16::MAX {
            for extended in [false, true] {
                if let Some(code) = from_windows_scancode(scancode, extended) {
                    assert_eq!(
                        to_windows_scancode(code),
                        Some((scancode, extended)),
                        "Windows code {scancode:#x} extended={extended}"
                    );
                }
            }
        }
    }

    #[test]
    fn windows_extended_flag_tells_keys_apart() {
        let numpad_enter = KeyCode::keyboard(0x58);
        let enter = KeyCode::keyboard(0x28);
        assert_eq!(from_windows_scancode(0x1C, false), Some(enter));
        assert_eq!(from_windows_scancode(0x1C, true), Some(numpad_enter));
        assert_eq!(
            from_windows_scancode(0x1D, false),
            Some(KeyCode::LEFT_CONTROL)
        );
        assert_eq!(
            from_windows_scancode(0x1D, true),
            Some(KeyCode::RIGHT_CONTROL)
        );
        // Num Lock is reported as extended and Pause is not.
        assert_eq!(
            from_windows_scancode(0x45, true),
            Some(KeyCode::keyboard(0x53))
        );
        assert_eq!(
            from_windows_scancode(0x45, false),
            Some(KeyCode::keyboard(0x48))
        );
    }

    #[test]
    fn windows_codes_outside_the_table_are_unknown() {
        // The table's own E0 notation is not accepted as a scancode.
        assert_eq!(from_windows_scancode(0xE01D, false), None);
        assert_eq!(from_windows_scancode(0xE01D, true), None);
        // Fake shifts, Break (Ctrl+Pause) and SysRq (Alt+Print Screen).
        assert_eq!(from_windows_scancode(0x2A, true), None);
        assert_eq!(from_windows_scancode(0x36, true), None);
        assert_eq!(from_windows_scancode(0x46, true), None);
        assert_eq!(from_windows_scancode(0x54, false), None);
        // Break codes and the prefix bytes themselves.
        assert_eq!(from_windows_scancode(0x9E, false), None);
        assert_eq!(from_windows_scancode(0xE0, false), None);
        assert_eq!(from_windows_scancode(0x00, false), None);
    }

    #[test]
    fn keys_on_one_os_only_map_to_none_on_the_other() {
        let windows_only = [
            0x46, // Print Screen
            0x47, // Scroll Lock
            0x48, // Pause
            0x70, 0x71, 0x72, 0x73, // F21 to F24
            0x8A, // Convert
            0x8B, // NonConvert
        ];
        for usage in windows_only {
            let code = KeyCode::keyboard(usage);
            assert_eq!(to_mac(code), None, "{:?}", name(code));
            assert!(to_windows_scancode(code).is_some(), "{:?}", name(code));
        }
        for usage in [0xB5, 0xB6, 0xB7, 0xCD] {
            let code = KeyCode::consumer(usage);
            assert_eq!(to_mac(code), None, "{:?}", name(code));
            assert!(to_windows_scancode(code).is_some(), "{:?}", name(code));
        }
        // Fn and the JIS Eisu key exist only on a Mac and are not forwarded.
        assert_eq!(from_mac(0x3F), None);
        assert_eq!(from_mac(0x66), None);
    }

    #[test]
    fn deliberately_missing_keys_map_to_none() {
        let missing = [
            KeyCode::keyboard(0x00), // no key
            KeyCode::keyboard(0x32), // ISO #~
            KeyCode::keyboard(0x66), // Power
            KeyCode::keyboard(0x7F), // keyboard-page Mute
            KeyCode::keyboard(0x80), // keyboard-page Volume Up
            KeyCode::keyboard(0x81), // keyboard-page Volume Down
            KeyCode::keyboard(0xE8), // past the modifiers
            KeyCode::consumer(0x00),
            KeyCode(0x0001_0082), // System Sleep, generic desktop page
            KeyCode(u32::MAX),
        ];
        for code in missing {
            assert_eq!(to_mac(code), None, "{code:?}");
            assert_eq!(to_windows_scancode(code), None, "{code:?}");
            assert_eq!(name(code), None, "{code:?}");
        }
    }

    #[test]
    fn modifier_names_follow_w3c() {
        let expected = [
            (KeyCode::LEFT_CONTROL, "ControlLeft"),
            (KeyCode::LEFT_SHIFT, "ShiftLeft"),
            (KeyCode::LEFT_ALT, "AltLeft"),
            (KeyCode::LEFT_META, "MetaLeft"),
            (KeyCode::RIGHT_CONTROL, "ControlRight"),
            (KeyCode::RIGHT_SHIFT, "ShiftRight"),
            (KeyCode::RIGHT_ALT, "AltRight"),
            (KeyCode::RIGHT_META, "MetaRight"),
            (KeyCode::ESCAPE, "Escape"),
            (KeyCode::CAPS_LOCK, "CapsLock"),
        ];
        for (code, expected_name) in expected {
            assert_eq!(name(code), Some(expected_name));
            assert_eq!(from_name(expected_name), Some(code));
        }
    }

    #[test]
    fn default_emergency_chord_names_resolve() {
        // Left Control + Left Alt/Option + Left Shift + Escape.
        let chord: Vec<_> = ["ControlLeft", "AltLeft", "ShiftLeft", "Escape"]
            .into_iter()
            .map(from_name)
            .collect();
        assert_eq!(
            chord,
            [
                Some(KeyCode::LEFT_CONTROL),
                Some(KeyCode::LEFT_ALT),
                Some(KeyCode::LEFT_SHIFT),
                Some(KeyCode::ESCAPE),
            ]
        );
    }

    #[test]
    fn from_name_ignores_case_but_nothing_else() {
        assert_eq!(from_name("escape"), Some(KeyCode::ESCAPE));
        assert_eq!(from_name("METALEFT"), Some(KeyCode::LEFT_META));
        for unknown in [
            "", " Escape", "Escape ", "Esc", "Key", "KeyAA", "Cmd", "Fn", "Power",
        ] {
            assert_eq!(from_name(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn mac_and_windows_codes_for_familiar_keys() {
        let a = KeyCode::keyboard(0x04);
        assert_eq!(to_mac(a), Some(0x00));
        assert_eq!(to_windows_scancode(a), Some((0x1E, false)));
        assert_eq!(to_mac(KeyCode::LEFT_META), Some(0x37));
        assert_eq!(to_windows_scancode(KeyCode::LEFT_META), Some((0x5B, true)));
        assert_eq!(to_mac(KeyCode::RIGHT_ALT), Some(0x3D));
        assert_eq!(to_windows_scancode(KeyCode::RIGHT_ALT), Some((0x38, true)));
        assert_eq!(from_mac(0x35), Some(KeyCode::ESCAPE));
        assert_eq!(from_mac(0x48), Some(KeyCode::consumer(0xE9)));
    }

    #[test]
    fn unknown_macos_codes_are_none() {
        // Unassigned values between the kVK_* constants, and values past them.
        for mac in [0x34, 0x42, 0x44, 0x46, 0x4D, 0x6C, 0x70, 0x7F, 0x80, 0xFFFF] {
            assert_eq!(from_mac(mac), None, "{mac:#x}");
        }
    }
}
