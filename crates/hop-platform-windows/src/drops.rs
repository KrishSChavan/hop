//! The contract's drop catcher (milestone M5): files dropped on the edge
//! that leads to the other computer (docs/M4-M5-DESIGN.md, section 5.1).
//!
//! # Threads
//!
//! - The "hop-drop" thread, started with the first zones, is an OLE
//!   single-threaded apartment (`OleInitialize`) that pumps messages. It
//!   owns one window per zone (a topmost, never activated, layered tool
//!   window, so it can be invisible and still be found under the cursor),
//!   each registered with `RegisterDragDrop` and an `IDropTarget` of its
//!   own, and a message-only window that takes jobs and the mouse hook's
//!   arm and disarm messages. Drag and drop calls the targets on this
//!   thread; they are not agile, so COM never calls them anywhere else.
//! - The mouse hook, on Hop's input thread, follows the left button
//!   (`logic::drop_zone::DragGesture`) and posts "arm" and "disarm" to that
//!   window. It never waits: it only tries the lock in front of the zones,
//!   and posting returns at once.
//! - `take_drops` takes the dropped paths from a queue that the drop thread
//!   locks only to add one drop.
//!
//! The windows show only during a drag near their zone (the rules are in
//! `logic::drop_zone`): armed, they are invisible (alpha 1) but there for
//! drag and drop to find; with files over them they show the label; a drop
//! is always taken as a copy, never a move. While any window shows, a
//! timer hides it once no mouse button is held, so a missed button-up
//! cannot leave one at the edge to catch clicks. File names are never
//! logged.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use hop_platform::{DropCatcher, DropZone, PlatformError, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINTL, RECT, S_OK, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, CreateFontIndirectW, DT_CALCRECT, DT_CENTER,
    DT_NOPREFIX, DT_WORDBREAK, DeleteObject, DrawTextW, EndPaint, FW_SEMIBOLD, FillRect,
    GetSysColor, GetSysColorBrush, HBRUSH, HDC, HFONT, InvalidateRect, PAINTSTRUCT, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
    OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, HCURSOR, HWND_MESSAGE,
    HWND_TOPMOST, IDC_ARROW, KillTimer, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, NONCLIENTMETRICSW,
    PostMessageW, SPI_GETNONCLIENTMETRICS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DPICHANGED, WM_ERASEBKGND, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN,
    WM_TIMER, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, Ref, implement, w};

use crate::logic::coords::{BASE_DPI, PhysPoint, PhysRect, scale_from_dpi};
use crate::logic::drop_zone::{
    ARMED_ALPHA, DragGesture, Effect, HIGHLIGHT_ALPHA, Look, MAX_DROPPED_FILES, MAX_ZONES, Signal,
    TICK_MS, Target, ZoneSet, queue_drop, usable_zones, window_rect,
};
use crate::os;
use crate::worker::{self, WM_APP_WAKE, Worker};

const CONTROL_CLASS: PCWSTR = w!("HopDropThread");
const ZONE_CLASS: PCWSTR = w!("HopDropZone");

/// Posted by the mouse hook: show the zones in `wParam` (one bit each) of
/// the zone setting in `lParam`, armed.
const WM_APP_ARM: u32 = WM_APP + 0x20;
/// Posted by the mouse hook: the left button went up; `lParam` is the zone
/// setting.
const WM_APP_DISARM: u32 = WM_APP + 0x21;

const TICK_TIMER: usize = 1;

/// How long `set_zones` waits for the drop thread. Its jobs take
/// milliseconds; only a drag source that answers slowly can hold it up,
/// and the caller (the agent's core thread) must not stall for anywhere
/// near the input lease meanwhile. A job that misses this still runs: the
/// zones are then set a moment later.
const CALL_TIMEOUT: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// The space around the label, in 96-DPI pixels.
const LABEL_MARGIN: f64 = 12.0;

/// The drop thread, once started.
static DROPS: Mutex<Option<Worker>> = Mutex::new(None);

/// The drop thread's message-only window, as a number (see
/// `os::hwnd_to_raw`), for the mouse hook's posts; 0 until it starts.
static CONTROL: AtomicUsize = AtomicUsize::new(0);

/// The zones as the mouse hook checks them.
static ZONES: Mutex<ZoneSet> = Mutex::new(ZoneSet::EMPTY);

