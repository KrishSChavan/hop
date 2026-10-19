//! Parking and restoring the cursor while this PC controls another device,
//! and veiling the cursor another device's session left behind.
//!
//! Parking moves the cursor to the middle of the primary display, puts a
//! tiny, nearly invisible, topmost window under it whose class cursor is
//! transparent (so the cursor looks hidden), and clips the cursor to a box
//! around the middle (so it cannot drift toward an edge even if something
//! moves it). While parked, the hooks swallow physical motion, lease or not
//! (see `logic::swallow`), so the cursor does not actually move while its
//! motion is still reported; and the watchdog re-applies the clip, the
//! window and the position every tick, since Windows and other apps can
//! undo them.
//!
//! A parked cursor is given back by itself when the suppression lease lapses,
//! when capture stops, and when no lease has covered it for
//! `logic::park::PARKED_WITHOUT_LEASE`.
//!
//! # The veil
//!
//! When the other device's cursor leaves this PC through an edge, the PC's
//! cursor stays where the last injected move put it. Concealing it puts the
//! same invisible window around it, where it is, and nothing else: no warp,
//! no clip, nothing swallowed (the rules are in `logic::veil`). The mouse
//! hook lifts the veil at the first physical mouse event, inside the hook,
//! before Windows routes that event, so a click or a turn of the wheel goes
//! to the window under the cursor rather than to the veil. Parking,
//! restoring, a lapse, stopping capture, a panic and dropping the cursor
//! control lift it too, and so does the watchdog once the cursor is no
//! longer under it or no mouse hook would see the user touch the mouse.
//!
//! `SetSystemCursor` is never used: it changes the cursor for the whole
//! session, and if Hop crashed the user would be left without one. A
//! window's class cursor is gone with the window, and the window is gone
//! with the process.
//!
//! The window belongs to Hop's input thread, which pumps its messages, so
//! all of this runs there.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use hop_platform::{CursorControl, PlatformError, Result};
use hop_types::Point;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{HBRUSH, ValidateRect};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, CreateCursor, CreateWindowExW, DefWindowProcW, GetSystemMetrics, HCURSOR,
    HWND_TOPMOST, IDC_ARROW, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, SM_CXCURSOR, SM_CYCURSOR,
    SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetCursor, SetCursorPos, SetLayeredWindowAttributes, SetWindowPos, WM_ERASEBKGND,
    WM_MOUSEACTIVATE, WM_PAINT, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::logic::coords::{PhysPoint, PhysRect, to_logical, to_physical};
use crate::logic::park::{OVERLAY_SIZE, ParkSpot, ParkWatch, park_spot};
use crate::logic::veil::{self, Veil, VeilChange};
use crate::{engine, hooks, os, shared};

const OVERLAY_CLASS: PCWSTR = w!("HopParkedCursor");

/// The contract's cursor control.
#[derive(Debug, Default)]
pub(crate) struct WinCursor {
    /// This handle parked the cursor and has not restored it.
    parked: bool,
    /// This handle veiled the cursor, and has not revealed, parked or
    /// restored it since.
    concealed: bool,
}

impl CursorControl for WinCursor {
    fn position(&self) -> Result<Point> {
        Ok(to_logical(os::cursor_pos()?, os::primary_scale()))
    }

    fn park(&mut self) -> Result<()> {
        // Whatever the outcome, the cursor may be parked now: a park that
        // fails before changing anything leaves an earlier park in place,
        // and one the input thread did not answer in time may still happen.
        // So dropping this handle always checks; un-parking a cursor that is
        // not parked does nothing.
        self.parked = true;
        // Parking ends a conceal: the park takes the veil's window over.
        self.concealed = false;
        engine::run(park)?
    }

    fn restore(&mut self, at: Point) -> Result<()> {
        let target = to_physical(at, os::primary_scale());
        self.parked = false;
        self.concealed = false;
        engine::run(move || restore(target))?
    }

    fn conceal(&mut self) -> Result<()> {
        // As with parking, a veil the input thread did not put up in time
        // may still go up, so dropping this handle checks either way.
        self.concealed = true;
        engine::run(put_up_veil)?
    }

