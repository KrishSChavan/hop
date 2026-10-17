//! The watchdog's decisions: when the suppression lease has lapsed, and when
//! Windows has probably removed the hooks.
//!
//! Windows silently unhooks a low-level hook whose callback takes too long
//! (`LowLevelHooksTimeout`), and nothing tells the program. The only sign is
//! that input keeps happening while the hooks stop being called.
//!
//! Times here are `GetTickCount` milliseconds, the clock `GetLastInputInfo`
//! uses. It wraps every 49.7 days, so ages are computed with wrapping
//! subtraction.

use std::time::Instant;

use hop_platform::SuppressLease;

/// How often the watchdog runs, in milliseconds.
pub const TICK_MS: u32 = 500;

/// Input counts as recent for this long, in milliseconds.
pub const RECENT_INPUT_MS: u32 = 2_000;

/// Hooks silent for longer than this while input is recent are presumed
/// removed, in milliseconds.
pub const HOOK_SILENCE_MS: u32 = 2_000;

/// Raw mouse input counts as recent for this long, in milliseconds. Raw
/// Input keeps arriving when the mouse hook is gone, which makes a removed
/// mouse hook easy to spot.
pub const RECENT_RAW_MS: u32 = 1_000;

/// How old `then` is at `now`, both `GetTickCount` values. A `then` that is
/// slightly in the future (the two were read in the other order) comes out
/// as a huge age, which every check below treats as "long ago".
pub fn age(now: u32, then: u32) -> u32 {
    now.wrapping_sub(then)
}

/// When the hooks were last called: the more recent of the two.
pub fn latest_hook(now: u32, mouse: u32, keyboard: u32) -> u32 {
    if age(now, mouse) <= age(now, keyboard) {
        mouse
    } else {
        keyboard
    }
}

/// What the watchdog knows when it checks the hooks, all `GetTickCount`
/// milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookHealth {
    /// Now.
    pub now: u32,
    /// When the hooks were (re)installed.
    pub installed: u32,
    /// The last call of the mouse hook (or `installed`, if none yet).
    pub mouse_hook: u32,
    /// The last call of the keyboard hook (or `installed`, if none yet).
    pub keyboard_hook: u32,
    /// `GetLastInputInfo`: the last input event the system saw, if known.
    pub last_input: Option<u32>,
    /// The last raw mouse motion, if any.
    pub last_raw: Option<u32>,
}

impl HookHealth {
    /// Whether the hooks look removed and should be installed again:
    /// - the system saw input recently but neither hook has been called for
    ///   over [`HOOK_SILENCE_MS`]; or
    /// - Raw Input saw the mouse move recently but the mouse hook has been
    ///   silent for over [`HOOK_SILENCE_MS`].
    ///
    /// Never within [`HOOK_SILENCE_MS`] of the last install, so a reinstall
    /// gets a fair chance before the next one.
    pub fn needs_reinstall(&self) -> bool {
        if age(self.now, self.installed) <= HOOK_SILENCE_MS {
            return false;
        }
        let hooks_silent = age(
            self.now,
            latest_hook(self.now, self.mouse_hook, self.keyboard_hook),
        ) > HOOK_SILENCE_MS;
        let input_recent = self
            .last_input
            .is_some_and(|t| age(self.now, t) < RECENT_INPUT_MS);
        let mouse_silent = age(self.now, self.mouse_hook) > HOOK_SILENCE_MS;
        let raw_recent = self
            .last_raw
            .is_some_and(|t| age(self.now, t) < RECENT_RAW_MS);
        (input_recent && hooks_silent) || (raw_recent && mouse_silent)
    }
}