/// Numbers each setting of the zones.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Drops waiting for `take_drops`, oldest first.
static DROPPED: Mutex<VecDeque<Vec<PathBuf>>> = Mutex::new(VecDeque::new());

thread_local! {
    /// On the drop thread only.
    static STATE: RefCell<DropState> = RefCell::new(DropState::default());
    /// On Hop's input thread only: the left button's gesture, as the mouse
    /// hook follows it.
    static GESTURE: RefCell<DragGesture> = RefCell::new(DragGesture::default());
}

/// The zone windows and what they show. Drop thread.
#[derive(Default)]
struct DropState {
    /// The zone setting the windows belong to.
    generation: u64,
    /// The label, UTF-16 without a NUL.
    label: Vec<u16>,
    /// One window per zone, in the order the hook numbers them.
    zones: Vec<ZoneWindow>,
}

struct ZoneWindow {
    zone: DropZone,
    hwnd: HWND,
    target: Target,
}

fn with_state<T>(f: impl FnOnce(&mut DropState) -> T) -> Option<T> {
    STATE
        .try_with(|cell| cell.try_borrow_mut().ok().map(|mut state| f(&mut state)))
        .ok()
        .flatten()
}

/// The contract's drop catcher. Every handle shares the one drop thread and
/// the one set of zones; the last zones set win.
#[derive(Debug, Default)]
pub(crate) struct WinDrops {
    /// This handle set zones and has not cleared them.
    catching: bool,
}

impl DropCatcher for WinDrops {
    fn set_zones(&mut self, zones: Vec<DropZone>, label: &str) -> Result<()> {
        let zones = usable_zones(zones);
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        let set = ZoneSet::new(generation, &zones);
        self.catching = !zones.is_empty();
        let result = if zones.is_empty() {
            // Only a drop thread that is running has windows to take down.
            match running() {
                Some(worker) => worker.run(CALL_TIMEOUT, move || {
                    replace_zones(generation, Vec::new(), Vec::new())
                }),
                None => Ok(Ok(())),
            }
        } else {
            let label: Vec<u16> = label.encode_utf16().collect();
            worker().and_then(|worker| {
                worker.run(CALL_TIMEOUT, move || {
                    replace_zones(generation, zones, label)
                })
            })
        };
        // The hook arms by this setting from now on, even if the drop thread
        // was only slow: it ignores arms for a setting it does not have yet.
        *ZONES.lock().unwrap_or_else(PoisonError::into_inner) = set;
        result?
    }

    fn take_drops(&mut self) -> Vec<Vec<PathBuf>> {
        DROPPED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}

impl Drop for WinDrops {
    fn drop(&mut self) {
        if self.catching {
            let _ = self.set_zones(Vec::new(), "");
        }
    }
}

/// The drop thread, if it is running.
fn running() -> Option<Worker> {
    DROPS.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The drop thread, started if needed.
fn worker() -> Result<Worker> {
    let mut slot = DROPS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(worker) = slot.as_ref() {
        return Ok(worker.clone());
    }
    let worker = start()?;
    *slot = Some(worker.clone());
    Ok(worker)
}

fn start() -> Result<Worker> {
    let (ready, started) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("hop-drop".into())
        .spawn(move || main(&ready))
        .map_err(|error| {
            PlatformError::Os(format!("could not start Hop's drop thread: {error}"))
        })?;
    started
        .recv_timeout(START_TIMEOUT)
        .map_err(|_| PlatformError::Os("Hop's drop thread did not start".into()))?
        .map_err(PlatformError::Os)
}

/// The drop thread.
fn main(ready: &SyncSender<std::result::Result<Worker, String>>) {
    // SAFETY: this new thread's first COM call. It makes the thread a
    // single-threaded apartment, which drag and drop needs; balanced by
    // OleUninitialize below.
    if let Err(error) = unsafe { OleInitialize(None) } {
        let _ = ready.send(Err(format!("OleInitialize failed: {error}")));
        return;
    }
    match create_control_window() {
        Ok(control) => {
            let worker = Worker::install(control, "Hop's drop thread");
            CONTROL.store(os::hwnd_to_raw(control), Ordering::Release);
            // Nobody waiting any more means nobody will set zones here.
            if ready.send(Ok(worker)).is_ok() {
                worker::message_loop();
            }
            CONTROL.store(0, Ordering::Release);
            // The loop ends only if the thread is told to quit: take every
            // zone window down while OLE is still there to revoke it.
            if let Some(zones) = with_state(|state| std::mem::take(&mut state.zones)) {
                zones.into_iter().for_each(destroy_zone_window);
            }
        }
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
        }
    }
    // SAFETY: pairs with the OleInitialize above, on the same thread.
    unsafe { OleUninitialize() };
}

fn create_control_window() -> Result<HWND> {
    // SAFETY: IDC_ARROW is a shared system cursor; no instance needed.
    let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|error| os::os_error("LoadCursorW", &error))?;
    os::register_class(ZONE_CLASS, Some(zone_procedure), arrow, HBRUSH::default())?;
    os::register_class(
        CONTROL_CLASS,
        Some(control_procedure),
        HCURSOR::default(),
        HBRUSH::default(),
    )?;
    let instance = os::module_instance()?;
    // SAFETY: the class was just registered with a 'static procedure.
    // HWND_MESSAGE makes this a message-only window: never shown, only there
    // to receive messages.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CONTROL_CLASS,
            w!("Hop drop"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| os::os_error("CreateWindowExW (drop thread)", &error))
}

