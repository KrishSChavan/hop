//! The low-level mouse and keyboard hooks and Raw Input: what they report,
//! and what they swallow (the rules are in `logic::swallow`).
//!
//! Everything here runs on Hop's input thread (see `engine`). The hook
//! procedures are `extern "system"` functions that Windows calls with each
//! event before any app sees it. They classify the event, hand it to the
//! sink, decide whether to swallow it, and return. Windows removes a hook
//! that answers too slowly, so nothing slow happens in them.
//!
//! Their state lives in a thread-local (the input thread is the only thread
//! they run on); the suppression lease, which other threads change, lives
//! in `shared`. Keystrokes are never logged.

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Instant;

use hop_platform::{CapturedEvent, INJECTED_TAG, Result};
use hop_types::{InputEvent, MouseButton, ScrollUnit};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RAWMOUSE, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetSystemMetrics, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP,
    MSLLHOOKSTRUCT, SM_CXDRAG, SM_CYDRAG, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    XBUTTON1, XBUTTON2,
};

use crate::logic::coords::{PhysPoint, PhysRect, length_to_logical, to_logical};
use crate::logic::displays::primary_scale;
use crate::logic::keys::{HeldKeys, hook_scancode};
use crate::logic::motion::{HookMove, MotionTracker, position_before};
use crate::logic::scroll::wheel_notches;
use crate::logic::swallow::{Hooked, swallows};
use crate::logic::watchdog::HookHealth;
use crate::{cursor, drops, engine, os, shared};

// The structures Windows hands the hooks and Raw Input are read through raw
// pointers, and GetRawInputData is given RAWINPUT's size: the bindings must
// match the Windows SDK's x64 layouts exactly. Checked when compiling.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<MSLLHOOKSTRUCT>() == 32);
    assert!(size_of::<KBDLLHOOKSTRUCT>() == 24);
    assert!(size_of::<RAWINPUTHEADER>() == 24);
    assert!(size_of::<RAWMOUSE>() == 24);
    assert!(size_of::<RAWINPUT>() == 48);
    assert!(size_of::<RAWINPUTDEVICE>() == 16);
};

/// Where captured events go (the runtime's channel, typically).
pub(crate) type Sink = Box<dyn FnMut(CapturedEvent) + Send>;

/// What the hooks know. Only touched on the input thread.
struct HookState {
    /// `None` while capture is stopped; then nothing is reported.
    sink: Option<Sink>,
    motion: MotionTracker,
    /// Physical keys held, for auto-repeat detection.
    held_physical: HeldKeys,
    /// Keys Hop injected and holds, tracked apart from physical keys.
    held_injected: HeldKeys,
    /// The monitors, refreshed by the watchdog, for edge detection.
    monitors: Vec<PhysRect>,
    /// The primary scale, refreshed by the watchdog.
    scale: f64,
    /// The system's drag threshold (`SM_CXDRAG`, `SM_CYDRAG`), refreshed by
    /// the watchdog, for the drop zones.
    drag_threshold: (i32, i32),
    /// The cursor position after the last mouse event.
    cursor: PhysPoint,
    /// When each hook last ran and when raw motion last arrived
    /// (`GetTickCount`), for the watchdog.
    mouse_tick: u32,
    keyboard_tick: u32,
    raw_tick: Option<u32>,
}

impl HookState {
    fn new() -> Self {
        Self {
            sink: None,
            motion: MotionTracker::new(),
            held_physical: HeldKeys::default(),
            held_injected: HeldKeys::default(),
            monitors: Vec::new(),
            scale: 1.0,
            // Windows' default.
            drag_threshold: (4, 4),
            cursor: PhysPoint::default(),
            mouse_tick: 0,
            keyboard_tick: 0,
            raw_tick: None,
        }
    }
}

/// The installed hooks. `None` handles are retried by the watchdog.
struct Installed {
    mouse: Option<HHOOK>,
    keyboard: Option<HHOOK>,
    /// When they were last (re)installed (`GetTickCount`).
    at: u32,
}

