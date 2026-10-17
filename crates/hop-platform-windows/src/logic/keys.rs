//! Key rules: which scancode a low-level keyboard hook event stands for,
//! which keys are held (to spot auto-repeat), and how to inject a key.

use hop_types::KeyCode;

/// `VK_PACKET`: a "key" that carries a Unicode character from `SendInput`
/// (on-screen keyboards, some remote tools). Its scancode field holds the
/// character, not a scancode, so it is never a physical key.
pub const VK_PACKET: u32 = 0xE7;

/// `VK_NUMLOCK`.
pub const VK_NUMLOCK: u16 = 0x90;

/// `VK_PAUSE`.
pub const VK_PAUSE: u16 = 0x13;

/// The virtual key of the A key.
pub const VK_A: usize = 0x41;

/// Whether a key message is Ctrl+A, the select-all shortcut, for the
/// self-test window's text box: `key_down` (a `WM_KEYDOWN`), the virtual key
/// `vk` (its `wParam`), and whether Control and Alt are down.
///
/// A classic Win32 multi-line edit control has no select-all shortcut, and
/// the self-test selects all and types over it, so the test window handles
/// Ctrl+A itself. Ctrl+Alt+A is left alone: on some layouts that is AltGr+A,
/// which types a character.
pub fn is_select_all(key_down: bool, vk: usize, ctrl: bool, alt: bool) -> bool {
    key_down && vk == VK_A && ctrl && !alt
}

/// The Set 1 scancode and extended flag a low-level keyboard hook event
/// stands for, or `None` if it does not stand for a physical key.
///
/// `scan_code`, `extended` and `vk` are the hook's `scanCode`,
/// `LLKHF_EXTENDED` and `vkCode`. Some injected events carry no scancode
/// (`scan_code == 0`, only a virtual key); for those, `map_vk` is called with
/// the virtual key and must return `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX)`,
/// which puts 0xE0 (or 0xE1) in the high byte for extended keys.
pub fn hook_scancode(
    scan_code: u32,
    extended: bool,
    vk: u32,
    map_vk: impl FnOnce(u32) -> u32,
) -> Option<(u16, bool)> {
    if vk == VK_PACKET {
        return None;
    }
    if scan_code != 0 {
        // Make codes fit in a byte; anything wider is not one, and
        // `from_windows_scancode` rejects it.
        let scan_code = u16::try_from(scan_code).ok()?;
        return Some((scan_code, extended));
    }
    if vk == 0 {
        return None;
    }
    decode_vsc_ex(map_vk(vk))
}

/// Splits a `MAPVK_VK_TO_VSC_EX` result into scancode and extended flag.
///
/// The high byte is 0xE0 for extended keys and 0xE1 for Pause (which really
/// sends "E1 1D 45"). Low-level hooks report Pause as plain 0x45, so that is
/// what an `E1 1D` result becomes, matching `hop_keymap`'s table. 0 means the
/// virtual key has no scancode.
pub fn decode_vsc_ex(value: u32) -> Option<(u16, bool)> {
    let low = u16::try_from(value & 0xFF).ok()?;
    match value >> 8 {
        0x00 if low != 0 => Some((low, false)),
        0xE0 if low != 0 => Some((low, true)),
        0xE1 if low == 0x1D => Some((0x45, false)),
        _ => None,
    }
}

/// The keys held down, by scancode and extended flag, so that a key-down for
/// a key that is already down is recognized as an auto-repeat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldKeys {
    /// Bit `scancode | extended << 8`.
    bits: [u64; 8],
}

impl HeldKeys {
    fn bit(scan_code: u16, extended: bool) -> Option<(usize, u64)> {
        if scan_code > 0xFF {
            return None;
        }
        let index = usize::from(scan_code) | (usize::from(extended) << 8);
        Some((index / 64, 1 << (index % 64)))
    }

    /// Records a key-down. Returns `true` if the key was already down, which
    /// makes this key-down an auto-repeat.
    pub fn press(&mut self, scan_code: u16, extended: bool) -> bool {
        let Some((word, mask)) = Self::bit(scan_code, extended) else {
            return false;
        };
        let repeat = self.bits[word] & mask != 0;
        self.bits[word] |= mask;
        repeat
    }

