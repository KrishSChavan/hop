//! Pointer motion from the low-level mouse hook and Raw Input, including
//! pushes past a screen edge.
//!
//! # Where motion comes from
//!
//! The mouse hook reports where the cursor *would* go (`MSLLHOOKSTRUCT::pt`,
//! after Windows' pointer acceleration, clamped to the desktop), and while
//! the hook runs the cursor is still at its old position
//! (`GetCursorPos`). Their difference is the motion, shaped by the OS as the
//! contract asks:
//! - **Passive:** the event goes through and the cursor moves.
//! - **Suppressed or parked:** the event is swallowed (see `swallow`), so
//!   the cursor stays where it is and every event measures from there:
//!   "would-be minus actual".
//!
//! # Pushing past an edge
//!
//! At an outer edge of the desktop Windows clamps the position, so the hook
//! reports no motion on that axis even though the user is pushing. Raw Input
//! (`WM_INPUT`) still reports the device's motion there. The two arrive as
//! separate messages, in no guaranteed order, so raw motion is collected
//! between hook events: a hook event that did not move on an axis, at an
//! edge where nothing lies beyond, takes the collected raw motion for that
//! axis if it points outward. If raw motion keeps arriving with no hook
//! events at all (in case Windows skips the hook when the position cannot
//! change), the outward part is reported on its own after a few raw events.
//!
//! Raw motion is in device counts, without acceleration. It is used only at
//! edges, where all that matters is that the push is seen.
//!
//! # Hop's own cursor warps
//!
//! When Hop moves the cursor itself (`SetCursorPos`, to park or restore it),
//! one hook event may already be on its way, computed from the old position.
//! Measured from the new position it would be a huge bogus jump, and letting
//! it through would undo the warp. The first hook event after a warp is
//! therefore checked: if it lies nearer the old position than the new one,
//! it is stale and is swallowed without being reported.

use std::time::{Duration, Instant};

use super::coords::{PhysPoint, PhysRect};

/// A hook event this long after a warp can no longer be from before it.
pub const STALE_WINDOW: Duration = Duration::from_millis(500);

/// Raw motion older than this belongs to an earlier gesture and is dropped.
pub const RAW_GAP: Duration = Duration::from_millis(50);

/// After this many raw events with no hook event in between, the hook is
/// presumed not to be firing at the edge, and the outward raw motion is
/// reported on its own.
pub const RAW_WITHOUT_HOOK: u32 = 3;

/// The directions in which the cursor cannot move from a pixel because no
/// monitor lies there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Blocked {
    /// No monitor to the left.
    pub left: bool,
    /// No monitor to the right.
    pub right: bool,
    /// No monitor above.
    pub up: bool,
    /// No monitor below.
    pub down: bool,
}

/// Which directions are blocked at `p`, given the monitors' rectangles. A
/// point on no monitor (which Windows never reports) is blocked nowhere.
///
/// Works for any arrangement, including monitors of different sizes, where
/// part of one monitor's edge borders another monitor and part does not.
pub fn blocked_at(p: PhysPoint, monitors: &[PhysRect]) -> Blocked {
    let on = |x: i32, y: i32| monitors.iter().any(|m| m.contains(PhysPoint::new(x, y)));
    if !on(p.x, p.y) {
        return Blocked::default();
    }
    Blocked {
        left: !on(p.x.saturating_sub(1), p.y),
        right: !on(p.x.saturating_add(1), p.y),
        up: !on(p.x, p.y.saturating_sub(1)),
        down: !on(p.x, p.y.saturating_add(1)),
    }
}

/// Where the cursor was before a mouse-hook event at `pt`.
///
/// Inside a low-level mouse hook the event has not been applied yet, so
/// `GetCursorPos` (`reported`) still gives the old position; that is the
/// answer. Two fallbacks keep motion working if that ever fails:
/// - no reading at all: where the previous event left the cursor (`last`);
/// - a reading equal to `pt` although the previous event left the cursor
///   elsewhere, which is what `GetCursorPos` returning the *new* position
///   would look like: `last`, so the motion is not lost.
///
/// When the reading equals `pt` and `last` does too, the cursor really did
/// not move (pushing against an edge, or parked), and the answer is the same
/// either way.
pub fn position_before(reported: Option<PhysPoint>, pt: PhysPoint, last: PhysPoint) -> PhysPoint {
    match reported {
        Some(reported) if reported != pt || last == pt => reported,
        _ => last,
    }
}