thread_local! {
    static STATE: RefCell<HookState> = RefCell::new(HookState::new());
    static INSTALLED: RefCell<Option<Installed>> = const { RefCell::new(None) };
}

/// Runs `f` with the hook state, unless it is already in use further up
/// this thread's stack (a re-entrant call, which Windows can cause while a
/// hook or window procedure is running) or the thread is shutting down.
fn with_state<T>(f: impl FnOnce(&mut HookState) -> T) -> Option<T> {
    STATE
        .try_with(|cell| cell.try_borrow_mut().ok().map(|mut state| f(&mut state)))
        .ok()
        .flatten()
}

/// Starts capture: registers Raw Input, installs both hooks, and reports to
/// `sink` from now on. Runs on the input thread.
pub(crate) fn install(sink: Sink) -> Result<()> {
    if INSTALLED.with(|installed| installed.borrow().is_some()) {
        return Err(hop_platform::PlatformError::Os(
            "input capture is already running".into(),
        ));
    }
    register_raw_input(engine::window())?;
    let (mouse, keyboard) = match set_hooks() {
        Ok(hooks) => hooks,
        Err(error) => {
            unregister_raw_input();
            return Err(error);
        }
    };
    let now = os::tick();
    let cursor = os::cursor_pos().unwrap_or_default();
    with_state(|state| {
        *state = HookState::new();
        state.sink = Some(sink);
        state.cursor = cursor;
        state.mouse_tick = now;
        state.keyboard_tick = now;
    });
    INSTALLED.with(|installed| {
        *installed.borrow_mut() = Some(Installed {
            mouse: Some(mouse),
            keyboard: Some(keyboard),
            at: now,
        });
    });
    refresh_geometry();
    Ok(())
}

/// Stops capture: removes the hooks and Raw Input and drops the sink. Runs
/// on the input thread.
pub(crate) fn uninstall() {
    if let Some(installed) = INSTALLED.with(|installed| installed.borrow_mut().take()) {
        unhook(installed.mouse);
        unhook(installed.keyboard);
    }
    unregister_raw_input();
    // Nothing will see the left button go up now.
    drops::forget_gesture();
    // Dropping the sink lets the runtime's receiving side see the end.
    let sink = with_state(|state| {
        let sink = state.sink.take();
        *state = HookState::new();
        sink
    });
    drop(sink);
}

fn set_hooks() -> Result<(HHOOK, HHOOK)> {
    let instance = os::module_instance()?;
    // SAFETY: the procedures are 'static functions in this module. Windows
    // calls them on this thread, which pumps messages, and they touch only
    // this thread's state and the shared lease.
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(instance), 0) }
        .map_err(|error| os::os_error("SetWindowsHookExW (mouse)", &error))?;
    // SAFETY: as above.
    let keyboard = match unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(instance), 0)
    } {
        Ok(keyboard) => keyboard,
        Err(error) => {
            unhook(Some(mouse));
            return Err(os::os_error("SetWindowsHookExW (keyboard)", &error));
        }
    };
    Ok((mouse, keyboard))
}

/// Whether the mouse hook is installed, as far as Hop knows (Windows can
/// remove it silently; the watchdog then reinstalls it). Input thread.
pub(crate) fn mouse_hooked() -> bool {
    INSTALLED
        .try_with(|installed| {
            installed
                .try_borrow()
                .is_ok_and(|installed| installed.as_ref().is_some_and(|i| i.mouse.is_some()))
        })
        .unwrap_or(false)
}

fn unhook(hook: Option<HHOOK>) {
    if let Some(hook) = hook {
        // SAFETY: the handle came from SetWindowsHookExW on this thread. If
        // Windows already removed the hook, the call just fails.
        let _ = unsafe { UnhookWindowsHookEx(hook) };
    }
}

