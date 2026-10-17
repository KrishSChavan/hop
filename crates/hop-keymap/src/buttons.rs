//! Held mouse buttons.

use std::collections::BTreeSet;

use hop_types::MouseButton;

/// The mouse buttons a session currently holds down, so that all of them can
/// be released when the session ends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ButtonTracker {
    held: BTreeSet<MouseButton>,
}

impl ButtonTracker {
    /// A tracker with no buttons held.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `button` as held. Returns `false` if it already was.
    pub fn press(&mut self, button: MouseButton) -> bool {
        self.held.insert(button)
    }

    /// Records `button` as released. Returns `false` if it was not held.
    pub fn release(&mut self, button: MouseButton) -> bool {
        self.held.remove(&button)
    }

    /// Every held button, which the caller should now release, and forgets
    /// them all.
    ///
    /// The order is fixed (left, right, middle, back, forward) so that tests
    /// and logs are repeatable.
    pub fn release_all(&mut self) -> Vec<MouseButton> {
        std::mem::take(&mut self.held).into_iter().collect()
    }

    /// Whether any button is held.
    pub fn any_pressed(&self) -> bool {
        !self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_nothing_held() {
        let mut buttons = ButtonTracker::new();
        assert!(!buttons.any_pressed());
        assert!(buttons.release_all().is_empty());
    }

    #[test]
    fn press_and_release_report_changes() {
        let mut buttons = ButtonTracker::new();
        assert!(buttons.press(MouseButton::Left));
        assert!(!buttons.press(MouseButton::Left), "already held");
        assert!(buttons.any_pressed());
        assert!(buttons.release(MouseButton::Left));
        assert!(!buttons.release(MouseButton::Left), "already released");
        assert!(!buttons.any_pressed());
    }

    #[test]
    fn releasing_a_button_that_was_never_pressed_is_harmless() {
        let mut buttons = ButtonTracker::new();
        assert!(!buttons.release(MouseButton::Back));
        assert!(!buttons.any_pressed());
    }

    #[test]
    fn release_all_returns_held_buttons_in_a_fixed_order_and_clears() {
        let mut buttons = ButtonTracker::new();
        for button in [
            MouseButton::Forward,
            MouseButton::Middle,
            MouseButton::Left,
            MouseButton::Right,
        ] {
            buttons.press(button);
        }
        buttons.release(MouseButton::Middle);
        assert_eq!(
            buttons.release_all(),
            [MouseButton::Left, MouseButton::Right, MouseButton::Forward]
        );
        assert!(!buttons.any_pressed());
        assert!(buttons.release_all().is_empty());
    }
}