    fn reveal(&mut self) -> Result<()> {
        // Only a veil this handle put up can be there to lift; the mouse
        // hook, parking and the watchdog lift it without this handle
        // knowing, which leaves nothing to do here.
        if !std::mem::take(&mut self.concealed) {
            return Ok(());
        }
        engine::run(lift_veil)
    }
}

impl Drop for WinCursor {
    fn drop(&mut self) {
        if self.parked || self.concealed {
            // Show the cursor: back where it was before parking, or where it
            // is if it was veiled.
            let _ = engine::run(unpark_after_lapse);
        }
    }
}

/// A parked cursor.
#[derive(Clone, Copy, Debug)]
struct Parked {
    /// Where the cursor was before parking; it returns there if suppression
    /// lapses.
    home: PhysPoint,
    spot: ParkSpot,
}

#[derive(Default)]
struct ParkState {
    /// The hiding window, created on first park (or veil) and then reused.
    overlay: Option<HWND>,
    parked: Option<Parked>,
    /// Gives back a park that no suppression lease covers.
    watch: ParkWatch,
    /// The veil over a cursor another device's session left behind. Never
    /// together with `parked`: both use the one window.
    veil: Option<Veil>,
}

thread_local! {
    /// On the input thread only.
    static PARK: RefCell<ParkState> = RefCell::new(ParkState::default());
    /// Set while the input thread moves or clips the cursor itself (see
    /// [`moving_cursor`]).
    static MOVING: Cell<bool> = const { Cell::new(false) };
}

fn with_park<T>(f: impl FnOnce(&mut ParkState) -> T) -> Option<T> {
    PARK.try_with(|cell| cell.try_borrow_mut().ok().map(|mut state| f(&mut state)))
        .ok()
        .flatten()
}

/// Whether the cursor is parked, for the mouse hook, which pins a parked
/// cursor. Input thread.
pub(crate) fn is_parked() -> bool {
    with_park(|state| state.parked.is_some()).unwrap_or(false)
}

/// Whether the input thread is inside its own `SetCursorPos` or
/// `ClipCursor` call right now.
///
/// Windows does not document whether those calls run the low-level mouse
/// hook for the move they cause. If they do, the hook runs inside the call,
/// on this thread, and the event is Hop's own move, not the user's: the hook
/// must let it through without reporting it, or it would report a jump
/// across the screen as motion, and might swallow the warp itself.
pub(crate) fn moving_cursor() -> bool {
    MOVING.try_with(Cell::get).unwrap_or(false)
}

/// Runs `f` (a `SetCursorPos` or `ClipCursor` call) with [`moving_cursor`]
/// true. The flag is cleared even if `f` unwinds.
fn moving<T>(f: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            let _ = MOVING.try_with(|moving| moving.set(false));
        }
    }
    let _ = MOVING.try_with(|moving| moving.set(true));
    let _clear = Clear;
    f()
}

/// Parks the cursor. Input thread.
fn park() -> Result<()> {
    // A veiled cursor is hidden by the same window, which parking moves to
    // the park spot: the veil simply ends.
    with_park(|state| state.veil = None);
    let monitors = os::monitors()?;
    let primary = monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or(monitors.first())
        .ok_or_else(|| {
            PlatformError::Os("Windows reports no display to park the cursor on".into())
        })?;
    let spot = park_spot(primary.rect)
        .ok_or_else(|| PlatformError::Os("the primary display has no area".into()))?;
    // Parking again keeps the original home, so a lapse still returns the
    // cursor to where the user left it.
    let home = match with_park(|state| state.parked.map(|parked| parked.home)).flatten() {
        Some(home) => home,
        None => os::cursor_pos()?,
    };
    let result = (|| {
        let overlay = overlay()?;
        show_overlay(overlay, spot.overlay)?;
        warp(spot.center)?;
        show_hidden_shape();
        clip(Some(spot.clip))
    })();
    match result {
        Ok(()) => {
            with_park(|state| {
                state.parked = Some(Parked { home, spot });
                state.watch.parked();
            });
            Ok(())
        }
        Err(error) => {
            // Half-parked is worse than not parked: undo what took effect,
            // including the move to the middle of the screen.
            let _ = clip(None);
            show_arrow_shape();
            hide_overlay();
            with_park(|state| state.parked = None);
            if os::cursor_pos().is_ok_and(|now| now != home) {
                let _ = warp(home);
            }
            Err(error)
        }
    }
}

