//! The veil: hiding the cursor that the other computer's session left
//! behind on this PC, and showing it again (docs/M4-M5-DESIGN.md, section
//! 2).
//!
//! When the other computer's cursor leaves this PC through an edge, the
//! PC's cursor stays where the last injected move put it. The veil hides it
//! there: the parking overlay (an [`OVERLAY_SIZE`](super::park::OVERLAY_SIZE)
//! square, nearly invisible, topmost window whose class cursor is
//! transparent) is placed around the cursor. Unlike parking, nothing is
//! warped, clipped or swallowed, so the PC's own mouse works as always.
//!
//! The veil lifts:
//! - at the first *physical* mouse event: motion, a button or the wheel
//!   (keys do not count);
//! - when the other computer enters again (it reveals the cursor
//!   explicitly), when the cursor is parked or restored, when capture
//!   stops, a lease lapses, the input thread panics or the cursor control
//!   is dropped;
//! - at a watchdog tick that finds the cursor outside the veil, or no mouse
//!   hook to see the first physical event.
//!
//! What Hop injects never lifts it. `SendInput` only queues events, so the
//! last moves of the session that just ended can reach the mouse hook after
//! the veil went up; for [`SETTLE`] after that, the veil follows Hop's own
//! moves instead, so it stays over the cursor.
//!
//! Even if all of that failed, the veil hides the cursor only while the
//! cursor is over it: moving the mouse a few pixels shows it, and the window
//! is gone with the process.

use std::time::{Duration, Instant};

use super::coords::{PhysPoint, PhysRect};
use super::park::overlay_around;

/// For how long after the veil goes up it follows Hop's own injected moves,
/// which may still be on their way from the session that just ended.
pub const SETTLE: Duration = Duration::from_millis(500);

/// A veiled cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Veil {
    /// Where the overlay is: around the cursor.
    pub rect: PhysRect,
    /// When the veil went up.
    pub since: Instant,
}

impl Veil {
    /// A veil around a cursor at `cursor`, going up at `now`.
    pub fn new(cursor: PhysPoint, now: Instant) -> Self {
        Self {
            rect: overlay_around(cursor),
            since: now,
        }
    }

    /// Whether Hop's own last moves may still be on their way at `now`.
    pub fn settling(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) < SETTLE
    }
}

/// What one mouse-hook event does to the veil.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VeilChange {
    /// Nothing.
    Keep,
    /// Move the overlay here, around where Hop's own move takes the cursor.
    Follow(PhysRect),
    /// Take the veil away: the user touched the mouse.
    Lift,
}

/// What one mouse-hook event does to `veil` at `now`. `physical` is false
/// for the events Hop injects (tagged `INJECTED_TAG`); `moved_to` is where a
/// move takes the cursor, `None` for a button or the wheel.
pub fn on_mouse_event(
    veil: &Veil,
    physical: bool,
    moved_to: Option<PhysPoint>,
    now: Instant,
) -> VeilChange {
    if physical {
        return VeilChange::Lift;
    }
    match moved_to.map(overlay_around) {
        Some(rect) if veil.settling(now) && rect != veil.rect => VeilChange::Follow(rect),
        _ => VeilChange::Keep,
    }
}

/// The watchdog's check: whether to lift `veil` at `now`. `cursor` is where
/// the cursor is (`None` if Windows could not say), `mouse_hook` whether the
/// mouse hook that would see the first physical event is installed.
///
/// While the veil settles, the cursor can be a step ahead of the overlay
/// (Hop's own move has been decided on, but not applied yet), so it is not
/// lifted for being outside then.
pub fn watchdog_lifts(
    veil: &Veil,
    cursor: Option<PhysPoint>,
    mouse_hook: bool,
    now: Instant,
) -> bool {
    if !mouse_hook {
        return true;
    }
    if veil.settling(now) {
        return false;
    }
    !cursor.is_some_and(|cursor| veil.rect.contains(cursor))
}

#[cfg(test)]
mod tests {
    use super::super::park::OVERLAY_SIZE;
    use super::*;

    fn p(x: i32, y: i32) -> PhysPoint {
        PhysPoint::new(x, y)
    }