/// The parts of `(dx, dy)` that push into a blocked direction.
pub fn outward(dx: i64, dy: i64, blocked: Blocked) -> (i64, i64) {
    let x = if (dx > 0 && blocked.right) || (dx < 0 && blocked.left) {
        dx
    } else {
        0
    };
    let y = if (dy > 0 && blocked.down) || (dy < 0 && blocked.up) {
        dy
    } else {
        0
    };
    (x, y)
}

/// What to do with one mouse-hook move event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookMove {
    /// Report this motion, in physical pixels.
    Report {
        /// Rightward motion.
        dx: i64,
        /// Downward motion.
        dy: i64,
    },
    /// No motion to report.
    Still,
    /// Computed before Hop's own warp: swallow the event, report nothing.
    Stale,
}

/// A cursor move Hop made itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Warp {
    from: PhysPoint,
    to: PhysPoint,
    at: Instant,
}

/// Raw motion not yet matched with a hook event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PendingRaw {
    dx: i64,
    dy: i64,
    events: u32,
    last: Option<Instant>,
}

/// Turns hook and raw events into reported motion. Lives on the hook thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MotionTracker {
    pending: PendingRaw,
    warp: Option<Warp>,
}

impl MotionTracker {
    /// A tracker with nothing pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hop moved the cursor from `from` to `to` (where it actually ended
    /// up) at `now`.
    pub fn warped(&mut self, from: PhysPoint, to: PhysPoint, now: Instant) {
        self.warp = Some(Warp { from, to, at: now });
        self.pending = PendingRaw::default();
    }

    /// A physical move event from the mouse hook: `pt` is where the cursor
    /// would go, `before` where it is (`GetCursorPos` inside the hook).
    /// `passive` is false while the event is swallowed (suppressed or
    /// parked); raw motion is then ignored, since a cursor held still cannot
    /// cross an edge.
    pub fn hook_move(
        &mut self,
        pt: PhysPoint,
        before: PhysPoint,
        passive: bool,
        monitors: &[PhysRect],
        now: Instant,
    ) -> HookMove {
        if let Some(warp) = self.warp.take() {
            let recent = now.saturating_duration_since(warp.at) <= STALE_WINDOW;
            if recent && pt.distance_squared(warp.from) < pt.distance_squared(warp.to) {
                return HookMove::Stale;
            }
        }
        let mut dx = i64::from(pt.x) - i64::from(before.x);
        let mut dy = i64::from(pt.y) - i64::from(before.y);
        let raw = self.take_pending(now);
        if passive && let Some((raw_dx, raw_dy)) = raw {
            let (push_x, push_y) = outward(raw_dx, raw_dy, blocked_at(before, monitors));
            if dx == 0 {
                dx = push_x;
            }
            if dy == 0 {
                dy = push_y;
            }
        }
        if dx == 0 && dy == 0 {
            HookMove::Still
        } else {
            HookMove::Report { dx, dy }
        }
    }

    /// Relative motion from Raw Input (physical, not injected by Hop), with
    /// the cursor at `at`. Returns an edge push to report right away, in
    /// physical pixels, when raw motion keeps arriving without hook events.
    pub fn raw_motion(
        &mut self,
        dx: i32,
        dy: i32,
        at: PhysPoint,
        passive: bool,
        monitors: &[PhysRect],
        now: Instant,
    ) -> Option<(i64, i64)> {
        if !passive {
            self.pending = PendingRaw::default();
            return None;
        }
        if dx == 0 && dy == 0 {
            return None;
        }
        if self
            .pending
            .last
            .is_some_and(|last| now.saturating_duration_since(last) > RAW_GAP)
        {
            self.pending = PendingRaw::default();
        }
        self.pending.dx += i64::from(dx);
        self.pending.dy += i64::from(dy);
        self.pending.events += 1;
        self.pending.last = Some(now);
        if self.pending.events < RAW_WITHOUT_HOOK {
            return None;
        }
        let pending = std::mem::take(&mut self.pending);
        let push = outward(pending.dx, pending.dy, blocked_at(at, monitors));
        (push != (0, 0)).then_some(push)
    }