/// The drop thread's message-only window.
fn control_window() -> HWND {
    os::raw_to_hwnd(CONTROL.load(Ordering::Acquire))
}

/// Takes the old zone windows down and puts up one hidden window per zone
/// of setting `generation`. All or nothing: if one window cannot be made,
/// none is kept. The same zones and label again keep their windows, so a
/// drag over one goes on. Drop thread.
fn replace_zones(generation: u64, zones: Vec<DropZone>, label: Vec<u16>) -> Result<()> {
    let old = with_state(|state| {
        let same = state.label == label
            && state.zones.len() == zones.len()
            && state
                .zones
                .iter()
                .zip(&zones)
                .all(|(window, zone)| window.zone == *zone);
        state.generation = generation;
        state.label = label;
        (!same).then(|| std::mem::take(&mut state.zones))
    })
    .ok_or_else(|| PlatformError::Os("the drop thread is busy".into()))?;
    let Some(old) = old else {
        return Ok(());
    };
    old.into_iter().for_each(destroy_zone_window);
    stop_timer();
    let mut windows: Vec<ZoneWindow> = Vec::with_capacity(zones.len());
    for zone in zones {
        match create_zone_window() {
            Ok(hwnd) => windows.push(ZoneWindow {
                zone,
                hwnd,
                target: Target::default(),
            }),
            Err(error) => {
                windows.into_iter().for_each(destroy_zone_window);
                return Err(error);
            }
        }
    }
    let mut windows = Some(windows);
    with_state(|state| state.zones = windows.take().unwrap_or_default());
    if let Some(windows) = windows {
        windows.into_iter().for_each(destroy_zone_window);
        return Err(PlatformError::Os("the drop thread is busy".into()));
    }
    Ok(())
}

/// One zone's window, hidden, registered for drag and drop. Drop thread.
fn create_zone_window() -> Result<HWND> {
    let instance = os::module_instance()?;
    // SAFETY: the class was registered when the thread started, with a
    // 'static procedure. The window has no parent, so it belongs to this
    // thread, whose message loop serves it. It is created hidden.
    let hwnd = unsafe {
        CreateWindowExW(
            // A tool window has no taskbar button; no-activate keeps the
            // user's focus where it is, even when it is clicked; layered
            // allows the transparency. Never WS_EX_TRANSPARENT: drag and
            // drop must find the window under the cursor.
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            ZONE_CLASS,
            w!("Hop drop zone"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| os::os_error("CreateWindowExW (drop zone)", &error))?;
    let registered = (|| {
        // SAFETY: `hwnd` is the layered window just created on this thread.
        unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), ARMED_ALPHA, LWA_ALPHA) }
            .map_err(|error| os::os_error("SetLayeredWindowAttributes (drop zone)", &error))?;
        let target: IDropTarget = ZoneTarget {
            hwnd: os::hwnd_to_raw(hwnd),
        }
        .into();
        // SAFETY: `hwnd` is this thread's window, and this thread is an OLE
        // apartment. OLE keeps its own reference to the target until
        // RevokeDragDrop.
        unsafe { RegisterDragDrop(hwnd, &target) }
            .map_err(|error| os::os_error("RegisterDragDrop", &error))
    })();
    if let Err(error) = registered {
        // SAFETY: this thread's window, never shown.
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
    Ok(hwnd)
}

