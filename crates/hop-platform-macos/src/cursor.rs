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
//! Concealing hides the cursor where it is and nothing else: a peer's
//! session left it at this computer's edge, and the mouse it follows is
//! still this computer's own. The event tap shows it again at the first
//! physical mouse event (see `capture`), and so does everything that
//! un-parks.
//!
//! Every step is recorded in [`CursorState`], so each hide is balanced by
//! exactly one show however parking or concealing ends: a restore, a
//! reveal, a lapsed suppression lease, capture stopping, or the cursor
//! control being dropped. Parking and concealing share the one `hidden`
//! flag, so the cursor is hidden at most once, whatever the order. Every
//! change to the real cursor goes through [`CursorCalls`].

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

    /// Hides the cursor where it is. A parked cursor is hidden already, and
    /// its own restore shows it, so concealing it changes nothing.
    fn conceal(&mut self) -> Result<()> {
        let mut state = self.shared.cursor();
        conceal(&self.shared, &mut state)
    }

    fn reveal(&mut self) -> Result<()> {
        let mut state = self.shared.cursor();
        reveal(&self.shared, &mut state)
    }
}

impl Drop for MacCursor {
    /// Fail open: a cursor still parked or concealed when its control goes
    /// away is given back. Capture's `stop` does the same, but nothing
    /// guarantees capture outlives this, or that it ever ran to watch the
    /// lease.
    fn drop(&mut self) {
        unpark(&self.shared);
    }
}

/// The WindowServer calls behind parking, concealing and restoring. Every
/// change to the cursor goes through them, so tests can stand in for the
/// WindowServer and never freeze, move or hide the real cursor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CursorCalls {
    /// Where the cursor is now.
    pub(crate) position: fn() -> Result<Point>,
    /// Connects (`true`) or disconnects mouse motion and the cursor.
    pub(crate) attach: fn(bool) -> Result<()>,
    /// Moves the cursor without generating an event.
    pub(crate) warp: fn(Point) -> Result<()>,
    /// Hides the cursor, and says whether it worked, so the caller knows a
    /// show is owed.
    pub(crate) hide: fn() -> bool,
    /// Shows the cursor again, and says whether it worked.
    pub(crate) show: fn() -> bool,
}

impl CursorCalls {
    /// The WindowServer's own.
    pub(crate) const REAL: CursorCalls = CursorCalls {
        position: current_position,
        attach,
        warp: warp_cursor,
        hide,
        show,
    };
}