/// Shows the cursor again at `at` (physical pixels). Input thread.
fn restore(at: PhysPoint) -> Result<()> {
    with_park(|state| {
        state.parked = None;
        state.veil = None;
    });
    let unclipped = clip(None);
    show_arrow_shape();
    hide_overlay();
    let warped = warp(at);
    unclipped.and(warped)
}

/// Suppression lapsed, capture stopped, or the cursor handle went away:
/// lift a veil, and show and free a parked cursor where it was before
/// parking. Input thread.
pub(crate) fn unpark_after_lapse() {
    lift_veil();
    let Some(parked) = with_park(|state| state.parked.take()).flatten() else {
        return;
    };
    let _ = clip(None);
    show_arrow_shape();
    hide_overlay();
    let _ = warp(parked.home);
}

/// The watchdog's check for a park that no suppression lease has covered
/// for `PARKED_WITHOUT_LEASE`: gives the cursor back as a lapse would.
/// Input thread.
pub(crate) fn release_if_orphaned(now: Instant) {
    let leased = shared::suppressing(now);
    let orphaned = with_park(|state| {
        let parked = state.parked.is_some();
        state.watch.check(parked, leased, now)
    });
    if orphaned == Some(true) {
        unpark_after_lapse();
    }
}

/// After a panic: unclip and unhide, without touching anything that might
/// panic again. Input thread.
pub(crate) fn release_after_panic() {
    let _ = clip(None);
    show_arrow_shape();
    hide_overlay();
    with_park(|state| {
        state.parked = None;
        state.veil = None;
    });
}

/// Veils the cursor where it is (see the module documentation). A parked
/// cursor is hidden already, and restoring it shows it. Input thread.
fn put_up_veil() -> Result<()> {
    if is_parked() {
        return Ok(());
    }
    // Without the mouse hook nothing would see the user touch the mouse,
    // and the watchdog would only lift the veil again.
    if !hooks::mouse_hooked() {
        return Err(PlatformError::Os(
            "the cursor is hidden only while input capture runs, so that the mouse shows it again"
                .into(),
        ));
    }
    let veil = Veil::new(os::cursor_pos()?, Instant::now());
    if let Err(error) = overlay().and_then(|overlay| show_overlay(overlay, veil.rect)) {
        // Half-veiled is worse than not veiled.
        with_park(|state| state.veil = None);
        hide_overlay();
        return Err(error);
    }
    show_hidden_shape();
    with_park(|state| state.veil = Some(veil));
    Ok(())
}

/// Lifts the veil, if there is one: the cursor shows where it is. Input
/// thread, including inside the mouse hook, so it never waits.
pub(crate) fn lift_veil() {
    if with_park(|state| state.veil.take()).flatten().is_none() {
        return;
    }
    // While the cursor is still over the veil this thread owns its shape:
    // an arrow first, in case the window it lands on never sets one.
    show_arrow_shape();
    hide_overlay();
}

/// The mouse hook's part in the veil (see `logic::veil::on_mouse_event`):
/// a physical event lifts it at once, before Windows routes the event,
/// and the last moves of the session that just ended move it along.
/// `moved_to` is where a move takes the cursor; `None` for a button or the
/// wheel. Input thread, inside the hook.
pub(crate) fn veil_on_mouse(physical: bool, moved_to: Option<PhysPoint>, now: Instant) {
    let change = with_park(|state| {
        state
            .veil
            .map(|veil| veil::on_mouse_event(&veil, physical, moved_to, now))
    })
    .flatten();
    match change {
        Some(VeilChange::Lift) => lift_veil(),
        Some(VeilChange::Follow(rect)) => {
            let Some(Some(overlay)) = with_park(|state| state.overlay) else {
                return;
            };
            if show_overlay(overlay, rect).is_ok() {
                with_park(|state| {
                    if let Some(veil) = state.veil.as_mut() {
                        veil.rect = rect;
                    }
                });
            }
        }
        Some(VeilChange::Keep) | None => {}
    }
}

