//! Parking, hiding and restoring the cursor.
//!
//! Parking leaves the cursor where it is (at the edge it just crossed),
//! disconnects it from the mouse so it cannot move (the mouse's motion still
//! reaches the event tap as deltas), and hides it. It does not warp: macOS
//! adds a warp's distance to the delta of the next mouse event, and when
//! parking warped the cursor from the edge to the middle of the screen,
//! that jump went to the other computer as one big move back across the
//! edge (the "bounce" of the first two-laptop test). The cursor rests where
//! macOS stopped it at the edge, which is where the user's own motion took
//! it anyway; an auto-hidden Dock or menu bar on that edge therefore stays
//! shown while the other computer is used.
//!
//! Restoring reconnects the mouse, warps the cursor to where it comes back
//! and shows it. That warp's distance still reaches the next event, so it
//! is recorded first and capture takes it back out (see `convert`).
//!
//! Every step is recorded in [`CursorState`], so each hide is balanced by
//! exactly one show however parking ends: a restore, a lapsed suppression
//! lease, capture stopping, or the cursor control being dropped.

use std::sync::Arc;

use hop_platform::{CursorControl, PlatformError, Result};
use hop_types::Point;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDisplayHideCursor, CGDisplayShowCursor, CGError,
    CGEvent, CGMainDisplayID, CGWarpMouseCursorPosition,
};

use crate::displays::check;
use crate::ffi;
use crate::shared::{CursorState, Shared};

/// [`CursorControl`] for macOS.
pub(crate) struct MacCursor {
    shared: Arc<Shared>,
}

impl MacCursor {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

impl CursorControl for MacCursor {
    fn position(&self) -> Result<Point> {
        current_position()
    }

    fn park(&mut self) -> Result<()> {
        park(&self.shared)
    }