/// Revokes a zone window's drop target, then destroys the window. Drop
/// thread.
fn destroy_zone_window(zone: ZoneWindow) {
    // SAFETY: this thread's window; revoking one that is not registered
    // just fails.
    let _ = unsafe { RevokeDragDrop(zone.hwnd) };
    // SAFETY: this thread's window.
    let _ = unsafe { DestroyWindow(zone.hwnd) };
}

/// Makes zone window `hwnd` show `look`: hidden, or with the look's alpha,
/// and also put on top at `rect` when one is given (when it was hidden).
/// Drop thread.
fn show(hwnd: HWND, look: Look, rect: Option<PhysRect>) {
    let alpha = match look {
        Look::Hidden => {
            // SWP_HIDEWINDOW with SWP_NOACTIVATE, not ShowWindow(SW_HIDE),
            // which may activate another window.
            // SAFETY: this thread's window.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
                )
            };
            return;
        }
        Look::Armed => ARMED_ALPHA,
        Look::Highlighted => HIGHLIGHT_ALPHA,
    };
    // SAFETY: this thread's layered window.
    let _ = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) };
    if let Some(rect) = rect {
        // SAFETY: this thread's window; it is never activated.
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
    }
}

/// The hook's arm: show the zones in `zones` (one bit each), armed, where
/// they are now (the primary scale may have changed since they were set).
/// Drop thread.
fn arm(zones: u64, generation: u64) {
    let scale = os::primary_scale();
    let shows = with_state(|state| {
        if state.generation != generation {
            return Vec::new();
        }
        let mut shows = Vec::new();
        for (index, zone) in state.zones.iter_mut().enumerate().take(MAX_ZONES) {
            if zones & (1 << index) == 0 {
                continue;
            }
            let Some(rect) = window_rect(&zone.zone, scale) else {
                continue;
            };
            let hidden = zone.target.look() == Look::Hidden;
            zone.target.arm();
            if hidden {
                shows.push((zone.hwnd, rect));
            }
        }
        shows
    })
    .unwrap_or_default();
    for (hwnd, rect) in shows {
        show(hwnd, Look::Armed, Some(rect));
    }
    if any_shown() {
        start_timer();
    }
}

/// The hook's disarm: the left button went up, so the windows hide soon
/// (see `Target::disarm`). Drop thread.
fn disarm(generation: u64) {
    let now = Instant::now();
    with_state(|state| {
        if state.generation == generation {
            for zone in &mut state.zones {
                zone.target.disarm(now);
            }
        }
    });
}

/// The timer, every `TICK_MS` while a window shows: hide the windows that
/// are due (see `Target::tick`), and stop once none shows. Drop thread.
fn tick() {
    let now = Instant::now();
    let held = button_held();
    let hides = with_state(|state| {
        let mut hides = Vec::new();
        for zone in &mut state.zones {
            let shown = zone.target.look().shown();
            zone.target.tick(now, held);
            if shown && !zone.target.look().shown() {
                hides.push(zone.hwnd);
            }
        }
        hides
    })
    .unwrap_or_default();
    for hwnd in hides {
        show(hwnd, Look::Hidden, None);
    }
    if !any_shown() {
        stop_timer();
    }
}

fn any_shown() -> bool {
    with_state(|state| state.zones.iter().any(|zone| zone.target.look().shown())).unwrap_or(false)
}

fn start_timer() {
    // SAFETY: the drop thread's own window; no timer procedure, so the
    // timer arrives as WM_TIMER in `control_procedure`. Setting it again
    // only restarts it.
    unsafe { SetTimer(Some(control_window()), TICK_TIMER, TICK_MS, None) };
}

fn stop_timer() {
    // SAFETY: the drop thread's own window; a timer that is not running
    // just makes this fail.
    let _ = unsafe { KillTimer(Some(control_window()), TICK_TIMER) };
}

/// Whether a mouse button that can drag (left or right) is held now,
/// whatever the hooks saw.
fn button_held() -> bool {
    // SAFETY: plain calls; the high bit (a negative value) means "down".
    unsafe {
        GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0
            || GetAsyncKeyState(i32::from(VK_RBUTTON.0)) < 0
    }
}