impl Default for CursorCalls {
    fn default() -> Self {
        Self::REAL
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
/// park. A concealed cursor is parked as it is: its hide becomes the park's.
fn park(shared: &Shared) -> Result<()> {
    let mut state = shared.cursor();
    if state.parked_from.is_some() {
        return Ok(());
    }
    let calls = shared.calls();
    let from = (calls.position)()?;
    // Without the disconnect the cursor would drift; then nothing is parked.
    (calls.attach)(false)?;
    state.detached = true;
    state.parked_from = Some(from);
    // Hiding is best effort: a visible but pinned cursor still works.
    if !state.hidden && (calls.hide)() {
        state.hidden = true;
    }
    note_concealed(shared, &mut state);
    Ok(())
}

/// Hides the cursor where it is, unless it is parked (or still held from a
/// park whose reconnect failed): that cursor is hidden already, or is given
/// back by its own un-park.
fn conceal(shared: &Shared, state: &mut CursorState) -> Result<()> {
    if state.parked_from.is_some() || state.detached {
        return Ok(());
    }
    if !state.hidden {
        if !(shared.calls().hide)() {
            return Err(PlatformError::Os(
                "could not hide the cursor (CGDisplayHideCursor failed, or the WindowServer \
                 switch that lets a background app hide it is missing)"
                    .into(),
            ));
        }
        state.hidden = true;
    }
    note_concealed(shared, state);
    Ok(())
}

/// Shows a concealed cursor again where it is. Does nothing unless the
/// cursor is concealed: a parked cursor stays hidden until its own un-park.
/// A show that fails leaves the cursor concealed, so the next physical
/// mouse event tries again.
fn reveal(shared: &Shared, state: &mut CursorState) -> Result<()> {
    if !state.concealed || state.parked_from.is_some() || state.detached {
        return Ok(());
    }
    show_if_hidden(shared, state);
    note_concealed(shared, state);
    if state.hidden {
        return Err(PlatformError::Os("CGDisplayShowCursor failed".into()));
    }
    Ok(())
}

/// Shows a concealed cursor again, for the event tap, which must never
/// wait: if another thread is changing the cursor right now (inside a
/// WindowServer call, which can be slow), it gives up at once and returns
/// `false`, and the tap tries again at its next event or the watchdog's
/// next tick. It returns `true` once it could look, whatever it found.
pub(crate) fn try_reveal(shared: &Shared) -> bool {
    match shared.try_cursor() {
        Some(mut state) => {
            let _ = reveal(shared, &mut state);
            true
        }
        None => false,
    }
}

/// Un-parks a parked cursor, putting it back where it was parked from, and
/// shows a concealed one where it is. Capture's `stop` and dropping the
/// cursor control call this to fail open; it does nothing if the cursor is
/// not parked, detached or hidden.
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
    if state.parked_from.is_none() && !state.detached {
        // Not parked: at most concealed. Show it where it is; there is
        // nothing to reconnect and nowhere to warp it.
        show_if_hidden(shared, state);
        note_concealed(shared, state);
        return;
    }
    let at = match state.parked_from {
        Some(from) => Some(from),
        None => (shared.calls().position)().ok(),
    };
    match at {
        Some(at) => {
            let _ = release(shared, state, at);
        }
        None => {
            // Not even the position is known: reconnect and show in place.
            if (shared.calls().attach)(true).is_ok() {
                state.detached = false;
            }
            show_if_hidden(shared, state);
            note_concealed(shared, state);
        }
    }
}

/// Reconnects the mouse, moves the cursor to `at` and shows it. This ends a
/// park and a conceal alike.
fn release(shared: &Shared, state: &mut CursorState, at: Point) -> Result<()> {
    let calls = shared.calls();
    // Reconnect first, so the mouse works again even if the warp fails.
    let attached = (calls.attach)(true);
    let warped = warp(shared, at);
    // Reconnecting just after a warp also ends the short input pause that
    // macOS may still apply after it.
    let _ = (calls.attach)(true);
    if attached.is_ok() {
        state.detached = false;
    }
    state.parked_from = None;
    show_if_hidden(shared, state);
    note_concealed(shared, state);
    attached.and(warped)
}

fn show_if_hidden(shared: &Shared, state: &mut CursorState) {
    // A failed show leaves `hidden` set, so the next restore or reveal
    // tries again instead of showing twice later.
    if state.hidden && (shared.calls().show)() {
        state.hidden = false;
    }
}

/// Brings `concealed`, and the copy the tap reads, up to date after a
/// change: the cursor counts as concealed while Hop hides it outside a
/// park, which is what the next physical mouse event must undo.
fn note_concealed(shared: &Shared, state: &mut CursorState) {
    let concealed = state.hidden && state.parked_from.is_none() && !state.detached;
    shared.set_concealed(state, concealed);
}

/// Hides the cursor: the WindowServer half of [`CursorCalls::hide`]. Nothing
/// else in this crate calls `CGDisplayHideCursor`.
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

/// Shows the cursor: the WindowServer half of [`CursorCalls::show`].
fn show() -> bool {
    CGDisplayShowCursor(CGMainDisplayID()) == CGError::Success
}

/// Moves the cursor without generating an event, and tells capture. It is
/// noted first: the next mouse event can carry the warp's distance as soon
/// as the warp happens, before this thread could note it afterwards.
fn warp(shared: &Shared, to: Point) -> Result<()> {
    let calls = shared.calls();
    // Unknown, the warp counts as no move: capture then compensates nothing.
    let from = (calls.position)().unwrap_or(to);
    shared.note_warp(from, to);
    let warped = (calls.warp)(to);
    if warped.is_err() {
        // It moved nothing: the reverse move cancels the note.
        shared.note_warp(to, from);
    }
    warped
}

/// The WindowServer half of [`CursorCalls::warp`].
fn warp_cursor(to: Point) -> Result<()> {
    ffi::allow_input_right_after_warps();
    check(
        CGWarpMouseCursorPosition(CGPoint::new(to.x, to.y)),
        "CGWarpMouseCursorPosition",
    )
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
    //! Apart from reading the position, nothing here touches the real
    //! cursor: the tests that park, conceal and restore swap the
    //! WindowServer calls for a pretend WindowServer that only counts them.

    use std::cell::RefCell;

    use super::*;
    use crate::shared::Warp;

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

    /// What the pretend WindowServer has seen, on this test's thread.
    #[derive(Debug)]
    struct Server {
        position: Point,
        attached: bool,
        hides: u32,
        shows: u32,
        warps: Vec<Point>,
        /// Whether hiding works (the private switch is there).
        hide_works: bool,
        /// Whether showing works.
        show_works: bool,
    }

    impl Default for Server {
        fn default() -> Self {
            Server {
                position: Point::new(1511.0, 300.0),
                attached: true,
                hides: 0,
                shows: 0,
                warps: Vec::new(),
                hide_works: true,
                show_works: true,
            }
        }
    }

    thread_local! {
        static SERVER: RefCell<Server> = RefCell::new(Server::default());
    }

    fn server<R>(f: impl FnOnce(&mut Server) -> R) -> R {
        SERVER.with_borrow_mut(f)
    }

    /// Calls that change only the pretend WindowServer.
    const PRETEND: CursorCalls = CursorCalls {
        position: || Ok(server(|s| s.position)),
        attach: |attached| {
            server(|s| s.attached = attached);
            Ok(())
        },
        warp: |to| {
            server(|s| {
                s.position = to;
                s.warps.push(to);
            });
            Ok(())
        },
        hide: || {
            server(|s| {
                if s.hide_works {
                    s.hides += 1;
                }
                s.hide_works
            })
        },
        show: || {
            server(|s| {
                if s.show_works {
                    s.shows += 1;
                }
                s.show_works
            })
        },
    };

    /// A cursor control on the pretend WindowServer, and its shared state.
    fn pretend_cursor() -> (MacCursor, Arc<Shared>) {
        let shared = Shared::with_calls(PRETEND);
        (MacCursor::new(Arc::clone(&shared)), shared)
    }

    /// Hides not yet balanced by a show, as macOS counts them.
    fn hidden_count() -> i64 {
        server(|s| i64::from(s.hides) - i64::from(s.shows))
    }

    /// What must hold after every step, whatever came before.
    fn check_invariants(shared: &Shared, after: &str) {
        let state = shared.cursor();
        let owed = hidden_count();
        assert!(owed == 0 || owed == 1, "{after}: {owed} hides owed a show");
        assert_eq!(owed == 1, state.hidden, "{after}: the flag tracks the hide");
        assert_eq!(
            state.concealed,
            state.hidden && state.parked_from.is_none() && !state.detached,
            "{after}: concealed means hidden outside a park"
        );
        assert_eq!(
            shared.concealed(),
            state.concealed,
            "{after}: the tap's copy"
        );
        assert_eq!(
            server(|s| s.attached),
            !state.detached,
            "{after}: detached tracks the mouse"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Park,
        Restore,
        Conceal,
        Reveal,
        /// The event tap's own reveal, at a physical mouse event.
        TapReveal,
        /// Capture stopping, or a lapsed lease.
        Unpark,
    }

    const STEPS: [Step; 6] = [
        Step::Park,
        Step::Restore,
        Step::Conceal,
        Step::Reveal,
        Step::TapReveal,
        Step::Unpark,
    ];

    fn run(cursor: &mut MacCursor, shared: &Shared, step: Step) {
        match step {
            Step::Park => cursor.park().expect("park"),
            Step::Restore => cursor.restore(Point::new(1511.0, 420.0)).expect("restore"),
            Step::Conceal => cursor.conceal().expect("conceal"),
            Step::Reveal => cursor.reveal().expect("reveal"),
            Step::TapReveal => assert!(try_reveal(shared)),
            Step::Unpark => unpark(shared),
        }
    }

    #[test]
    fn hides_and_shows_stay_balanced_in_every_order() {
        // Every sequence of four steps, each followed by the checks, then
        // the cursor control dropped: every hide must have had its show.
        let mut sequences = vec![Vec::new()];
        for _ in 0..4 {
            sequences = sequences
                .into_iter()
                .flat_map(|done: Vec<Step>| {
                    STEPS.iter().map(move |&step| {
                        let mut next = done.clone();
                        next.push(step);
                        next
                    })
                })
                .collect();
        }
        assert_eq!(sequences.len(), 6usize.pow(4));
        for steps in sequences {
            server(|s| *s = Server::default());
            let (mut cursor, shared) = pretend_cursor();
            for (i, &step) in steps.iter().enumerate() {
                run(&mut cursor, &shared, step);
                check_invariants(&shared, &format!("{:?}", &steps[..=i]));
            }
            drop(cursor);
            assert_eq!(hidden_count(), 0, "{steps:?}, then dropped: a hide is left");
            assert!(
                server(|s| s.attached),
                "{steps:?}, then dropped: still detached"
            );
            let state = shared.cursor();
            assert!(
                state.parked_from.is_none() && !state.hidden && !state.concealed,
                "{steps:?}: {state:?}"
            );
        }
    }

    #[test]
    fn a_conceal_hides_in_place_and_a_reveal_shows_in_place() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.conceal().expect("conceal");
        assert_eq!(hidden_count(), 1);
        assert!(shared.concealed());
        // Concealing again hides nothing more.
        cursor.conceal().expect("conceal");
        assert_eq!(server(|s| s.hides), 1);
        cursor.reveal().expect("reveal");
        assert_eq!(hidden_count(), 0);
        assert!(!shared.concealed());
        // Nothing moved, froze or warped.
        assert!(server(|s| s.attached && s.warps.is_empty()));
        assert_eq!(server(|s| s.position), Point::new(1511.0, 300.0));
        assert_eq!(shared.take_warp(), None);
        // Revealing a cursor that is not concealed shows nothing.
        cursor.reveal().expect("reveal");
        assert_eq!(server(|s| s.shows), 1);
    }

    #[test]
    fn a_failed_hide_leaves_nothing_concealed() {
        let (mut cursor, shared) = pretend_cursor();
        server(|s| s.hide_works = false);
        assert!(cursor.conceal().is_err());
        let state = shared.cursor();
        assert!(!state.concealed && !state.hidden);
        assert!(!shared.concealed());
        drop(state);
        // So nothing is owed: dropping the control shows nothing.
        drop(cursor);
        assert_eq!(server(|s| (s.hides, s.shows)), (0, 0));
    }

    #[test]
    fn revealing_a_parked_cursor_does_nothing() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.park().expect("park");
        assert_eq!(hidden_count(), 1);
        // A conceal while parked changes nothing either.
        cursor.conceal().expect("conceal");
        cursor.reveal().expect("reveal");
        assert!(try_reveal(&shared));
        assert_eq!(server(|s| (s.hides, s.shows)), (1, 0), "still hidden once");
        assert!(!server(|s| s.attached), "still parked");
        assert!(!shared.concealed());
        // The restore shows it, once.
        cursor.restore(Point::new(1511.0, 420.0)).expect("restore");
        assert_eq!(server(|s| (s.hides, s.shows)), (1, 1));
    }