/// The watchdog's check of a veiled cursor, every tick: lift the veil once
/// the cursor is no longer under it or no mouse hook would see the user
/// touch the mouse (see `logic::veil::watchdog_lifts`); otherwise put it
/// back on top, in case another topmost window covered it. Input thread.
pub(crate) fn check_veil(now: Instant) {
    let Some(Some(veil)) = with_park(|state| state.veil) else {
        return;
    };
    if veil::watchdog_lifts(&veil, os::cursor_pos().ok(), hooks::mouse_hooked(), now) {
        lift_veil();
    } else if let Some(Some(overlay)) = with_park(|state| state.overlay) {
        let _ = show_overlay(overlay, veil.rect);
    }
}

/// The watchdog's tick while parked: re-apply what Windows or other apps may
/// have undone. Windows drops the clip on some events (a desktop switch,
/// another app's `ClipCursor`), and topmost windows can cover the overlay.
/// Input thread.
pub(crate) fn reassert() {
    let Some(Some(parked)) = with_park(|state| state.parked) else {
        return;
    };
    let _ = clip(Some(parked.spot.clip));
    if let Some(Some(overlay)) = with_park(|state| state.overlay) {
        let _ = show_overlay(overlay, parked.spot.overlay);
    }
    if os::cursor_pos().is_ok_and(|now| now != parked.spot.center) {
        let _ = warp(parked.spot.center);
    }
    show_hidden_shape();
}

/// Makes the parked or veiled cursor invisible right away. While the cursor
/// is over the overlay, this thread owns the cursor's shape, so `SetCursor`
/// takes effect at once; the overlay's class cursor does the same, but only
/// when Windows next asks (`WM_SETCURSOR`, on a move), and parked motion is
/// swallowed.
fn show_hidden_shape() {
    if let Ok(cursor) = transparent_cursor() {
        // SAFETY: a cursor handle that lives for the whole process.
        unsafe { SetCursor(Some(cursor)) };
    }
}

/// A normal arrow, set just before the overlay goes away. The window the
/// cursor lands on normally sets its own shape at once; if its app is hung
/// and never does, the user still sees an arrow instead of nothing.
fn show_arrow_shape() {
    // SAFETY: IDC_ARROW is a shared system cursor; no instance needed.
    if let Ok(arrow) = unsafe { LoadCursorW(None, IDC_ARROW) } {
        // SAFETY: a system cursor, never destroyed.
        unsafe { SetCursor(Some(arrow)) };
    }
}

/// Moves the cursor and tells the hooks, which must treat the event that
/// may be in flight as stale.
fn warp(to: PhysPoint) -> Result<()> {
    let from = os::cursor_pos().unwrap_or(to);
    // SAFETY: plain call; Windows clamps the point onto the desktop.
    moving(|| unsafe { SetCursorPos(to.x, to.y) })
        .map_err(|error| os::os_error("SetCursorPos", &error))?;
    let landed = os::cursor_pos().unwrap_or(to);
    hooks::note_warp(from, landed);
    Ok(())
}

/// Clips the cursor to `area`, or frees it.
fn clip(area: Option<PhysRect>) -> Result<()> {
    let rect = area.map(|area| RECT {
        left: area.left,
        top: area.top,
        right: area.right,
        bottom: area.bottom,
    });
    // A clip can move the cursor into the rectangle, like SetCursorPos.
    // SAFETY: the pointer, if any, is to `rect`, which outlives the call.
    moving(|| unsafe { ClipCursor(rect.as_ref().map(|rect| rect as *const RECT)) })
        .map_err(|error| os::os_error("ClipCursor", &error))
}

/// The hiding window, created on first use.
fn overlay() -> Result<HWND> {
    if let Some(Some(overlay)) = with_park(|state| state.overlay) {
        return Ok(overlay);
    }
    os::register_class(
        OVERLAY_CLASS,
        Some(overlay_procedure),
        transparent_cursor()?,
        HBRUSH::default(),
    )?;
    let instance = os::module_instance()?;
    // SAFETY: the class was just registered with a 'static procedure. The
    // window has no parent, so it belongs to this (the input) thread, whose
    // message loop serves it. It is created hidden.
    let hwnd = unsafe {
        CreateWindowExW(
            // A tool window has no taskbar button; no-activate keeps the
            // user's focus where it is; layered allows the transparency.
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            OVERLAY_CLASS,
            w!("Hop"),
            WS_POPUP,
            0,
            0,
            OVERLAY_SIZE,
            OVERLAY_SIZE,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| os::os_error("CreateWindowExW (cursor overlay)", &error))?;
    // Alpha 1 of 255: invisible to the eye, but unlike a fully transparent
    // window it still takes part in hit-testing, so the cursor over it gets
    // the window's (transparent) class cursor. WS_EX_TRANSPARENT is avoided
    // for the same reason: it would let the cursor "see" the window below.
    // SAFETY: `hwnd` is the layered window just created on this thread.
    unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA) }
        .map_err(|error| os::os_error("SetLayeredWindowAttributes", &error))?;
    with_park(|state| state.overlay = Some(hwnd));
    Ok(hwnd)
}