    #[test]
    fn the_veil_goes_up_around_the_cursor() {
        let now = Instant::now();
        // Left at the PC's left edge, where the other computer's cursor left.
        let veil = Veil::new(p(0, 412), now);
        assert!(veil.rect.contains(p(0, 412)));
        assert_eq!(veil.rect.width(), OVERLAY_SIZE);
        assert_eq!(veil.rect, overlay_around(p(0, 412)));
        assert!(veil.settling(now));
        assert!(veil.settling(now + SETTLE - Duration::from_millis(1)));
        assert!(!veil.settling(now + SETTLE));
    }

    #[test]
    fn the_first_physical_mouse_event_lifts_it() {
        let t = Instant::now();
        let veil = Veil::new(p(0, 412), t);
        // Motion, at once or much later; a button or the wheel (no position).
        for at in [t, t + Duration::from_secs(60)] {
            assert_eq!(
                on_mouse_event(&veil, true, Some(p(1, 412)), at),
                VeilChange::Lift
            );
            assert_eq!(on_mouse_event(&veil, true, None, at), VeilChange::Lift);
        }
        // Even a move to where the cursor already is (pushing at the edge).
        assert_eq!(
            on_mouse_event(&veil, true, Some(p(0, 412)), t),
            VeilChange::Lift
        );
    }

    #[test]
    fn hops_own_events_never_lift_it() {
        let t = Instant::now();
        let veil = Veil::new(p(0, 412), t);
        // Buttons released when the session ended, arriving late.
        for at in [t, t + SETTLE, t + Duration::from_secs(60)] {
            assert_eq!(on_mouse_event(&veil, false, None, at), VeilChange::Keep);
        }
        // A late move after the veil settled leaves it where it is; the
        // watchdog lifts it if the cursor ends up outside.
        assert_eq!(
            on_mouse_event(&veil, false, Some(p(40, 400)), t + SETTLE),
            VeilChange::Keep
        );
    }

    #[test]
    fn it_follows_the_last_moves_of_the_session_that_just_ended() {
        let t = Instant::now();
        let veil = Veil::new(p(10, 412), t);
        // The session's last move, still queued when the veil went up.
        assert_eq!(
            on_mouse_event(&veil, false, Some(p(0, 409)), t + Duration::from_millis(3)),
            VeilChange::Follow(overlay_around(p(0, 409)))
        );
        // A move to the point the veil is already around changes nothing.
        assert_eq!(
            on_mouse_event(&veil, false, Some(p(10, 412)), t),
            VeilChange::Keep
        );
        // A small move still re-centres it, so the cursor stays in the middle.
        assert_eq!(
            on_mouse_event(&veil, false, Some(p(11, 412)), t),
            VeilChange::Follow(overlay_around(p(11, 412)))
        );
    }

    #[test]
    fn the_watchdog_lifts_a_veil_the_cursor_left() {
        let t = Instant::now();
        let veil = Veil::new(p(0, 412), t);
        let later = t + SETTLE;
        assert!(!watchdog_lifts(&veil, Some(p(0, 412)), true, later));
        assert!(!watchdog_lifts(&veil, Some(p(3, 415)), true, later));
        // Something moved the cursor off it (another app, or a move the hook
        // never saw): it shows anyway, so the veil goes.
        assert!(watchdog_lifts(&veil, Some(p(4, 412)), true, later));
        assert!(watchdog_lifts(&veil, Some(p(900, 50)), true, later));
        // Windows could not say where the cursor is: fail open.
        assert!(watchdog_lifts(&veil, None, true, later));
    }

    #[test]
    fn the_watchdog_gives_a_settling_veil_time_but_never_without_a_hook() {
        let t = Instant::now();
        let veil = Veil::new(p(0, 412), t);
        // Hop's own last move is decided on but not applied yet.
        let soon = t + Duration::from_millis(100);
        assert!(!watchdog_lifts(&veil, Some(p(30, 412)), true, soon));
        assert!(!watchdog_lifts(&veil, None, true, soon));
        // Without a mouse hook, nothing would see the user touch the mouse.
        assert!(watchdog_lifts(&veil, Some(p(0, 412)), false, t));
        assert!(watchdog_lifts(&veil, Some(p(0, 412)), false, t + SETTLE));
    }
}
