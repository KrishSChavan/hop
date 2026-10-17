//! The emergency chord: a key combination that always ends a session from
//! the controller, even if the controlled side has stopped answering.

use std::collections::BTreeSet;

use hop_types::{InputEvent, KeyCode};

/// A key combination: every key in `modifiers` held, then `key` pressed.
///
/// The press of `key` that completes the chord is never forwarded, so the
/// controlled device does not see a stray Escape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    /// Keys that must already be held when `key` goes down.
    pub modifiers: BTreeSet<KeyCode>,
    /// The key whose press completes the chord.
    pub key: KeyCode,
}

impl Chord {
    /// A chord of `modifiers` plus `key`.
    pub fn new(modifiers: impl IntoIterator<Item = KeyCode>, key: KeyCode) -> Self {
        Self {
            modifiers: modifiers.into_iter().collect(),
            key,
        }
    }

    /// Whether `event` completes the chord, given the keys physically held
    /// (including the key `event` presses, if the caller already recorded it).
    pub fn is_completed_by(&self, event: &InputEvent, held: &BTreeSet<KeyCode>) -> bool {
        match event {
            InputEvent::Key {
                code,
                pressed: true,
                ..
            } => *code == self.key && self.modifiers.iter().all(|m| held.contains(m)),
            _ => false,
        }
    }
}

impl Default for Chord {
    /// Left Control + Left Alt/Option + Left Shift + Escape.
    fn default() -> Self {
        Self::new(
            [
                KeyCode::LEFT_CONTROL,
                KeyCode::LEFT_ALT,
                KeyCode::LEFT_SHIFT,
            ],
            KeyCode::ESCAPE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> InputEvent {
        InputEvent::Key {
            code,
            pressed: true,
            repeat: false,
        }
    }

    fn held(keys: &[KeyCode]) -> BTreeSet<KeyCode> {
        keys.iter().copied().collect()
    }

    #[test]
    fn default_chord_needs_all_three_modifiers() {
        let chord = Chord::default();
        let all = [
            KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::LEFT_SHIFT,
        ];
        assert!(chord.is_completed_by(&press(KeyCode::ESCAPE), &held(&all)));
        assert!(!chord.is_completed_by(&press(KeyCode::ESCAPE), &held(&all[..2])));
        // The right-hand modifiers are different keys and do not count.
        let right = [
            KeyCode::RIGHT_CONTROL,
            KeyCode::RIGHT_ALT,
            KeyCode::RIGHT_SHIFT,
        ];
        assert!(!chord.is_completed_by(&press(KeyCode::ESCAPE), &held(&right)));
    }

    #[test]
    fn only_a_press_of_the_key_completes_it() {
        let chord = Chord::default();
        let all = held(&[
            KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::LEFT_SHIFT,
        ]);
        let release = InputEvent::Key {
            code: KeyCode::ESCAPE,
            pressed: false,
            repeat: false,
        };
        assert!(!chord.is_completed_by(&release, &all));
        assert!(!chord.is_completed_by(&press(KeyCode::keyboard(0x04)), &all));
        let click = InputEvent::Button {
            button: hop_types::MouseButton::Left,
            pressed: true,
        };
        assert!(!chord.is_completed_by(&click, &all));
    }
}