    /// Records a key-up.
    pub fn release(&mut self, scan_code: u16, extended: bool) {
        if let Some((word, mask)) = Self::bit(scan_code, extended) {
            self.bits[word] &= !mask;
        }
    }
}

/// How to inject one key with `SendInput`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyStroke {
    /// The virtual key, or 0 when the scancode alone says which key.
    pub vk: u16,
    /// The Set 1 scancode, without the E0 prefix.
    pub scan: u16,
    /// Send `KEYEVENTF_EXTENDEDKEY` (an E0 key).
    pub extended: bool,
    /// Send `KEYEVENTF_SCANCODE`: Windows works out the virtual key from the
    /// scancode, as for a physical key.
    pub by_scancode: bool,
}

/// How to inject `code`, or `None` if Windows has no such key (the key is
/// then skipped).
///
/// Keys go in as scancodes (`KEYEVENTF_SCANCODE`), so the PC's own keyboard
/// layout gives them meaning, as for its own keyboard. Num Lock and Pause are
/// the exception: Windows reports them as extended 0x45 and plain 0x45, but
/// as injected scancodes both would read as Num Lock. They go in as virtual
/// keys instead, with the scancode Windows itself reports for them.
pub fn stroke_for(code: KeyCode) -> Option<KeyStroke> {
    const NUM_LOCK: KeyCode = KeyCode::keyboard(0x53);
    const PAUSE: KeyCode = KeyCode::keyboard(0x48);
    if code == NUM_LOCK {
        return Some(KeyStroke {
            vk: VK_NUMLOCK,
            scan: 0x45,
            extended: true,
            by_scancode: false,
        });
    }
    if code == PAUSE {
        return Some(KeyStroke {
            vk: VK_PAUSE,
            scan: 0x45,
            extended: false,
            by_scancode: false,
        });
    }
    let (scan, extended) = hop_keymap::to_windows_scancode(code)?;
    Some(KeyStroke {
        vk: 0,
        scan,
        extended,
        by_scancode: true,
    })
}

#[cfg(test)]
mod tests {
    use hop_keymap::{from_name, from_windows_scancode};

    use super::*;

    fn never_called(_: u32) -> u32 {
        panic!("MapVirtualKeyW must not be consulted when the hook has a scancode")
    }

    #[test]
    fn the_hooks_scancode_wins_when_present() {
        // Right Control: scancode 0x1D with LLKHF_EXTENDED.
        assert_eq!(
            hook_scancode(0x1D, true, 0xA3, never_called),
            Some((0x1D, true))
        );
        assert_eq!(
            hook_scancode(0x1E, false, 0x41, never_called),
            Some((0x1E, false))
        );
        // Not a make code: passed on, and the key table rejects it.
        assert_eq!(
            hook_scancode(0x1E0, false, 0x41, never_called),
            Some((0x1E0, false))
        );
        assert_eq!(from_windows_scancode(0x1E0, false), None);
        assert_eq!(hook_scancode(0x1_0000, false, 0x41, never_called), None);
    }

    #[test]
    fn a_missing_scancode_falls_back_to_the_virtual_key() {
        // VK_RCONTROL maps to E0 1D.
        let mapped = hook_scancode(0, false, 0xA3, |vk| {
            assert_eq!(vk, 0xA3);
            0xE01D
        });
        assert_eq!(mapped, Some((0x1D, true)));
        assert_eq!(hook_scancode(0, false, 0x41, |_| 0x1E), Some((0x1E, false)));
        // Pause maps to E1 1D; hooks and the key table call it plain 0x45.
        let pause = hook_scancode(0, false, u32::from(VK_PAUSE), |_| 0xE11D);
        assert_eq!(pause, Some((0x45, false)));
        assert_eq!(from_windows_scancode(0x45, false), from_name("Pause"));
        // No scancode for this virtual key, or no virtual key at all.
        assert_eq!(hook_scancode(0, false, 0xFF, |_| 0), None);
        assert_eq!(hook_scancode(0, false, 0, never_called), None);
    }