    /// The raw motion collected since the last hook event, if it is recent.
    fn take_pending(&mut self, now: Instant) -> Option<(i64, i64)> {
        let pending = std::mem::take(&mut self.pending);
        let recent = pending
            .last
            .is_some_and(|last| now.saturating_duration_since(last) <= RAW_GAP);
        (recent && pending.events > 0).then_some((pending.dx, pending.dy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920x1080 primary with a 1280x1024 monitor to its right, top
    /// aligned: the right monitor's lower part has nothing to its left...
    /// and the primary's right edge below row 1024 has nothing to its right.
    fn desk() -> Vec<PhysRect> {
        vec![
            PhysRect::new(0, 0, 1920, 1080),
            PhysRect::new(1920, 0, 3200, 1024),
        ]
    }

    fn p(x: i32, y: i32) -> PhysPoint {
        PhysPoint::new(x, y)
    }

    #[test]
    fn blocked_directions_follow_the_monitor_layout() {
        let desk = desk();
        assert_eq!(blocked_at(p(500, 500), &desk), Blocked::default());
        // Primary's right edge, beside the second monitor: open.
        assert!(!blocked_at(p(1919, 500), &desk).right);
        // Below the second monitor's bottom: nothing to the right.
        assert!(blocked_at(p(1919, 1050), &desk).right);
        assert!(blocked_at(p(1919, 1079), &desk).down);
        let corner = blocked_at(p(0, 0), &desk);
        assert!(corner.left && corner.up && !corner.right && !corner.down);
        assert!(blocked_at(p(3199, 10), &desk).right);
        assert!(blocked_at(p(2000, 1023), &desk).down);
        assert_eq!(blocked_at(p(-5, -5), &desk), Blocked::default());
    }

    #[test]
    fn the_position_before_an_event_comes_from_get_cursor_pos() {
        // The normal case: GetCursorPos still has the old position.
        assert_eq!(
            position_before(Some(p(100, 100)), p(105, 98), p(100, 100)),
            p(100, 100)
        );
        // Something else moved the cursor since the last event; the reading
        // is right and the stale `last` is ignored.
        assert_eq!(
            position_before(Some(p(300, 300)), p(305, 300), p(100, 100)),
            p(300, 300)
        );
        // Not moving (at an edge, or parked): all three agree.
        assert_eq!(
            position_before(Some(p(1919, 5)), p(1919, 5), p(1919, 5)),
            p(1919, 5)
        );
        // A reading equal to the new position, though the last event left the
        // cursor elsewhere: fall back to where it was left.
        assert_eq!(
            position_before(Some(p(105, 98)), p(105, 98), p(100, 100)),
            p(100, 100)
        );
        // No reading at all.
        assert_eq!(position_before(None, p(105, 98), p(100, 100)), p(100, 100));
    }

    #[test]
    fn hook_motion_is_where_the_cursor_would_go_minus_where_it_is() {
        let now = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        assert_eq!(
            motion.hook_move(p(105, 98), p(100, 100), true, &desk, now),
            HookMove::Report { dx: 5, dy: -2 }
        );
        assert_eq!(
            motion.hook_move(p(100, 100), p(100, 100), true, &desk, now),
            HookMove::Still
        );
        // Suppressed: the cursor stays parked, each event measures from it.
        assert_eq!(
            motion.hook_move(p(959 + 12, 539), p(959, 539), false, &desk, now),
            HookMove::Report { dx: 12, dy: 0 }
        );
    }

    #[test]
    fn raw_motion_reveals_a_push_past_an_outer_edge() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        // At the right edge below the second monitor, pushing right: the
        // hook's position does not change, Raw Input says +4.
        let edge = p(1919, 1050);
        assert_eq!(motion.raw_motion(4, 0, edge, true, &desk, t), None);
        assert_eq!(
            motion.hook_move(edge, edge, true, &desk, t + Duration::from_millis(1)),
            HookMove::Report { dx: 4, dy: 0 }
        );
        // Sliding down along the edge while pushing: one event carries both.
        assert_eq!(motion.raw_motion(3, 2, edge, true, &desk, t), None);
        assert_eq!(
            motion.hook_move(p(1919, 1052), edge, true, &desk, t),
            HookMove::Report { dx: 3, dy: 2 }
        );
        // Raw motion pointing inward, or along an open edge, is not a push.
        assert_eq!(motion.raw_motion(-4, 0, edge, true, &desk, t), None);
        assert_eq!(
            motion.hook_move(edge, edge, true, &desk, t),
            HookMove::Still
        );
        let open = p(1919, 500);
        assert_eq!(motion.raw_motion(4, 0, open, true, &desk, t), None);
        assert_eq!(
            motion.hook_move(open, open, true, &desk, t),
            HookMove::Still
        );
    }

    #[test]
    fn the_hook_axis_wins_when_it_moved() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        // Arriving at the edge: the hook moved 2 of the 6 counts; the push
        // shows up on the next event, not added to this one.
        assert_eq!(motion.raw_motion(6, 0, p(1917, 1050), true, &desk, t), None);
        assert_eq!(
            motion.hook_move(p(1919, 1050), p(1917, 1050), true, &desk, t),
            HookMove::Report { dx: 2, dy: 0 }
        );
    }

    #[test]
    fn old_raw_motion_is_not_a_push() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        let edge = p(1919, 1050);
        assert_eq!(motion.raw_motion(4, 0, edge, true, &desk, t), None);
        // A second later the user slides along the edge; the old raw count
        // (which arrived after its own hook event) must not count as a push.
        let later = t + Duration::from_secs(1);
        assert_eq!(
            motion.hook_move(p(1919, 1055), edge, true, &desk, later),
            HookMove::Report { dx: 0, dy: 5 }
        );
        // Same when the next raw event starts a new gesture.
        assert_eq!(motion.raw_motion(4, 0, edge, true, &desk, t), None);
        assert_eq!(motion.raw_motion(0, 3, edge, true, &desk, later), None);
        assert_eq!(
            motion.hook_move(p(1919, 1053), edge, true, &desk, later),
            HookMove::Report { dx: 0, dy: 3 }
        );
    }