/// The mouse as a Raw Input device (usage page 1, usage 2).
fn mouse_device(
    flags: windows::Win32::UI::Input::RAWINPUTDEVICE_FLAGS,
    target: HWND,
) -> RAWINPUTDEVICE {
    RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: flags,
        hwndTarget: target,
    }
}

/// Raw Input reports mouse motion even where the cursor cannot move (at a
/// screen edge). `RIDEV_INPUTSINK` delivers it while Hop is in the
/// background, which is always.
fn register_raw_input(target: HWND) -> Result<()> {
    let device = mouse_device(RIDEV_INPUTSINK, target);
    // SAFETY: one fully initialized RAWINPUTDEVICE and its size.
    unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) }
        .map_err(|error| os::os_error("RegisterRawInputDevices", &error))
}

fn unregister_raw_input() {
    let device = mouse_device(RIDEV_REMOVE, HWND::default());
    // SAFETY: as above; RIDEV_REMOVE requires a null target.
    let _ = unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) };
}

/// The watchdog's refresh of the monitor layout and primary scale, which
/// the hooks need for edge detection and logical units, and of the drag
/// threshold. Monitors and scaling can change at any time, and a
/// message-only window does not get the broadcast that announces it, so
/// this simply runs every tick while capturing. It costs microseconds.
pub(crate) fn refresh_geometry() {
    if INSTALLED.with(|installed| installed.borrow().is_none()) {
        return;
    }
    let Ok(monitors) = os::monitors() else {
        return;
    };
    let scale = primary_scale(&monitors);
    let rects: Vec<PhysRect> = monitors.iter().map(|monitor| monitor.rect).collect();
    // SAFETY: plain calls.
    let drag_threshold = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
    with_state(|state| {
        state.monitors = rects;
        state.scale = scale;
        state.drag_threshold = drag_threshold;
    });
}

/// The watchdog's hook check: reinstall the hooks if Windows seems to have
/// removed them (see [`HookHealth::needs_reinstall`]).
pub(crate) fn check_health() {
    let Some(installed_at) =
        INSTALLED.with(|installed| installed.borrow().as_ref().map(|installed| installed.at))
    else {
        return;
    };
    let Some((mouse_hook, keyboard_hook, last_raw)) =
        with_state(|state| (state.mouse_tick, state.keyboard_tick, state.raw_tick))
    else {
        return;
    };
    let health = HookHealth {
        now: os::tick(),
        installed: installed_at,
        mouse_hook,
        keyboard_hook,
        last_input: os::last_input_tick(),
        last_raw,
    };
    if health.needs_reinstall() {
        reinstall();
    }
}

/// Removes and installs both hooks again. A newly installed hook is also
/// first in line, ahead of hooks other programs added since.
fn reinstall() {
    let Some(old) = INSTALLED.with(|installed| installed.borrow_mut().take()) else {
        return;
    };
    unhook(old.mouse);
    unhook(old.keyboard);
    let now = os::tick();
    // On failure the handles stay empty and the next check tries again.
    let (mouse, keyboard) = match set_hooks() {
        Ok((mouse, keyboard)) => (Some(mouse), Some(keyboard)),
        Err(_) => (None, None),
    };
    with_state(|state| {
        state.mouse_tick = now;
        state.keyboard_tick = now;
    });
    INSTALLED.with(|installed| {
        *installed.borrow_mut() = Some(Installed {
            mouse,
            keyboard,
            at: now,
        });
    });
}

/// Hop moved the cursor itself, from `from` to `to`; the next hook event
/// may predate the move (see `logic::motion`).
pub(crate) fn note_warp(from: PhysPoint, to: PhysPoint) {
    with_state(|state| {
        state.motion.warped(from, to, Instant::now());
        state.cursor = to;
    });
}

/// Is this event one Hop injected itself?
fn is_physical(extra_info: usize) -> bool {
    extra_info != INJECTED_TAG as usize
}