    #[test]
    fn unicode_packets_are_not_keys() {
        assert_eq!(hook_scancode(0x61, false, VK_PACKET, never_called), None);
        assert_eq!(hook_scancode(0, false, VK_PACKET, never_called), None);
    }

    #[test]
    fn vsc_ex_values_decode() {
        assert_eq!(decode_vsc_ex(0x0000), None);
        assert_eq!(decode_vsc_ex(0x002A), Some((0x2A, false)));
        assert_eq!(decode_vsc_ex(0xE05B), Some((0x5B, true)));
        assert_eq!(decode_vsc_ex(0xE000), None);
        assert_eq!(decode_vsc_ex(0xE11D), Some((0x45, false)));
        assert_eq!(decode_vsc_ex(0xE145), None);
        assert_eq!(decode_vsc_ex(0x1234), None);
    }

    #[test]
    fn a_second_key_down_is_a_repeat_until_the_key_up() {
        let mut held = HeldKeys::default();
        assert!(!held.press(0x1E, false));
        assert!(held.press(0x1E, false));
        // The extended twin is a different key (Enter vs. Numpad Enter).
        assert!(!held.press(0x1C, false));
        assert!(!held.press(0x1C, true));
        held.release(0x1E, false);
        assert!(!held.press(0x1E, false));
        // Out-of-range codes are never "held".
        assert!(!held.press(0x100, false));
        assert!(!held.press(0x100, false));
        held.release(0x100, false);
        held.release(0x1C, false);
        held.release(0x1C, true);
        held.release(0x1E, false);
        assert_eq!(held, HeldKeys::default());
        assert!(!held.press(0xFF, true));
        assert!(held.press(0xFF, true));
    }

    #[test]
    fn keys_inject_as_scancodes_except_num_lock_and_pause() {
        let key = |name| from_name(name).unwrap_or_else(|| panic!("{name} is a known key"));
        assert_eq!(
            stroke_for(key("KeyA")),
            Some(KeyStroke {
                vk: 0,
                scan: 0x1E,
                extended: false,
                by_scancode: true
            })
        );
        assert_eq!(
            stroke_for(key("MetaLeft")),
            Some(KeyStroke {
                vk: 0,
                scan: 0x5B,
                extended: true,
                by_scancode: true
            })
        );
        assert_eq!(
            stroke_for(key("NumLock")),
            Some(KeyStroke {
                vk: VK_NUMLOCK,
                scan: 0x45,
                extended: true,
                by_scancode: false
            })
        );
        assert_eq!(
            stroke_for(key("Pause")),
            Some(KeyStroke {
                vk: VK_PAUSE,
                scan: 0x45,
                extended: false,
                by_scancode: false
            })
        );
        // Keys a PC lacks are skipped.
        assert_eq!(stroke_for(KeyCode::keyboard(0x7F)), None);
    }

    #[test]
    fn only_ctrl_a_selects_all_in_the_test_window() {
        assert!(is_select_all(true, VK_A, true, false));
        // Plain A types a letter; a key-up does nothing; AltGr+A (Ctrl+Alt)
        // types a character on some layouts; other letters are not it.
        assert!(!is_select_all(true, VK_A, false, false));
        assert!(!is_select_all(false, VK_A, true, false));
        assert!(!is_select_all(true, VK_A, true, true));
        assert!(!is_select_all(true, 0x42, true, false));
    }

    #[test]
    fn every_injected_stroke_reads_back_as_the_same_key() {
        // What the hook reports for Hop's own injected keys must map back to
        // the key that was injected.
        for usage in 0..=0xFFFF_u16 {
            for code in [KeyCode::keyboard(usage), KeyCode::consumer(usage)] {
                if let Some(stroke) = stroke_for(code) {
                    let read_back = hook_scancode(
                        u32::from(stroke.scan),
                        stroke.extended,
                        u32::from(stroke.vk),
                        never_called,
                    )
                    .and_then(|(scan, extended)| from_windows_scancode(scan, extended));
                    assert_eq!(read_back, Some(code), "{code:?}");
                }
            }
        }
    }
}
