//! Where the cursor waits, hidden, while this PC controls another device,
//! and when a parked cursor must be given back.

use std::time::{Duration, Instant};

use super::coords::{PhysPoint, PhysRect};

/// Side of the invisible window that hides the parked cursor, in physical
/// pixels. Small, but more than one pixel, so a one-pixel nudge cannot
/// uncover the cursor.
pub const OVERLAY_SIZE: i32 = 8;

/// Where the invisible window goes to hide a cursor at `p`: an
/// [`OVERLAY_SIZE`] square with `p` at its middle (just right of and below
/// it, for an even size). Parking puts it at the park spot; the veil (see
/// `veil`) wherever the cursor was left.
pub fn overlay_around(p: PhysPoint) -> PhysRect {
    let left = p.x.saturating_sub(OVERLAY_SIZE / 2);
    let top = p.y.saturating_sub(OVERLAY_SIZE / 2);
    PhysRect::new(
        left,
        top,
        left.saturating_add(OVERLAY_SIZE),
        top.saturating_add(OVERLAY_SIZE),
    )
}

/// How long the cursor may stay parked with no suppression lease in force
/// before the watchdog gives it back.
///
/// Hop's core always suppresses while the cursor is parked, and a lease
/// that runs out un-parks the cursor at once. This covers the other ways a
/// park can be left behind: suppression ended with `Passive` but the restore
/// never came, or a park without any suppression (the self-test parks alone
/// for 2 seconds) whose caller hung. A parked cursor is pinned, so without
/// this the mouse would stay stuck until Hop exits. Each `park` call starts
/// the time again.
pub const PARKED_WITHOUT_LEASE: Duration = Duration::from_secs(5);

/// Watches a parked cursor for [`PARKED_WITHOUT_LEASE`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParkWatch {
    /// Since when the cursor has been parked with no lease in force.
    unleased_since: Option<Instant>,
}

impl ParkWatch {
    /// The cursor was just parked: the time starts again.
    pub fn parked(&mut self) {
        self.unleased_since = None;
    }

    /// The watchdog's check, every tick. Returns `true` once the cursor has
    /// been `parked` without a lease (`leased` false) for
    /// [`PARKED_WITHOUT_LEASE`]: the caller must then un-park it.
    pub fn check(&mut self, parked: bool, leased: bool, now: Instant) -> bool {
        if !parked || leased {
            self.unleased_since = None;
            return false;
        }
        let since = *self.unleased_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= PARKED_WITHOUT_LEASE {
            self.unleased_since = None;
            true
        } else {
            false
        }
    }
}

/// The parked cursor's geometry on one monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParkSpot {
    /// Where the cursor is put: the monitor's middle.
    pub center: PhysPoint,
    /// The invisible window under the cursor, whose transparent class cursor
    /// is what hides it.
    pub overlay: PhysRect,
    /// What the cursor is clipped to (`ClipCursor`).
    pub clip: PhysRect,
}