/// The low-level mouse hook.
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: for HC_ACTION, lparam points to an MSLLHOOKSTRUCT that
        // Windows keeps valid for the duration of this call; it is copied.
        let info = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
        // wparam holds the message number, which fits in 32 bits.
        let message = wparam.0 as u32;
        if os::guarded(false, after_panic, || on_mouse(message, &info)) {
            // Swallowed: neither later hooks nor any app see the event.
            return LRESULT(1);
        }
    }
    // SAFETY: the hook's own arguments, passed on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// The low-level keyboard hook.
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT that
        // Windows keeps valid for the duration of this call; it is copied.
        let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if os::guarded(false, after_panic, || on_key(&info)) {
            return LRESULT(1);
        }
    }
    // SAFETY: the hook's own arguments, passed on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// A panic in a hook (most likely in the sink): fail open, and stop
/// reporting, so a sink that panics on every event cannot make every event
/// slow, which would get the hooks removed. Capture is then dead until it is
/// restarted, but local input works normally: without a sink the hooks
/// swallow nothing, even if the runtime keeps renewing the lease.
fn after_panic() {
    engine::fail_open();
    let sink = with_state(|state| state.sink.take());
    drop(sink);
}

/// Whether the suppression lease covers a physical or injected event right
/// now. Lock-free: only the lease's atomic copy is read.
///
/// The first event after the lease ran out also asks the input thread to
/// clean up (show and free the cursor) at once, instead of at the next
/// watchdog tick.
fn should_suppress(physical: bool, now: Instant) -> bool {
    if !physical {
        // Never swallow what Hop injected.
        return false;
    }
    if shared::suppressing(now) {
        return true;
    }
    if shared::lapse_needs_cleanup(now) {
        engine::request_lease_check();
    }
    false
}

/// One mouse event. Returns whether to swallow it.
///
/// Whether to swallow follows `logic::swallow` (plus the stale-event rule),
/// never whether this particular event could be turned into a report, so
/// input meant for the other device cannot leak into local apps.
fn on_mouse(message: u32, info: &MSLLHOOKSTRUCT) -> bool {
    if cursor::moving_cursor() {
        // Hop's own SetCursorPos or ClipCursor, running on this thread, has
        // called the hook for the move it causes: not the user's input.
        return false;
    }
    let now = Instant::now();
    let physical = is_physical(info.dwExtraInfo);
    // A veiled cursor shows again at the user's first touch of the mouse,
    // and the veil is gone before Windows routes this very event, so a
    // click lands where the user sees the cursor (see `cursor`).
    let moved_to = (message == WM_MOUSEMOVE).then_some(PhysPoint::new(info.pt.x, info.pt.y));
    cursor::veil_on_mouse(physical, moved_to, now);
    let suppressing = should_suppress(physical, now);
    let parked = hooked(message) == Hooked::Motion && physical && cursor::is_parked();
    match with_state(|state| state.mouse(message, info, physical, suppressing, parked, now)) {
        Some(swallow) => swallow,
        // The state is busy further up this thread's stack (a re-entrant
        // call): decide as if the sink were there.
        None => swallows(hooked(message), physical, true, suppressing, parked),
    }
}

/// The kind of a mouse-hook message, for `logic::swallow`.
fn hooked(message: u32) -> Hooked {
    if message == WM_MOUSEMOVE {
        Hooked::Motion
    } else {
        Hooked::Other
    }
}

/// One key event. Returns whether to swallow it.
fn on_key(info: &KBDLLHOOKSTRUCT) -> bool {
    let physical = is_physical(info.dwExtraInfo);
    let suppressing = should_suppress(physical, Instant::now());
    // `None`: the state is busy further up this thread's stack; decide as
    // if the sink were there.
    let reporting = with_state(|state| state.key(info, physical)).unwrap_or(true);
    swallows(Hooked::Other, physical, reporting, suppressing, false)
}

