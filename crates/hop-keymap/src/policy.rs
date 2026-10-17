//! The Cmd ↔ Ctrl swap.

use hop_types::{KeyCode, Os};

/// How modifier keys change on their way from one OS to another.
///
/// macOS puts shortcuts on Command (Meta), while Windows and Linux put them
/// on Control. With the swap on, Cmd+C typed on a Mac arrives on Windows as
/// Ctrl+C, and Ctrl+C typed on a PC arrives on a Mac as Cmd+C. The Mac's
/// Control key then acts as the Windows key, and the other way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModifierPolicy {
    /// Swap Meta (Command or the Windows key) with Control, left with left and
    /// right with right, when exactly one of the two devices is a Mac. On by
    /// default.
    pub swap_cmd_ctrl: bool,
}

impl Default for ModifierPolicy {
    fn default() -> Self {
        Self {
            swap_cmd_ctrl: true,
        }
    }
}

impl ModifierPolicy {
    /// The key to inject on `target` for `code` typed on a `source` keyboard.
    ///
    /// Only the four Meta and Control keys ever change, and only when the swap
    /// is on and exactly one of `source` and `target` is macOS. Between two
    /// Macs, or between Windows and Linux, every key passes through unchanged.
    pub fn map(&self, code: KeyCode, source: Os, target: Os) -> KeyCode {
        let crosses_mac_boundary = (source == Os::MacOs) != (target == Os::MacOs);
        if !(self.swap_cmd_ctrl && crosses_mac_boundary) {
            return code;
        }
        match code {
            KeyCode::LEFT_META => KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_CONTROL => KeyCode::LEFT_META,
            KeyCode::RIGHT_META => KeyCode::RIGHT_CONTROL,
            KeyCode::RIGHT_CONTROL => KeyCode::RIGHT_META,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_OS: [Os; 3] = [Os::MacOs, Os::Windows, Os::Linux];

    const SWAPPED: [(KeyCode, KeyCode); 4] = [
        (KeyCode::LEFT_META, KeyCode::LEFT_CONTROL),
        (KeyCode::LEFT_CONTROL, KeyCode::LEFT_META),
        (KeyCode::RIGHT_META, KeyCode::RIGHT_CONTROL),
        (KeyCode::RIGHT_CONTROL, KeyCode::RIGHT_META),
    ];

    /// Keys the swap must never touch, including the other modifiers.
    const UNCHANGED: [KeyCode; 8] = [
        KeyCode::LEFT_ALT,
        KeyCode::RIGHT_ALT,
        KeyCode::LEFT_SHIFT,
        KeyCode::RIGHT_SHIFT,
        KeyCode::CAPS_LOCK,
        KeyCode::ESCAPE,
        KeyCode::keyboard(0x06), // C
        KeyCode::consumer(0xE9), // Volume Up
    ];

    #[test]
    fn swap_is_on_by_default() {
        assert!(ModifierPolicy::default().swap_cmd_ctrl);
    }

    #[test]
    fn swap_matrix_for_every_os_pair() {
        for swap_cmd_ctrl in [false, true] {
            let policy = ModifierPolicy { swap_cmd_ctrl };
            for source in ALL_OS {
                for target in ALL_OS {
                    let one_side_is_mac = (source == Os::MacOs) != (target == Os::MacOs);
                    let expect_swap = swap_cmd_ctrl && one_side_is_mac;
                    for (from, swapped) in SWAPPED {
                        let expected = if expect_swap { swapped } else { from };
                        assert_eq!(
                            policy.map(from, source, target),
                            expected,
                            "{from:?} from {source:?} to {target:?}, swap={swap_cmd_ctrl}"
                        );
                    }
                    for code in UNCHANGED {
                        assert_eq!(policy.map(code, source, target), code);
                    }
                }
            }
        }
    }

    #[test]
    fn the_architecture_examples_hold() {
        let policy = ModifierPolicy::default();
        // Cmd+C on the Mac keyboard arrives on Windows as Ctrl+C.
        assert_eq!(
            policy.map(KeyCode::LEFT_META, Os::MacOs, Os::Windows),
            KeyCode::LEFT_CONTROL
        );
        // Ctrl+C on the PC keyboard arrives on the Mac as Cmd+C.
        assert_eq!(
            policy.map(KeyCode::LEFT_CONTROL, Os::Windows, Os::MacOs),
            KeyCode::LEFT_META
        );
        // The Mac's Control key acts as the Windows key.
        assert_eq!(
            policy.map(KeyCode::LEFT_CONTROL, Os::MacOs, Os::Windows),
            KeyCode::LEFT_META
        );
        // The swap keeps sides: right Command becomes right Control.
        assert_eq!(
            policy.map(KeyCode::RIGHT_META, Os::MacOs, Os::Windows),
            KeyCode::RIGHT_CONTROL
        );
    }

    #[test]
    fn mapping_twice_gives_back_the_original_key() {
        // Going there and back again (Mac to PC, then PC to Mac) is lossless.
        let policy = ModifierPolicy::default();
        for source in ALL_OS {
            for target in ALL_OS {
                for usage in 0..=0xFF {
                    let code = KeyCode::keyboard(usage);
                    let there = policy.map(code, source, target);
                    assert_eq!(policy.map(there, target, source), code);
                }
            }
        }
    }
}