/// After a panic on the drop thread: hide every zone window, so none is
/// left at the edge to catch clicks.
fn fail_open() {
    let windows = with_state(|state| {
        state
            .zones
            .iter_mut()
            .map(|zone| {
                zone.target = Target::default();
                zone.hwnd
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    for hwnd in windows {
        show(hwnd, Look::Hidden, None);
    }
}

/// Runs `f` with the target of zone window `hwnd`, then makes the window
/// show what the target shows now. `None` if the window is not (or no
/// longer) a zone's. Drop thread.
fn on_target<R>(hwnd: usize, f: impl FnOnce(&mut Target) -> R) -> Option<R> {
    let (result, before, after, window) = with_state(|state| {
        let zone = state
            .zones
            .iter_mut()
            .find(|zone| os::hwnd_to_raw(zone.hwnd) == hwnd)?;
        let before = zone.target.look();
        let result = f(&mut zone.target);
        Some((result, before, zone.target.look(), zone.hwnd))
    })
    .flatten()?;
    if before != after {
        show(window, after, None);
    }
    Some(result)
}

/// The control window's procedure: jobs, the hook's arm and disarm, the
/// timer.
unsafe extern "system" fn control_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = os::guarded(None, fail_open, || match message {
        WM_APP_WAKE => {
            worker::run_pending_jobs(fail_open);
            Some(LRESULT(0))
        }
        WM_APP_ARM => {
            arm(wparam.0 as u64, lparam.0 as u64);
            Some(LRESULT(0))
        }
        WM_APP_DISARM => {
            disarm(lparam.0 as u64);
            Some(LRESULT(0))
        }
        WM_TIMER if wparam.0 == TICK_TIMER => {
            tick();
            Some(LRESULT(0))
        }
        _ => None,
    });
    match handled {
        Some(result) => result,
        // SAFETY: the arguments are passed on unchanged.
        None => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// A zone window's procedure: never activate; paint the label.
unsafe extern "system" fn zone_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = os::guarded(None, fail_open, || match message {
        // A click on it (one that lands within the hide delay) must not
        // take focus.
        WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
        // That click is lost, but the window hides at once (see
        // `Target::clicked`), so the next one goes through.
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            on_target(os::hwnd_to_raw(hwnd), Target::clicked);
            Some(LRESULT(0))
        }
        // Everything is drawn in WM_PAINT.
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_PAINT => {
            paint(hwnd);
            Some(LRESULT(0))
        }
        // Moved onto a display with another scale: keep the size Hop gave
        // it, and draw the label again for the new scale.
        WM_DPICHANGED => {
            // SAFETY: this thread's window.
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
            Some(LRESULT(0))
        }
        _ => None,
    });
    match handled {
        Some(result) => result,
        // SAFETY: the arguments are passed on unchanged.
        None => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Paints a zone window: the system's highlight colour with the label in
/// the middle. An armed window's alpha makes it invisible all the same.
fn paint(hwnd: HWND) {
    let mut painting = PAINTSTRUCT::default();
    // SAFETY: `hwnd` is this thread's window, in its WM_PAINT; EndPaint
    // follows.
    let hdc = unsafe { BeginPaint(hwnd, &mut painting) };
    if !hdc.is_invalid() {
        draw_label(hwnd, hdc);
    }
    // SAFETY: pairs with BeginPaint above.
    let _ = unsafe { EndPaint(hwnd, &painting) };
}

fn draw_label(hwnd: HWND, hdc: HDC) {
    let mut area = RECT::default();
    // SAFETY: `area` is a valid out-pointer; the window is this thread's.
    if unsafe { GetClientRect(hwnd, &mut area) }.is_err() {
        return;
    }
    // SAFETY: `hdc` is the paint context; system colours and brushes are
    // never freed.
    unsafe {
        FillRect(hdc, &area, GetSysColorBrush(COLOR_HIGHLIGHT));
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(GetSysColor(COLOR_HIGHLIGHTTEXT)));
    }
    let mut label = with_state(|state| state.label.clone()).unwrap_or_default();
    if label.is_empty() {
        return;
    }
    // SAFETY: plain call on this thread's window; 0 if it fails.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => BASE_DPI,
        dpi => dpi,
    };
    let font = label_font(dpi);
    // SAFETY: `font` is a font this function created; the previous one is
    // put back before the font is deleted.
    let previous = font.map(|font| unsafe { SelectObject(hdc, font.into()) });
    let margin = (LABEL_MARGIN * scale_from_dpi(dpi)).round() as i32;
    let mut text = RECT {
        left: area.left + margin,
        top: area.top + margin,
        right: area.right - margin,
        bottom: area.bottom - margin,
    };
    let format = DT_CENTER | DT_WORDBREAK | DT_NOPREFIX;
    let mut measured = text;
    // SAFETY: `label` and `measured` are valid for the call; DT_CALCRECT
    // only measures, growing `measured` to fit the wrapped lines.
    unsafe { DrawTextW(hdc, &mut label, &mut measured, format | DT_CALCRECT) };
    // Wrapped text cannot be centred vertically by DrawTextW itself.
    let spare = (text.bottom - text.top) - (measured.bottom - measured.top);
    text.top += spare.max(0) / 2;
    // SAFETY: as above; this time it draws, clipped to the window.
    unsafe { DrawTextW(hdc, &mut label, &mut text, format) };
    if let (Some(font), Some(previous)) = (font, previous) {
        // SAFETY: the previous font goes back into the context first, so
        // the deleted font is no longer selected.
        unsafe {
            SelectObject(hdc, previous);
            let _ = DeleteObject(font.into());
        }
    }
}

/// The label's font: the system's message font at `dpi`, larger and
/// heavier so it reads at a glance.
fn label_font(dpi: u32) -> Option<HFONT> {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `metrics` is a NONCLIENTMETRICSW with cbSize set, and the
    // size passed is its size.
    unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast::<c_void>()),
            0,
            dpi,
        )
    }
    .ok()?;
    let mut font = metrics.lfMessageFont;
    font.lfHeight = font.lfHeight.saturating_mul(3) / 2;
    font.lfWeight = FW_SEMIBOLD.0 as i32;
    // SAFETY: a LOGFONTW filled in by Windows, then adjusted.
    let font = unsafe { CreateFontIndirectW(&font) };
    (!font.is_invalid()).then_some(font)
}

