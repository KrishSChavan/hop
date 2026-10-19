//! State shared by capture, the cursor, the drop catcher and the lease
//! watchdog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Instant;

use hop_platform::{CaptureMode, SuppressLease};
use hop_types::Point;

use crate::catcher::DropState;
use crate::cursor::CursorCalls;

/// Locks `mutex`, carrying on if another thread panicked while holding it:
/// every value guarded here stays consistent between statements, and
/// failing open matters more than refusing to touch the cursor.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One per [`hop_platform::Platform`]: the capture thread, the cursor
/// control, the drop catcher and the lease watchdog all see the same lease,
/// cursor and drop state.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    lease: Mutex<SuppressLease>,
    cursor: Mutex<CursorState>,
    /// [`CursorState::concealed`], for the tap, which must never wait for
    /// the cursor lock. It is written only while that lock is held
    /// ([`Shared::set_concealed`]) and read without it.
    concealed: AtomicBool,
    /// The warp capture has not seen yet. Warps produce no event, so capture
    /// would otherwise report the pre-warp position as the "previous
    /// position" of the next event, and macOS adds a warp's distance to the
    /// delta of the next mouse event (see `convert`).
    warp: Mutex<Option<Warp>>,
    /// The calls that change the cursor: the WindowServer's own, except in
    /// tests.
    calls: CursorCalls,
    /// The drop zones, and the files dropped on them.
    drops: Arc<DropState>,
}

/// A warp of the cursor from `from` to `to`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Warp {
    /// Where the cursor was just before the warp.
    pub(crate) from: Point,
    /// Where the warp put it.
    pub(crate) to: Point,
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
    /// balanced yet. macOS counts hides, so they must stay paired: parking
    /// and concealing share this one flag, and so this one hide.
    pub(crate) hidden: bool,
    /// The cursor is hidden but not parked: a peer's session left it at the
    /// edge ([`hop_platform::CursorControl::conceal`]), or the show that
    /// should have ended a park failed. The next physical mouse event shows
    /// it again. Set exactly when `hidden` is and the cursor is neither
    /// parked nor detached: parking clears it, and so does every show that
    /// works.
    pub(crate) concealed: bool,
}

impl Shared {
    /// The state for a new platform, changing the real cursor.
    pub(crate) fn new() -> Arc<Self> {
        Self::with_calls(CursorCalls::REAL)
    }

    /// The state for a new platform whose cursor changes go through
    /// `calls`. Tests pass calls that only record what would have happened.
    pub(crate) fn with_calls(calls: CursorCalls) -> Arc<Self> {
        Arc::new(Self {
            calls,
            ..Self::default()
        })
    }

    /// The calls that change the cursor.
    pub(crate) fn calls(&self) -> &CursorCalls {
        &self.calls
    }

    /// The drop zones, and the files dropped on them.
    pub(crate) fn drops(&self) -> &Arc<DropState> {
        &self.drops
    }

    /// Whether the cursor is concealed, read without the cursor lock: the
    /// answer can be a moment old, so act on it only through the lock.
    pub(crate) fn concealed(&self) -> bool {
        self.concealed.load(Ordering::Acquire)
    }

    /// Sets [`CursorState::concealed`] and the copy the tap reads together.
    /// `state` is this state's cursor, locked by the caller.
    pub(crate) fn set_concealed(&self, state: &mut CursorState, concealed: bool) {
        state.concealed = concealed;
        self.concealed.store(concealed, Ordering::Release);
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

    /// Records a warp from `from` to `to`, just before it happens: the next
    /// event can carry the warp's distance as soon as the warp is done.
    /// Warps that capture has not seen yet add up, so `from` stays the
    /// first one's. A warp that then fails is cancelled by noting the
    /// reverse move, which works whether or not capture has already taken
    /// the record (the converter adds up what it takes the same way).
    pub(crate) fn note_warp(&self, from: Point, to: Point) {
        let mut warp = lock(&self.warp);
        let from = warp.map_or(from, |w| w.from);
        *warp = Some(Warp { from, to });
    }

    /// The warp that capture has not seen yet.
    pub(crate) fn take_warp(&self) -> Option<Warp> {
        lock(&self.warp).take()
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
        shared.note_warp(Point::new(0.0, 0.0), Point::new(3.0, 4.0));
        let warp = Some(Warp {
            from: Point::new(0.0, 0.0),
            to: Point::new(3.0, 4.0),
        });
        assert_eq!(shared.take_warp(), warp);
        assert_eq!(shared.take_warp(), None);
    }

    #[test]
    fn warps_capture_has_not_seen_add_up() {
        let shared = Shared::new();
        shared.note_warp(Point::new(1511.0, 300.0), Point::new(756.0, 491.0));
        shared.note_warp(Point::new(756.0, 491.0), Point::new(1511.0, 420.0));
        // macOS adds both distances to the next event, so the first warp's
        // starting point counts.
        assert_eq!(
            shared.take_warp(),
            Some(Warp {
                from: Point::new(1511.0, 300.0),
                to: Point::new(1511.0, 420.0),
            })
        );
    }

    #[test]
    fn the_tap_s_copy_of_concealed_follows_the_cursor_state() {
        let shared = Shared::new();
        assert!(!shared.concealed());
        let mut state = shared.cursor();
        shared.set_concealed(&mut state, true);
        assert!(state.concealed && shared.concealed());
        shared.set_concealed(&mut state, false);
        assert!(!state.concealed && !shared.concealed());
    }

    #[test]
    fn noting_the_reverse_move_cancels_a_failed_warp() {
        let shared = Shared::new();
        shared.note_warp(Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        shared.note_warp(Point::new(10.0, 0.0), Point::new(10.0, 50.0));
        // The second warp failed: only the first one happened.
        shared.note_warp(Point::new(10.0, 50.0), Point::new(10.0, 0.0));
        assert_eq!(
            shared.take_warp(),
            Some(Warp {
                from: Point::new(0.0, 0.0),
                to: Point::new(10.0, 0.0),
            })
        );
    }
}