/// The park spot for `monitor` (the primary monitor), or `None` if the
/// monitor has no area.
///
/// The clip rectangle is deliberately larger than the overlay: a quarter of
/// the monitor's smaller side around the middle, in each direction.
/// Windows clamps the position a low-level mouse hook reports to the clip
/// rectangle, and while parked, motion is measured as that position minus
/// the parked position, so a tiny clip would clamp the measurement to
/// nothing. This one still keeps the cursor far from every screen edge,
/// which is what parking is for.
pub fn park_spot(monitor: PhysRect) -> Option<ParkSpot> {
    if monitor.is_empty() {
        return None;
    }
    let center = monitor.center();
    let overlay = overlay_around(center);
    let reach = (monitor.width().min(monitor.height()) / 4).max(1);
    let clip = PhysRect::new(
        (center.x - reach).max(monitor.left),
        (center.y - reach).max(monitor.top),
        (center.x + reach + 1).min(monitor.right),
        (center.y + reach + 1).min(monitor.bottom),
    );
    Some(ParkSpot {
        center,
        overlay,
        clip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spot_is_the_middle_with_the_overlay_under_it() {
        let spot = park_spot(PhysRect::new(0, 0, 1920, 1080)).expect("a real monitor");
        assert_eq!(spot.center, PhysPoint::new(959, 539));
        assert!(spot.overlay.contains(spot.center));
        assert_eq!(spot.overlay.width(), OVERLAY_SIZE);
        assert!(spot.clip.contains(spot.center));
        // 1080 / 4 = 270 pixels of room each way.
        assert_eq!(spot.clip, PhysRect::new(689, 269, 1230, 810));
        assert_eq!(spot.overlay, PhysRect::new(955, 535, 963, 543));
    }

    #[test]
    fn the_overlay_is_centred_on_the_cursor_wherever_it_is() {
        for p in [
            PhysPoint::new(0, 0),
            PhysPoint::new(1919, 540),
            PhysPoint::new(-2560, -400),
            PhysPoint::new(3, 1079),
        ] {
            let overlay = overlay_around(p);
            assert!(overlay.contains(p), "{p:?}");
            assert_eq!(overlay.width(), OVERLAY_SIZE);
            assert_eq!(overlay.height(), OVERLAY_SIZE);
            assert_eq!(overlay.center(), PhysPoint::new(p.x - 1, p.y - 1));
            // Half the overlay on each side, so a nudge either way stays on it.
            assert_eq!(p.x - overlay.left, OVERLAY_SIZE / 2);
            assert_eq!(overlay.right - p.x, OVERLAY_SIZE / 2);
        }
        // Absurd coordinates saturate instead of wrapping.
        let corner = overlay_around(PhysPoint::new(i32::MIN, i32::MAX));
        assert_eq!(corner.left, i32::MIN);
        assert_eq!(corner.bottom, i32::MAX);
    }

    #[test]
    fn the_clip_stays_on_the_monitor_and_away_from_its_edges() {
        for monitor in [
            PhysRect::new(0, 0, 1366, 768),
            PhysRect::new(-2560, -400, 0, 1040),
            PhysRect::new(0, 0, 3, 3),
            PhysRect::new(0, 0, 1, 1),
        ] {
            let spot = park_spot(monitor).expect("a real monitor");
            assert!(monitor.contains(spot.center));
            assert!(spot.clip.contains(spot.center));
            assert!(spot.clip.left >= monitor.left && spot.clip.right <= monitor.right);
            assert!(spot.clip.top >= monitor.top && spot.clip.bottom <= monitor.bottom);
            if monitor.width() > 8 {
                assert!(spot.clip.left > monitor.left && spot.clip.right < monitor.right);
            }
        }
        assert_eq!(park_spot(PhysRect::new(0, 0, 0, 100)), None);
    }

    #[test]
    fn a_park_no_lease_covers_is_given_back() {
        // Regression: a park left behind by `Passive` without a restore was
        // re-applied every tick forever.
        let t = Instant::now();
        let tick = Duration::from_millis(500);
        let mut watch = ParkWatch::default();
        watch.parked();
        let mut released_at = None;
        for n in 1..=20 {
            if watch.check(true, false, t + tick * n) {
                released_at = Some(tick * n);
                break;
            }
        }
        // The first unleased tick starts the time, so the release comes one
        // grace period after it.
        assert_eq!(released_at, Some(tick + PARKED_WITHOUT_LEASE));
        // Once released, it does not fire again by itself.
        assert!(!watch.check(true, false, t + tick * 13));
    }

    #[test]
    fn a_leased_park_is_left_alone_and_a_new_park_restarts_the_time() {
        let t = Instant::now();
        let mut watch = ParkWatch::default();
        // Parked and suppressing, as Hop's core does: never released here
        // (a lapsed lease is handled on its own, at once).
        for n in 0..100 {
            assert!(!watch.check(true, true, t + Duration::from_secs(n)));
        }
        // The self-test parks alone for 2 seconds: well within the grace.
        assert!(!watch.check(true, false, t));
        assert!(!watch.check(true, false, t + Duration::from_secs(2)));
        // Parking again restarts the time.
        watch.parked();
        assert!(!watch.check(true, false, t + Duration::from_secs(4)));
        assert!(!watch.check(true, false, t + Duration::from_secs(8)));
        assert!(watch.check(true, false, t + Duration::from_secs(9)));
        // A lease, or an un-park, in between also restarts it.
        assert!(!watch.check(true, false, t + Duration::from_secs(10)));
        assert!(!watch.check(true, true, t + Duration::from_secs(11)));
        assert!(!watch.check(true, false, t + Duration::from_secs(12)));
        assert!(!watch.check(false, false, t + Duration::from_secs(16)));
        assert!(!watch.check(true, false, t + Duration::from_secs(17)));
        assert!(!watch.check(true, false, t + Duration::from_secs(21)));
        assert!(watch.check(true, false, t + Duration::from_secs(22)));
    }
}