/// The watchdog's lease check: if the suppression lease was renewed at some
/// point but has run out, clears it and returns `true`; the caller must then
/// go back to passive capture and un-park the cursor.
pub fn lease_lapsed(lease: &mut SuppressLease, now: Instant) -> bool {
    if lease.lapsed(now) {
        lease.clear();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use hop_platform::SUPPRESS_LEASE;

    use super::*;

    fn health(now: u32) -> HookHealth {
        HookHealth {
            now,
            installed: now.wrapping_sub(60_000),
            mouse_hook: now.wrapping_sub(10),
            keyboard_hook: now.wrapping_sub(5_000),
            last_input: Some(now.wrapping_sub(10)),
            last_raw: Some(now.wrapping_sub(10)),
        }
    }

    #[test]
    fn healthy_hooks_are_left_alone() {
        assert!(!health(100_000).needs_reinstall());
        // Idle machine: no recent input, silent hooks are fine.
        let idle = HookHealth {
            mouse_hook: 1_000,
            keyboard_hook: 1_000,
            last_input: Some(1_000),
            last_raw: Some(1_000),
            ..health(100_000)
        };
        assert!(!idle.needs_reinstall());
    }

    #[test]
    fn recent_input_with_silent_hooks_means_they_were_removed() {
        let removed = HookHealth {
            mouse_hook: 90_000,
            keyboard_hook: 90_000,
            last_input: Some(99_500),
            last_raw: None,
            ..health(100_000)
        };
        assert!(removed.needs_reinstall());
        // Typing only, with a silent mouse hook, is not a sign by itself...
        let typing = HookHealth {
            mouse_hook: 90_000,
            keyboard_hook: 99_900,
            last_input: Some(99_900),
            last_raw: None,
            ..health(100_000)
        };
        assert!(!typing.needs_reinstall());
        // ...but raw mouse motion the mouse hook never saw is.
        let mouse_gone = HookHealth {
            last_raw: Some(99_800),
            ..typing
        };
        assert!(mouse_gone.needs_reinstall());
    }

    #[test]
    fn a_fresh_install_gets_time_before_the_next() {
        let fresh = HookHealth {
            installed: 99_000,
            mouse_hook: 99_000,
            keyboard_hook: 99_000,
            last_input: Some(99_990),
            last_raw: Some(99_990),
            ..health(100_000)
        };
        assert!(!fresh.needs_reinstall());
        assert!(
            HookHealth {
                now: 101_500,
                ..fresh
            }
            .needs_reinstall()
        );
    }

    #[test]
    fn tick_wraparound_is_handled() {
        // 49.7 days in: the clock wrapped between the hook call and now.
        let now = 1_000_u32;
        let removed = HookHealth {
            now,
            installed: u32::MAX - 100_000,
            mouse_hook: u32::MAX - 5_000,
            keyboard_hook: u32::MAX - 5_000,
            last_input: Some(500),
            last_raw: None,
        };
        assert!(removed.needs_reinstall());
        assert!(!health(5).needs_reinstall());
        assert_eq!(age(5, u32::MAX), 6);
        assert_eq!(latest_hook(5, u32::MAX, 3), 3);
        assert_eq!(latest_hook(5, 4, u32::MAX - 10), 4);
        // Input "from the future" (read in the other order) is not recent.
        let future = HookHealth {
            mouse_hook: 90_000,
            keyboard_hook: 90_000,
            last_input: Some(100_001),
            last_raw: None,
            ..health(100_000)
        };
        assert!(!future.needs_reinstall());
    }

    #[test]
    fn the_lease_lapses_once_and_then_stays_passive() {
        let start = Instant::now();
        let mut lease = SuppressLease::default();
        assert!(!lease_lapsed(&mut lease, start), "never renewed");
        lease.renew(start);
        assert!(!lease_lapsed(&mut lease, start + SUPPRESS_LEASE / 2));
        assert!(lease.active(start + SUPPRESS_LEASE / 2));
        assert!(lease_lapsed(&mut lease, start + SUPPRESS_LEASE));
        assert!(!lease.active(start + SUPPRESS_LEASE));
        assert!(
            !lease_lapsed(&mut lease, start + SUPPRESS_LEASE * 2),
            "only once"
        );
        // Renewing again restarts it.
        lease.renew(start + SUPPRESS_LEASE * 3);
        assert!(lease.active(start + SUPPRESS_LEASE * 3));
        assert!(lease_lapsed(&mut lease, start + SUPPRESS_LEASE * 5));
    }
}