/// What a zone window answers drag and drop. Not agile, so COM calls it
/// only on the drop thread, where the window and its state live.
#[implement(IDropTarget, Agile = false)]
struct ZoneTarget {
    /// The zone's window, as a number (see `os::hwnd_to_raw`).
    hwnd: usize,
}

impl IDropTarget_Impl for ZoneTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _at: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let hwnd = self.hwnd;
        respond(effect, |copy_allowed| {
            let files = data.as_ref().is_some_and(holds_files);
            on_target(hwnd, |target| target.drag_enter(files, copy_allowed)).unwrap_or(Effect::None)
        });
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _at: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let hwnd = self.hwnd;
        respond(effect, |copy_allowed| {
            on_target(hwnd, |target| target.drag_over(copy_allowed)).unwrap_or(Effect::None)
        });
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        let hwnd = self.hwnd;
        os::guarded((), fail_open, || {
            on_target(hwnd, Target::drag_leave);
        });
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _at: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let hwnd = self.hwnd;
        respond(effect, |copy_allowed| {
            // Only a copy is ever taken, so a drop that cannot be one is not
            // even read.
            let paths = match data.as_ref() {
                Some(data) if copy_allowed => dropped_paths(data),
                _ => Vec::new(),
            };
            let answer = on_target(hwnd, |target| {
                target.drop_files(!paths.is_empty(), copy_allowed)
            })
            .unwrap_or(Effect::None);
            if answer == Effect::Copy {
                queue_drop(
                    &mut DROPPED.lock().unwrap_or_else(PoisonError::into_inner),
                    paths,
                );
            }
            answer
        });
        Ok(())
    }
}

/// Answers a drag-and-drop call through `effect`, its in/out effect:
/// reads what the source allows, leaves `DROPEFFECT_NONE` there while
/// `decide` runs (which is what stays if it panics), and writes what
/// `decide`, given whether a copy is allowed, returns. Drop thread.
fn respond(effect: *mut DROPEFFECT, decide: impl FnOnce(bool) -> Effect) {
    let allowed = if effect.is_null() {
        DROPEFFECT_NONE
    } else {
        // SAFETY: OLE passes a valid, aligned in/out pointer that stays
        // valid for the whole call.
        unsafe { effect.replace(DROPEFFECT_NONE) }
    };
    let answer = os::guarded(Effect::None, fail_open, || {
        decide(allowed.contains(DROPEFFECT_COPY))
    });
    if !effect.is_null() {
        let answer = match answer {
            Effect::Copy => DROPEFFECT_COPY,
            Effect::None => DROPEFFECT_NONE,
        };
        // SAFETY: as above.
        unsafe { effect.write(answer) };
    }
}