    #[test]
    fn raw_pushes_are_reported_alone_if_the_hook_stays_quiet() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        let edge = p(1919, 1050);
        assert_eq!(motion.raw_motion(2, 1, edge, true, &desk, t), None);
        assert_eq!(motion.raw_motion(2, 1, edge, true, &desk, t), None);
        // Third raw event, no hook in between: the outward part goes out.
        assert_eq!(motion.raw_motion(2, 1, edge, true, &desk, t), Some((6, 0)));
        // And it is not reported again by the next hook event.
        assert_eq!(
            motion.hook_move(edge, edge, true, &desk, t),
            HookMove::Still
        );
        // Away from any edge, nothing is reported however many arrive.
        for _ in 0..10 {
            assert_eq!(motion.raw_motion(3, 3, p(500, 500), true, &desk, t), None);
        }
    }

    #[test]
    fn suppressed_input_ignores_raw_motion() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        let edge = p(1919, 1050);
        for _ in 0..5 {
            assert_eq!(motion.raw_motion(5, 0, edge, false, &desk, t), None);
        }
        assert_eq!(
            motion.hook_move(edge, edge, false, &desk, t),
            HookMove::Still
        );
    }

    #[test]
    fn the_event_in_flight_during_a_warp_is_stale() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        // Parking: from the right edge to the middle.
        motion.warped(p(1919, 500), p(959, 539), t);
        // An event computed before the warp, near the old position.
        assert_eq!(
            motion.hook_move(p(1919, 503), p(959, 539), false, &desk, t),
            HookMove::Stale
        );
        // Only the first event is suspect.
        assert_eq!(
            motion.hook_move(p(1919, 503), p(959, 539), false, &desk, t),
            HookMove::Report { dx: 960, dy: -36 }
        );
        // A fresh event right after a warp is measured normally.
        motion.warped(p(1919, 500), p(959, 539), t);
        assert_eq!(
            motion.hook_move(p(962, 539), p(959, 539), false, &desk, t),
            HookMove::Report { dx: 3, dy: 0 }
        );
        // Restoring: from the middle back to the edge; a stale event from
        // the parked position must not drag the cursor back.
        motion.warped(p(959, 539), p(1919, 500), t);
        assert_eq!(
            motion.hook_move(p(961, 539), p(1919, 500), true, &desk, t),
            HookMove::Stale
        );
        // Long after a warp, nothing is stale.
        motion.warped(p(959, 539), p(1919, 500), t);
        let later = t + STALE_WINDOW + Duration::from_millis(1);
        assert_eq!(
            motion.hook_move(p(961, 539), p(1919, 500), true, &desk, later),
            HookMove::Report { dx: -958, dy: 39 }
        );
        // A warp to where the cursor already was cannot make events stale.
        motion.warped(p(959, 539), p(959, 539), t);
        assert_eq!(
            motion.hook_move(p(960, 539), p(959, 539), false, &desk, t),
            HookMove::Report { dx: 1, dy: 0 }
        );
    }

    #[test]
    fn a_warp_drops_pending_raw_motion() {
        let t = Instant::now();
        let mut motion = MotionTracker::new();
        let desk = desk();
        let edge = p(1919, 1050);
        assert_eq!(motion.raw_motion(4, 0, edge, true, &desk, t), None);
        motion.warped(p(500, 500), edge, t);
        assert_eq!(
            motion.hook_move(edge, edge, true, &desk, t),
            HookMove::Still
        );
    }
}