impl HookState {
    /// One mouse event; returns whether to swallow it. `suppressing`: the
    /// lease covers it. `parked`: a physical move with the cursor parked.
    fn mouse(
        &mut self,
        message: u32,
        info: &MSLLHOOKSTRUCT,
        physical: bool,
        suppressing: bool,
        parked: bool,
        now: Instant,
    ) -> bool {
        self.mouse_tick = os::tick();
        let reporting = self.sink.is_some();
        let swallow = swallows(hooked(message), physical, reporting, suppressing, parked);
        if !swallow {
            // What reaches this PC's apps may be a drag toward a drop zone.
            let at = PhysPoint::new(info.pt.x, info.pt.y);
            drops::follow_mouse(message, at, self.scale, self.drag_threshold);
        }
        if !reporting {
            // Nothing is reported, so `swallow` is false: fail open.
            return swallow;
        }
        let pt = PhysPoint::new(info.pt.x, info.pt.y);
        let button = |button, pressed| Some(InputEvent::Button { button, pressed });
        // XBUTTON1/2 in the high word: the side buttons.
        let side = |pressed| match (info.mouseData >> 16) as u16 {
            XBUTTON1 => button(MouseButton::Back, pressed),
            XBUTTON2 => button(MouseButton::Forward, pressed),
            _ => None,
        };
        let event = match message {
            WM_MOUSEMOVE => return self.moved(pt, physical, swallow, now),
            WM_LBUTTONDOWN => button(MouseButton::Left, true),
            WM_LBUTTONUP => button(MouseButton::Left, false),
            WM_RBUTTONDOWN => button(MouseButton::Right, true),
            WM_RBUTTONUP => button(MouseButton::Right, false),
            WM_MBUTTONDOWN => button(MouseButton::Middle, true),
            WM_MBUTTONUP => button(MouseButton::Middle, false),
            WM_XBUTTONDOWN => side(true),
            WM_XBUTTONUP => side(false),
            WM_MOUSEWHEEL => scroll(0.0, wheel_notches(info.mouseData)),
            WM_MOUSEHWHEEL => scroll(wheel_notches(info.mouseData), 0.0),
            _ => None,
        };
        if let Some(event) = event {
            self.emit(event, pt, physical);
        }
        self.cursor = pt;
        swallow
    }

    /// A move: report the attempted motion (see `logic::motion`). `swallow`
    /// says whether the cursor is held still (suppressed or parked); the
    /// result is whether to swallow the event.
    fn moved(&mut self, pt: PhysPoint, physical: bool, swallow: bool, now: Instant) -> bool {
        // Inside the hook the cursor has not moved yet (see
        // `position_before` for the fallbacks).
        let before = position_before(os::cursor_pos().ok(), pt, self.cursor);
        let verdict = if physical {
            // Raw motion only fills in pushes past an edge where the cursor
            // really moves; a cursor held still is not at an edge it could
            // cross.
            self.motion
                .hook_move(pt, before, !swallow, &self.monitors, now)
        } else {
            match (
                i64::from(pt.x) - i64::from(before.x),
                i64::from(pt.y) - i64::from(before.y),
            ) {
                (0, 0) => HookMove::Still,
                (dx, dy) => HookMove::Report { dx, dy },
            }
        };
        match verdict {
            HookMove::Stale => {
                self.cursor = before;
                return true;
            }
            HookMove::Still => {}
            HookMove::Report { dx, dy } => {
                let event = InputEvent::PointerMotion {
                    dx: length_to_logical(dx as f64, self.scale),
                    dy: length_to_logical(dy as f64, self.scale),
                };
                self.emit(event, before, physical);
            }
        }
        // A swallowed move leaves the cursor where it was.
        self.cursor = if swallow { before } else { pt };
        swallow
    }