/// `CF_HDROP` in global memory: the file paths Explorer and other apps
/// drag.
fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Whether the dragged data holds file paths.
fn holds_files(data: &IDataObject) -> bool {
    // SAFETY: a valid FORMATETC for the call; QueryGetData only answers.
    let answer = unsafe { data.QueryGetData(&hdrop_format()) };
    answer == S_OK
}

/// The file paths in the dropped data, at most `MAX_DROPPED_FILES`; empty
/// if there are none or they cannot be read.
fn dropped_paths(data: &IDataObject) -> Vec<PathBuf> {
    // SAFETY: a valid FORMATETC; on success the medium is Hop's to release.
    let Ok(mut medium) = (unsafe { data.GetData(&hdrop_format()) }) else {
        return Vec::new();
    };
    let paths = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
        // SAFETY: a TYMED_HGLOBAL medium holds a global memory handle,
        // which for CF_HDROP is the HDROP.
        let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
        paths_in(hdrop)
    } else {
        Vec::new()
    };
    // SAFETY: releases what GetData handed over, exactly once.
    unsafe { ReleaseStgMedium(&mut medium) };
    paths
}

fn paths_in(hdrop: HDROP) -> Vec<PathBuf> {
    // SAFETY: index 0xFFFFFFFF with no buffer asks for the number of files.
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let count = usize::try_from(count).map_or(0, |count| count.min(MAX_DROPPED_FILES));
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        // SAFETY: no buffer: asks for the path's length, without the NUL.
        let len = unsafe { DragQueryFileW(hdrop, index, None) };
        if len == 0 {
            continue;
        }
        let mut path = vec![0_u16; len as usize + 1];
        // SAFETY: `path` has room for the path and its NUL; Windows writes
        // no more than the slice's length.
        let copied = unsafe { DragQueryFileW(hdrop, index, Some(&mut path)) };
        path.truncate((copied as usize).min(len as usize));
        if !path.is_empty() {
            paths.push(PathBuf::from(OsString::from_wide(&path)));
        }
    }
    paths
}

/// The mouse hook's part (Hop's input thread): follows the left button
/// through `message` at `at` and tells the drop thread when a drag comes
/// near a zone and when the button goes up. `scale` is the primary
/// display's, `threshold` the system's drag threshold. Never waits.
pub(crate) fn follow_mouse(message: u32, at: PhysPoint, scale: f64, threshold: (i32, i32)) {
    if CONTROL.load(Ordering::Relaxed) == 0 {
        // No drop thread, so no zones.
        return;
    }
    let signal = GESTURE
        .try_with(|gesture| {
            let mut gesture = gesture.try_borrow_mut().ok()?;
            match message {
                WM_LBUTTONDOWN => gesture.press(at),
                WM_LBUTTONUP => gesture.release(),
                WM_MOUSEMOVE if gesture.held() => {
                    let zones = match ZONES.try_lock() {
                        Ok(zones) => zones,
                        Err(TryLockError::Poisoned(zones)) => zones.into_inner(),
                        // Being replaced right now: the next move checks.
                        Err(TryLockError::WouldBlock) => return None,
                    };
                    gesture.moved(at, threshold, &zones, scale)
                }
                _ => None,
            }
        })
        .ok()
        .flatten();
    if let Some(signal) = signal {
        post(signal);
    }
}

/// Capture stopped, so the hook will not see the left button go up: zones
/// a drag armed are disarmed now. Input thread.
pub(crate) fn forget_gesture() {
    let signal = GESTURE
        .try_with(|gesture| gesture.try_borrow_mut().ok()?.release())
        .ok()
        .flatten();
    if let Some(signal) = signal {
        post(signal);
    }
}

/// Posts `signal` to the drop thread without waiting.
fn post(signal: Signal) {
    let control = CONTROL.load(Ordering::Acquire);
    if control == 0 {
        return;
    }
    // Both numbers fit in a pointer-sized word on 64-bit Windows; the drop
    // thread reads them back the same way.
    let (message, zones, generation) = match signal {
        Signal::Arm { zones, generation } => (WM_APP_ARM, zones, generation),
        Signal::Disarm { generation } => (WM_APP_DISARM, 0, generation),
    };
    // SAFETY: posting never waits, and a window that is gone only makes
    // the call fail.
    let _ = unsafe {
        PostMessageW(
            Some(os::raw_to_hwnd(control)),
            message,
            WPARAM(zones as usize),
            LPARAM(generation as isize),
        )
    };
}
