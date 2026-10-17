//! Per-session key translation with a record of what was injected.

use std::collections::{BTreeMap, BTreeSet};

use hop_types::{KeyCode, Os};

use crate::ModifierPolicy;

/// Translates the keys of one incoming session and remembers which key it
/// injected for each key that is still down.
///
/// The receiver keeps one `Translator` per session. Recording the injected
/// key at key-down means the key-up always releases that same key, even if
/// the policy changed while the key was held: without this, turning the swap
/// off mid-press would release Command where Control went down, leaving
/// Control stuck on the target.
#[derive(Clone, Debug)]
pub struct Translator {
    policy: ModifierPolicy,
    source: Os,
    target: Os,
    /// Source key → the key injected for it when it went down.
    held: BTreeMap<KeyCode, KeyCode>,
}

impl Translator {
    /// A translator for keys typed on a `source` keyboard and injected on
    /// `target`, with nothing held.
    pub fn new(policy: ModifierPolicy, source: Os, target: Os) -> Self {
        Self {
            policy,
            source,
            target,
            held: BTreeMap::new(),
        }
    }

    /// The key to inject for a key-down of `code`, which is recorded as held.
    ///
    /// If `code` is already held (a duplicate key-down), this returns the key
    /// recorded the first time, so the eventual key-up still matches.
    pub fn press(&mut self, code: KeyCode) -> KeyCode {
        let mapped = self.map(code);
        *self.held.entry(code).or_insert(mapped)
    }

    /// The key to inject for an auto-repeat key-down of `code`.
    ///
    /// This behaves exactly like [`press`](Self::press). A repeat for a key
    /// whose first key-down was never seen (it went down before the session
    /// began) still makes the target see the key go down, so it is recorded
    /// too and [`release_all`](Self::release_all) will release it.
    pub fn repeat(&mut self, code: KeyCode) -> KeyCode {
        self.press(code)
    }

    /// The key to inject for a key-up of `code`: the one recorded when it went
    /// down, which is then forgotten.
    ///
    /// A key-up for a key that was never pressed is mapped with the current
    /// policy, so it still reaches the target as the key it most likely is.
    pub fn release(&mut self, code: KeyCode) -> KeyCode {
        match self.held.remove(&code) {
            Some(mapped) => mapped,
            None => self.map(code),
        }
    }

    /// Replaces the policy for keys pressed from now on. Keys already held are
    /// still released as recorded.
    pub fn set_policy(&mut self, policy: ModifierPolicy) {
        self.policy = policy;
    }

    /// The policy in use for new key-downs.
    pub fn policy(&self) -> ModifierPolicy {
        self.policy
    }

    /// Every injected key that is still down, which the caller should now
    /// release, and forgets them all. Use it when the session ends.
    ///
    /// Each injected key appears once, even if two source keys map to it.
    /// Ordinary keys come first and modifiers last, each group in [`KeyCode`]
    /// order. That is the order a person lets go of a shortcut (the C of
    /// Ctrl+C before the Ctrl), so the target never sees a moment in which a
    /// letter is still held without the modifiers it went down with. An app
    /// that read the keyboard in that moment could take it for a plain
    /// keystroke.
    pub fn release_all(&mut self) -> Vec<KeyCode> {
        let injected: BTreeSet<KeyCode> = std::mem::take(&mut self.held).into_values().collect();
        let (modifiers, others): (Vec<KeyCode>, Vec<KeyCode>) =
            injected.into_iter().partition(|code| code.is_modifier());
        others.into_iter().chain(modifiers).collect()
    }