fn show_overlay(overlay: HWND, area: PhysRect) -> Result<()> {
    // SAFETY: `overlay` is this thread's window.
    unsafe {
        SetWindowPos(
            overlay,
            Some(HWND_TOPMOST),
            area.left,
            area.top,
            area.width(),
            area.height(),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .map_err(|error| os::os_error("SetWindowPos (show cursor overlay)", &error))
}

fn hide_overlay() {
    let Some(Some(overlay)) = with_park(|state| state.overlay) else {
        return;
    };
    // SWP_HIDEWINDOW with SWP_NOACTIVATE, not ShowWindow(SW_HIDE), which
    // may activate another window.
    // SAFETY: `overlay` is this thread's window.
    let _ = unsafe {
        SetWindowPos(
            overlay,
            None,
            0,
            0,
            0,
            0,
            SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        )
    };
}

/// A fully transparent cursor, created once per process. Asked for at 1x1;
/// if Windows refuses that size, at the system cursor size.
fn transparent_cursor() -> Result<HCURSOR> {
    // Stored as a number: HCURSOR holds a pointer, so it is not Sync. The
    // cursor is never destroyed; it lives as long as the window class.
    static CURSOR: OnceLock<usize> = OnceLock::new();
    if let Some(&raw) = CURSOR.get() {
        return Ok(HCURSOR(raw as *mut c_void));
    }
    let instance = os::module_instance()?;
    let cursor = blank_cursor(instance, 1, 1).or_else(|_| {
        // SAFETY: plain calls.
        let (width, height) =
            unsafe { (GetSystemMetrics(SM_CXCURSOR), GetSystemMetrics(SM_CYCURSOR)) };
        blank_cursor(instance, width.max(1), height.max(1))
    })?;
    let raw = *CURSOR.get_or_init(|| cursor.0 as usize);
    Ok(HCURSOR(raw as *mut c_void))
}

fn blank_cursor(
    instance: windows::Win32::Foundation::HINSTANCE,
    width: i32,
    height: i32,
) -> Result<HCURSOR> {
    // Each mask row is padded to a 16-bit boundary. AND bits of 1 keep the
    // screen pixel and XOR bits of 0 change nothing: a cursor that draws
    // nothing at all.
    let row_bytes = usize::try_from(width).unwrap_or(1).div_ceil(16) * 2;
    let len = row_bytes * usize::try_from(height).unwrap_or(1);
    let and_mask = vec![0xFF_u8; len];
    let xor_mask = vec![0x00_u8; len];
    // SAFETY: both masks hold `height` rows of `row_bytes` bytes, exactly
    // what CreateCursor reads for a cursor of this size, and outlive the
    // call (Windows copies them).
    unsafe {
        CreateCursor(
            Some(instance),
            0,
            0,
            width,
            height,
            and_mask.as_ptr().cast::<c_void>(),
            xor_mask.as_ptr().cast::<c_void>(),
        )
    }
    .map_err(|error| os::os_error("CreateCursor", &error))
}

/// The hiding window's procedure: never activate, never paint.
unsafe extern "system" fn overlay_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = os::guarded(None, engine::fail_open, || match message {
        // A click on it (if one ever got through) must not take focus.
        WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
        // Nothing to draw: the window is invisible.
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_PAINT => {
            // SAFETY: `hwnd` is this window; marks it as painted.
            let _ = unsafe { ValidateRect(Some(hwnd), None) };
            Some(LRESULT(0))
        }
        _ => None,
    });
    match handled {
        Some(result) => result,
        // Everything else, including WM_SETCURSOR, which sets the class
        // cursor, goes to the default procedure.
        // SAFETY: the arguments are passed on unchanged.
        None => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