    #[test]
    fn parking_a_concealed_cursor_takes_its_hide_over() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.conceal().expect("conceal");
        cursor.park().expect("park");
        assert_eq!(server(|s| s.hides), 1, "no second hide");
        assert!(!shared.concealed(), "parking ends the conceal");
        // A physical mouse event while parked shows nothing.
        assert!(try_reveal(&shared));
        assert_eq!(server(|s| s.shows), 0);
        cursor.restore(Point::new(1511.0, 420.0)).expect("restore");
        assert_eq!(server(|s| (s.hides, s.shows)), (1, 1));
        assert!(!shared.concealed(), "restoring ends it too");
    }

    #[test]
    fn un_parking_shows_a_concealed_cursor_without_moving_it() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.conceal().expect("conceal");
        unpark(&shared);
        assert_eq!(server(|s| (s.hides, s.shows)), (1, 1));
        assert!(server(|s| s.warps.is_empty()), "no warp");
        assert_eq!(shared.take_warp(), None);
        assert!(!shared.concealed());
        drop(cursor);
        assert_eq!(server(|s| s.shows), 1, "and only once");
    }

    #[test]
    fn dropping_the_control_shows_a_concealed_cursor() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.conceal().expect("conceal");
        drop(cursor);
        assert_eq!(hidden_count(), 0);
        assert!(!shared.concealed());
    }

    #[test]
    fn a_failed_show_stays_owed_until_one_works() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.conceal().expect("conceal");
        server(|s| s.show_works = false);
        assert!(cursor.reveal().is_err());
        assert!(shared.concealed(), "the next mouse event tries again");
        assert!(try_reveal(&shared));
        assert!(shared.concealed());
        server(|s| s.show_works = true);
        assert!(try_reveal(&shared));
        assert!(!shared.concealed());
        assert_eq!(hidden_count(), 0);
    }

    #[test]
    fn the_tap_never_waits_to_reveal() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.conceal().expect("conceal");
        let in_use = shared.cursor();
        assert!(!try_reveal(&shared), "gives up instead of waiting");
        drop(in_use);
        assert_eq!(hidden_count(), 1, "still hidden: the reveal is owed");
        assert!(try_reveal(&shared));
        assert_eq!(hidden_count(), 0);
    }

    #[test]
    fn restoring_warps_and_tells_capture() {
        let (mut cursor, shared) = pretend_cursor();
        cursor.park().expect("park");
        cursor.restore(Point::new(1511.0, 700.0)).expect("restore");
        assert_eq!(server(|s| s.warps.clone()), [Point::new(1511.0, 700.0)]);
        assert_eq!(
            shared.take_warp(),
            Some(Warp {
                from: Point::new(1511.0, 300.0),
                to: Point::new(1511.0, 700.0),
            })
        );
    }
}