    fn restore(&mut self, at: Point) -> Result<()> {
        let mut state = self.shared.cursor();
        release(&self.shared, &mut state, at)
    }
}

impl Drop for MacCursor {
    /// Fail open: a cursor still parked when its control goes away is given
    /// back. Capture's `stop` does the same, but nothing guarantees capture
    /// outlives this, or that it ever ran to watch the lease.
    fn drop(&mut self) {
        unpark(&self.shared);
    }
}

/// Where the cursor is now, in Quartz global coordinates. A new, empty
/// event carries the current cursor location; it is read and never posted.
pub(crate) fn current_position() -> Result<Point> {
    let event = CGEvent::new(None)
        .ok_or_else(|| PlatformError::Os("could not read the cursor position".into()))?;
    let location = CGEvent::location(Some(&event));
    Ok(Point::new(location.x, location.y))
}

/// Parks and hides the cursor where it is. Parking twice is the same as
/// parking once: the position remembered is the one from before the first
/// park.
fn park(shared: &Shared) -> Result<()> {
    let mut state = shared.cursor();
    if state.parked_from.is_some() {
        return Ok(());
    }
    let from = current_position()?;
    // Without the disconnect the cursor would drift; then nothing is parked.
    attach(false)?;
    state.detached = true;
    state.parked_from = Some(from);
    // Hiding is best effort: a visible but pinned cursor still works.
    if !state.hidden && hide() {
        state.hidden = true;
    }
    Ok(())
}

/// Un-parks a parked cursor, putting it back where it was parked from.
/// Capture's `stop` and dropping the cursor control call this to fail
/// open; it does nothing if the cursor is not parked, detached or hidden.
pub(crate) fn unpark(shared: &Shared) {
    unpark_locked(shared, &mut shared.cursor());
}

/// Like [`unpark`], for the lease watchdog on the tap thread, which must
/// never wait: if another thread is changing the cursor right now (inside
/// a WindowServer call, which can be slow), it gives up at once and returns
/// `false`, and the watchdog tries again on its next tick. Waiting would
/// hold up every mouse and keyboard event on the Mac.
pub(crate) fn try_unpark(shared: &Shared) -> bool {
    match shared.try_cursor() {
        Some(mut state) => {
            unpark_locked(shared, &mut state);
            true
        }
        None => false,
    }
}

fn unpark_locked(shared: &Shared, state: &mut CursorState) {
    if state.parked_from.is_none() && !state.detached && !state.hidden {
        return;
    }
    let at = match state.parked_from {
        Some(from) => Some(from),
        None => current_position().ok(),
    };
    match at {
        Some(at) => {
            let _ = release(shared, state, at);
        }
        None => {
            // Not even the position is known: reconnect and show in place.
            if attach(true).is_ok() {
                state.detached = false;
            }
            show_if_hidden(state);
        }
    }
}

/// Reconnects the mouse, moves the cursor to `at` and shows it.
fn release(shared: &Shared, state: &mut CursorState, at: Point) -> Result<()> {
    // Reconnect first, so the mouse works again even if the warp fails.
    let attached = attach(true);
    let warped = warp(shared, at);
    // Reconnecting just after a warp also ends the short input pause that
    // macOS may still apply after it.
    let _ = attach(true);
    if attached.is_ok() {
        state.detached = false;
    }
    state.parked_from = None;
    show_if_hidden(state);
    attached.and(warped)
}

fn show_if_hidden(state: &mut CursorState) {
    // A failed show leaves `hidden` set, so the next restore tries again
    // instead of showing twice later.
    if state.hidden && CGDisplayShowCursor(CGMainDisplayID()) == CGError::Success {
        state.hidden = false;
    }
}

/// Hides the cursor. Returns whether it worked, so the caller knows a show
/// is owed.
fn hide() -> bool {
    // A background app's hide request is ignored unless its connection is
    // allowed to set the cursor while in the background. If that private
    // switch is missing, hiding cannot work, so it is skipped.
    if !ffi::allow_cursor_hiding_in_background() {
        return false;
    }
    // The display argument is ignored: macOS hides the cursor everywhere.
    CGDisplayHideCursor(CGMainDisplayID()) == CGError::Success
}

/// Moves the cursor without generating an event, and tells capture. It is
/// noted first: the next mouse event can carry the warp's distance as soon
/// as the warp happens, before this thread could note it afterwards.
fn warp(shared: &Shared, to: Point) -> Result<()> {
    ffi::allow_input_right_after_warps();
    // Unknown, the warp counts as no move: capture then compensates nothing.
    let from = current_position().unwrap_or(to);
    shared.note_warp(from, to);
    let warped = check(
        CGWarpMouseCursorPosition(CGPoint::new(to.x, to.y)),
        "CGWarpMouseCursorPosition",
    );
    if warped.is_err() {
        // It moved nothing: the reverse move cancels the note.
        shared.note_warp(to, from);
    }
    warped
}

/// Connects or disconnects mouse motion and the cursor position.
fn attach(attached: bool) -> Result<()> {
    check(
        CGAssociateMouseAndMouseCursorPosition(attached),
        "CGAssociateMouseAndMouseCursorPosition",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the cursor position from an event that is never posted.
    #[test]
    fn the_position_can_be_read_without_moving_anything() {
        let position = current_position().expect("cursor position");
        assert!(position.x.is_finite() && position.y.is_finite());
    }

    #[test]
    fn un_parking_a_cursor_that_was_never_parked_does_nothing() {
        // Nothing is parked, detached or hidden, so this returns before any
        // call that could move or show the cursor.
        let shared = Shared::new();
        unpark(&shared);
        let state = shared.cursor();
        assert!(state.parked_from.is_none() && !state.detached && !state.hidden);
        assert_eq!(shared.take_warp(), None, "no warp happened");
    }

    #[test]
    fn the_watchdog_never_waits_for_a_cursor_in_use() {
        // Nothing is parked, so neither call below touches the real cursor.
        let shared = Shared::new();
        let in_use = shared.cursor();
        assert!(!try_unpark(&shared), "gives up instead of waiting");
        drop(in_use);
        assert!(
            try_unpark(&shared),
            "free: it looks, and finds nothing to undo"
        );
        assert_eq!(shared.take_warp(), None, "no warp happened");
    }
}
