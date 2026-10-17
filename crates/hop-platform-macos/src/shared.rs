//! State shared by capture, the cursor and the lease watchdog.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Instant;

use hop_platform::{CaptureMode, SuppressLease};
use hop_types::Point;

/// Locks `mutex`, carrying on if another thread panicked while holding it:
/// every value guarded here stays consistent between statements, and
/// failing open matters more than refusing to touch the cursor.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One per [`hop_platform::Platform`]: the capture thread, the cursor
/// control and the lease watchdog all see the same lease and cursor state.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    lease: Mutex<SuppressLease>,
    cursor: Mutex<CursorState>,
    /// Where a warp last put the cursor. Warps produce no event, so capture
    /// would otherwise report the pre-warp position as the "previous
    /// position" of the next event.
    warped_to: Mutex<Option<Point>>,
}

/// What the cursor control has done to the cursor, so it can be undone
/// exactly once.
#[derive(Debug, Default)]
pub(crate) struct CursorState {
    /// Where the cursor was when it was parked; `Some` while parked.
    pub(crate) parked_from: Option<Point>,
    /// The mouse is disconnected from the cursor
    /// (`CGAssociateMouseAndMouseCursorPosition(false)`).
    pub(crate) detached: bool,
    /// Hop holds one `CGDisplayHideCursor` that no `CGDisplayShowCursor` has
    /// balanced yet. macOS counts hides, so they must stay paired.
    pub(crate) hidden: bool,
}

impl Shared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Starts or extends suppression from `now`.
    pub(crate) fn renew_lease(&self, now: Instant) {
        lock(&self.lease).renew(now);
    }

    /// Ends suppression.
    pub(crate) fn clear_lease(&self) {
        lock(&self.lease).clear();
    }

    /// Whether physical input is being swallowed at `now`.
    pub(crate) fn suppressing(&self, now: Instant) -> bool {
        lock(&self.lease).active(now)
    }

    /// The capture mode in effect at `now`, after any lapse.
    pub(crate) fn mode(&self, now: Instant) -> CaptureMode {
        if self.suppressing(now) {
            CaptureMode::Suppress
        } else {
            CaptureMode::Passive
        }
    }

    /// Whether the lease ran out by `now` without being renewed. A lapse is
    /// reported once: the lease is cleared, so the watchdog un-parks the
    /// cursor once rather than every tick.
    pub(crate) fn take_lapse(&self, now: Instant) -> bool {
        let mut lease = lock(&self.lease);
        let lapsed = lease.lapsed(now);
        if lapsed {
            lease.clear();
        }
        lapsed
    }

    /// The cursor state. Hold the guard only for the few calls that change
    /// the cursor.
    pub(crate) fn cursor(&self) -> MutexGuard<'_, CursorState> {
        lock(&self.cursor)
    }

    /// The cursor state, unless another thread holds it right now.
    pub(crate) fn try_cursor(&self) -> Option<MutexGuard<'_, CursorState>> {
        match self.cursor.try_lock() {
            Ok(guard) => Some(guard),
            // As in `lock`: carry on after a panic elsewhere.
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Records that a warp just put the cursor at `to`.
    pub(crate) fn note_warp(&self, to: Point) {
        *lock(&self.warped_to) = Some(to);
    }

    /// The position of the last warp that capture has not seen yet.
    pub(crate) fn take_warp(&self) -> Option<Point> {
        lock(&self.warped_to).take()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hop_platform::SUPPRESS_LEASE;

    use super::*;

    #[test]
    fn suppression_lasts_one_lease_unless_renewed() {
        let shared = Shared::new();
        let start = Instant::now();
        assert_eq!(shared.mode(start), CaptureMode::Passive);

        shared.renew_lease(start);
        assert_eq!(shared.mode(start), CaptureMode::Suppress);
        assert!(shared.suppressing(start + SUPPRESS_LEASE / 2));
        assert!(!shared.take_lapse(start + SUPPRESS_LEASE / 2));

        // Renewed halfway, it now runs until 1.5 leases after the start.
        shared.renew_lease(start + SUPPRESS_LEASE / 2);
        assert!(shared.suppressing(start + SUPPRESS_LEASE));
        assert!(!shared.take_lapse(start + SUPPRESS_LEASE));

        let after = start + SUPPRESS_LEASE * 2;
        assert_eq!(shared.mode(after), CaptureMode::Passive);
        assert!(shared.take_lapse(after), "the watchdog sees the lapse");
        assert!(
            !shared.take_lapse(after + Duration::from_millis(100)),
            "and only once"
        );
    }

    #[test]
    fn going_passive_is_not_a_lapse() {
        let shared = Shared::new();
        let start = Instant::now();
        shared.renew_lease(start);
        shared.clear_lease();
        assert_eq!(shared.mode(start), CaptureMode::Passive);
        assert!(!shared.take_lapse(start + SUPPRESS_LEASE * 3));
    }

    #[test]
    fn a_warp_is_reported_once() {
        let shared = Shared::new();
        assert_eq!(shared.take_warp(), None);
        shared.note_warp(Point::new(1.0, 2.0));
        shared.note_warp(Point::new(3.0, 4.0));
        assert_eq!(shared.take_warp(), Some(Point::new(3.0, 4.0)));
        assert_eq!(shared.take_warp(), None);
    }
}