    /// The keys held now, as `(source key, injected key)` pairs in source-key
    /// order.
    pub fn pressed(&self) -> impl Iterator<Item = (KeyCode, KeyCode)> + '_ {
        self.held
            .iter()
            .map(|(&source_key, &injected)| (source_key, injected))
    }

    fn map(&self, code: KeyCode) -> KeyCode {
        self.policy.map(code, self.source, self.target)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SWAP: ModifierPolicy = ModifierPolicy {
        swap_cmd_ctrl: true,
    };
    const NO_SWAP: ModifierPolicy = ModifierPolicy {
        swap_cmd_ctrl: false,
    };
    const A: KeyCode = KeyCode::keyboard(0x04);
    const C: KeyCode = KeyCode::keyboard(0x06);

    fn mac_to_windows() -> Translator {
        Translator::new(SWAP, Os::MacOs, Os::Windows)
    }

    #[test]
    fn press_and_release_apply_the_swap() {
        let mut keys = mac_to_windows();
        assert_eq!(keys.press(KeyCode::LEFT_META), KeyCode::LEFT_CONTROL);
        assert_eq!(keys.press(C), C);
        assert_eq!(keys.release(C), C);
        assert_eq!(keys.release(KeyCode::LEFT_META), KeyCode::LEFT_CONTROL);
        assert_eq!(keys.pressed().count(), 0);
    }

    #[test]
    fn key_up_matches_key_down_when_the_policy_flips_mid_press() {
        let mut keys = mac_to_windows();
        assert_eq!(keys.press(KeyCode::LEFT_META), KeyCode::LEFT_CONTROL);
        keys.set_policy(NO_SWAP);
        assert_eq!(keys.policy(), NO_SWAP);
        // A repeat of the held key keeps the recorded mapping too.
        assert_eq!(keys.repeat(KeyCode::LEFT_META), KeyCode::LEFT_CONTROL);
        assert_eq!(keys.release(KeyCode::LEFT_META), KeyCode::LEFT_CONTROL);
        // The next press uses the new policy.
        assert_eq!(keys.press(KeyCode::LEFT_META), KeyCode::LEFT_META);
        keys.set_policy(SWAP);
        assert_eq!(keys.release(KeyCode::LEFT_META), KeyCode::LEFT_META);
    }

    #[test]
    fn release_without_press_maps_with_the_current_policy() {
        let mut keys = mac_to_windows();
        assert_eq!(keys.release(KeyCode::RIGHT_META), KeyCode::RIGHT_CONTROL);
        keys.set_policy(NO_SWAP);
        assert_eq!(keys.release(KeyCode::RIGHT_META), KeyCode::RIGHT_META);
        assert_eq!(keys.pressed().count(), 0);
    }

    #[test]
    fn duplicate_press_keeps_the_first_mapping() {
        let mut keys = mac_to_windows();
        assert_eq!(keys.press(KeyCode::LEFT_CONTROL), KeyCode::LEFT_META);
        keys.set_policy(NO_SWAP);
        assert_eq!(keys.press(KeyCode::LEFT_CONTROL), KeyCode::LEFT_META);
        assert_eq!(keys.release(KeyCode::LEFT_CONTROL), KeyCode::LEFT_META);
    }

    #[test]
    fn repeat_of_a_key_pressed_before_the_session_is_recorded() {
        let mut keys = mac_to_windows();
        assert_eq!(keys.repeat(A), A);
        assert_eq!(keys.pressed().collect::<Vec<_>>(), [(A, A)]);
        assert_eq!(keys.release_all(), [A]);
    }

    #[test]
    fn same_os_and_windows_to_linux_pass_keys_through() {
        for (source, target) in [
            (Os::MacOs, Os::MacOs),
            (Os::Windows, Os::Windows),
            (Os::Windows, Os::Linux),
            (Os::Linux, Os::Windows),
        ] {
            let mut keys = Translator::new(SWAP, source, target);
            assert_eq!(keys.press(KeyCode::LEFT_META), KeyCode::LEFT_META);
            assert_eq!(keys.release(KeyCode::LEFT_META), KeyCode::LEFT_META);
        }
    }

    #[test]
    fn pressed_lists_source_and_injected_keys_in_source_order() {
        let mut keys = mac_to_windows();
        keys.press(KeyCode::LEFT_META);
        keys.press(C);
        assert_eq!(
            keys.pressed().collect::<Vec<_>>(),
            [(C, C), (KeyCode::LEFT_META, KeyCode::LEFT_CONTROL)]
        );
    }

    #[test]
    fn release_all_puts_modifiers_last_and_clears() {
        let mut keys = mac_to_windows();
        keys.press(KeyCode::LEFT_SHIFT);
        keys.press(KeyCode::LEFT_META);
        keys.press(C);
        keys.press(A);
        keys.press(KeyCode::consumer(0xE9));
        assert_eq!(
            keys.release_all(),
            [
                A,
                C,
                KeyCode::consumer(0xE9),
                KeyCode::LEFT_CONTROL,
                KeyCode::LEFT_SHIFT,
            ]
        );
        assert_eq!(keys.pressed().count(), 0);
        assert!(keys.release_all().is_empty());
    }

    #[test]
    fn release_all_lists_a_shared_injected_key_once() {
        // Command went down with the swap on (injected as Control), then
        // Control went down with it off (also injected as Control).
        let mut keys = mac_to_windows();
        keys.press(KeyCode::LEFT_META);
        keys.set_policy(NO_SWAP);
        keys.press(KeyCode::LEFT_CONTROL);
        assert_eq!(keys.pressed().count(), 2);
        assert_eq!(keys.release_all(), [KeyCode::LEFT_CONTROL]);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Press(KeyCode),
        Repeat(KeyCode),
        Release(KeyCode),
        SetSwap(bool),
    }

    fn any_os() -> impl Strategy<Value = Os> {
        prop_oneof![Just(Os::MacOs), Just(Os::Windows), Just(Os::Linux)]
    }

    fn any_op() -> impl Strategy<Value = Op> {
        // A few letters plus every modifier, so the swap is exercised often.
        let key = prop_oneof![
            (0x04u16..0x08).prop_map(KeyCode::keyboard),
            (0xE0u16..=0xE7).prop_map(KeyCode::keyboard),
        ];
        prop_oneof![
            key.clone().prop_map(Op::Press),
            key.clone().prop_map(Op::Repeat),
            key.prop_map(Op::Release),
            any::<bool>().prop_map(Op::SetSwap),
        ]
    }

    proptest! {
        /// Whatever happens during a session, every key the target is left
        /// holding is released by `release_all`, and each key-up releases the
        /// key its key-down injected.
        #[test]
        fn no_key_is_left_stuck_on_the_target(
            source in any_os(),
            target in any_os(),
            ops in proptest::collection::vec(any_op(), 0..64),
        ) {
            let mut keys = Translator::new(ModifierPolicy::default(), source, target);
            // What the target OS believes is down, and what each source key injected.
            let mut target_down = BTreeSet::new();
            let mut injected_for = BTreeMap::new();
            for op in ops {
                match op {
                    Op::Press(code) | Op::Repeat(code) => {
                        let injected = if matches!(op, Op::Press(_)) {
                            keys.press(code)
                        } else {
                            keys.repeat(code)
                        };
                        let first = *injected_for.entry(code).or_insert(injected);
                        prop_assert_eq!(injected, first);
                        target_down.insert(injected);
                    }
                    Op::Release(code) => {
                        let injected = keys.release(code);
                        if let Some(expected) = injected_for.remove(&code) {
                            prop_assert_eq!(injected, expected);
                        }
                        target_down.remove(&injected);
                    }
                    Op::SetSwap(swap_cmd_ctrl) => keys.set_policy(ModifierPolicy { swap_cmd_ctrl }),
                }
            }
            let released = keys.release_all();
            let released_set: BTreeSet<KeyCode> = released.iter().copied().collect();
            prop_assert_eq!(released_set.len(), released.len(), "no duplicates");
            // Exactly the keys still recorded, which covers all the target holds.
            let still_recorded: BTreeSet<KeyCode> = injected_for.into_values().collect();
            prop_assert_eq!(&released_set, &still_recorded);
            prop_assert!(target_down.is_subset(&released_set));
            let first_modifier = released.iter().position(|code| code.is_modifier());
            if let Some(first_modifier) = first_modifier {
                prop_assert!(released[first_modifier..].iter().all(|code| code.is_modifier()));
            }
            prop_assert_eq!(keys.pressed().count(), 0);
        }
    }
}
