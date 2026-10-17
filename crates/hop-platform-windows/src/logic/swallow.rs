//! Which hook events are swallowed, so that no app sees them.
//!
//! A low-level hook swallows an event by returning nonzero: neither later
//! hooks nor any window receives it, and for a move the cursor stays where
//! it is. The rules, in order:
//!
//! 1. **Only physical events.** What Hop injects (tagged `INJECTED_TAG`)
//!    always goes through.
//! 2. **Only while capture reports.** An event that is swallowed but reported
//!    nowhere is simply lost. After the capture sink is gone (it panicked and
//!    was dropped), the runtime may still be renewing the lease, so input
//!    must flow to local apps again whatever the lease says: fail open.
//! 3. **Everything while suppressing.** While the suppression lease is
//!    active, every physical event goes to the other device instead.
//! 4. **Motion while parked.** A parked cursor is pinned by swallowing
//!    physical motion, with or without a lease: Windows has no way to
//!    disconnect the mouse from the cursor (macOS does), so this is how the
//!    cursor stays put while its motion is still reported. Buttons, keys and
//!    the wheel are not touched by parking alone. A park that no lease
//!    covers is ended by the watchdog (see `park::ParkWatch`), so this can
//!    never pin the mouse for long.

/// The kind of hook event, for [`swallows`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hooked {
    /// `WM_MOUSEMOVE`.
    Motion,
    /// A button, the wheel, or a key.
    Other,
}

/// Whether a hook swallows an event (see the module documentation).
///
/// `reporting`: a capture sink is attached, so the event is reported.
/// `suppressing`: the suppression lease is active. `parked`: Hop has parked
/// the cursor.
pub fn swallows(
    event: Hooked,
    physical: bool,
    reporting: bool,
    suppressing: bool,
    parked: bool,
) -> bool {
    physical && reporting && (suppressing || (parked && event == Hooked::Motion))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_swallows_every_physical_event() {
        for event in [Hooked::Motion, Hooked::Other] {
            assert!(swallows(event, true, true, true, false));
            assert!(swallows(event, true, true, true, true));
            assert!(!swallows(event, true, true, false, false));
        }
    }

    #[test]
    fn hops_own_events_always_go_through() {
        for event in [Hooked::Motion, Hooked::Other] {
            for (suppressing, parked) in [(true, true), (true, false), (false, true)] {
                assert!(!swallows(event, false, true, suppressing, parked));
            }
        }
    }

    #[test]
    fn a_parked_cursor_is_pinned_without_a_lease_but_only_its_motion() {
        // This is what keeps the parked cursor still in passive mode, as the
        // contract's "pin it" asks.
        assert!(swallows(Hooked::Motion, true, true, false, true));
        // Clicks, keys and the wheel still reach local apps.
        assert!(!swallows(Hooked::Other, true, true, false, true));
    }

    #[test]
    fn without_a_sink_nothing_is_swallowed() {
        // Regression: after a panicking sink was dropped, a lease the runtime
        // kept renewing swallowed all physical input, reported to nobody.
        for event in [Hooked::Motion, Hooked::Other] {
            for (suppressing, parked) in [(true, true), (true, false), (false, true)] {
                assert!(!swallows(event, true, false, suppressing, parked));
            }
        }
    }
}