    /// One key event. Returns whether capture is reporting (a sink is
    /// attached), which `logic::swallow` needs; a key Hop does not know is
    /// not reported, but while suppressing it is still swallowed.
    fn key(&mut self, info: &KBDLLHOOKSTRUCT, physical: bool) -> bool {
        self.keyboard_tick = os::tick();
        if self.sink.is_none() {
            return false;
        }
        let pressed = !info.flags.contains(LLKHF_UP);
        let extended = info.flags.contains(LLKHF_EXTENDED);
        let scancode = hook_scancode(info.scanCode, extended, info.vkCode, |vk| {
            // SAFETY: plain call; reads this thread's keyboard layout.
            unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX) }
        });
        let Some((scan, extended)) = scancode else {
            return true;
        };
        let held = if physical {
            &mut self.held_physical
        } else {
            &mut self.held_injected
        };
        let repeat = if pressed {
            held.press(scan, extended)
        } else {
            held.release(scan, extended);
            false
        };
        let Some(code) = hop_keymap::from_windows_scancode(scan, extended) else {
            return true;
        };
        let event = InputEvent::Key {
            code,
            pressed,
            repeat,
        };
        self.emit(event, self.cursor, physical);
        true
    }

    /// Relative motion from Raw Input; reports an edge push when the hook
    /// cannot (see `logic::motion`).
    fn raw(&mut self, dx: i32, dy: i32, passive: bool, now: Instant) {
        self.raw_tick = Some(os::tick());
        if self.sink.is_none() {
            return;
        }
        let at = os::cursor_pos().unwrap_or(self.cursor);
        if let Some((push_x, push_y)) =
            self.motion
                .raw_motion(dx, dy, at, passive, &self.monitors, now)
        {
            let event = InputEvent::PointerMotion {
                dx: length_to_logical(push_x as f64, self.scale),
                dy: length_to_logical(push_y as f64, self.scale),
            };
            self.emit(event, at, true);
        }
    }

    fn emit(&mut self, event: InputEvent, cursor: PhysPoint, physical: bool) {
        let cursor = to_logical(cursor, self.scale);
        if let Some(sink) = self.sink.as_mut() {
            sink(CapturedEvent {
                event,
                cursor,
                physical,
            });
        }
    }
}

/// A wheel event, unless it carries no motion.
fn scroll(dx: f64, dy: f64) -> Option<InputEvent> {
    (dx != 0.0 || dy != 0.0).then_some(InputEvent::Scroll {
        dx,
        dy,
        unit: ScrollUnit::Line,
    })
}

/// `WM_INPUT` on the input thread's window: relative mouse motion, used to
/// see pushes past a screen edge and to watch the mouse hook's health.
pub(crate) fn on_raw_input(lparam: LPARAM) {
    os::guarded((), after_panic, || {
        let Some(mouse) = read_raw_mouse(lparam) else {
            return;
        };
        // Absolute devices (pens, touch, remote desktop) cannot push past an
        // edge. Hop's own injected moves are absolute too, and tagged.
        if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0
            || mouse.ulExtraInformation == INJECTED_TAG as u32
            || (mouse.lLastX == 0 && mouse.lLastY == 0)
        {
            return;
        }
        let now = Instant::now();
        let passive = !shared::suppressing(now);
        with_state(|state| state.raw(mouse.lLastX, mouse.lLastY, passive, now));
    });
}

fn read_raw_mouse(lparam: LPARAM) -> Option<RAWMOUSE> {
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: lparam is the HRAWINPUT of the WM_INPUT being handled; `raw`
    // is a writable buffer of `size` bytes, and Windows writes no more than
    // that (it fails instead if the input is larger, which mouse input
    // never is).
    let copied = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut c_void),
            RID_INPUT,
            Some((&raw mut raw).cast::<c_void>()),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if copied == 0 || copied == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    // SAFETY: the header says this record is mouse input, so the union
    // holds a RAWMOUSE, which Windows filled in above.
    Some(unsafe { raw.data.mouse })
}
