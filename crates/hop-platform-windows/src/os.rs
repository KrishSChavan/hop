//! Small wrappers over the Win32 calls the other modules share: errors,
//! process setup, monitors, the cursor position, window classes, ticks.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use hop_platform::{PlatformError, Result};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, POINT, RECT,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HBRUSH, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY,
    MONITORINFO, MONITORINFOEXW, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetProcessInformation,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetProcessDpiAwareness,
    MDT_EFFECTIVE_DPI, PROCESS_PER_MONITOR_DPI_AWARE, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, HCURSOR, MONITORINFOF_PRIMARY, RegisterClassExW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WNDCLASSEXW,
    WNDPROC,
};
use windows::core::{BOOL, Error as WinError, PCWSTR};

use crate::logic::coords::{BASE_DPI, PhysPoint, PhysRect, scale_from_dpi};
use crate::logic::displays::Monitor;

/// A Windows error, with what Hop was doing when it happened.
pub(crate) fn os_error(what: &str, error: &WinError) -> PlatformError {
    PlatformError::Os(format!("{what} failed: {error}"))
}

/// The calling thread's last Windows error (`GetLastError`), with what Hop
/// was doing. Call it right after the failing call, before anything else
/// can overwrite the error.
pub(crate) fn last_error(what: &str) -> PlatformError {
    os_error(what, &WinError::from_thread())
}

/// Runs `f`, catching a panic so it can never unwind into Windows through a
/// callback. On a panic, `on_panic` runs (to fail open) and `fallback` is
/// returned.
pub(crate) fn guarded<T>(fallback: T, on_panic: impl FnOnce(), f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => {
            // The fail-open action itself must not panic again; if it does,
            // there is nothing more that can safely be done here.
            let _ = catch_unwind(AssertUnwindSafe(on_panic));
            fallback
        }
    }
}

/// A window handle as a plain number, for handing to other threads (`HWND`
/// holds a raw pointer, so it is not `Send`). Only the thread that owns the
/// window turns it back into an `HWND` to use it, or posts messages to it,
/// which Windows allows from any thread.
pub(crate) fn hwnd_to_raw(hwnd: HWND) -> usize {
    hwnd.0 as usize
}

/// The inverse of [`hwnd_to_raw`].
pub(crate) fn raw_to_hwnd(raw: usize) -> HWND {
    HWND(raw as *mut c_void)
}

/// One-time process setup for the backend: per-monitor DPI awareness (so
/// every coordinate Windows hands Hop is in physical pixels) and the EcoQoS
/// opt-out. Safe to call any number of times, from any thread.
pub(crate) fn prepare_process() -> Result<()> {
    static PREPARED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    PREPARED
        .get_or_init(|| {
            set_dpi_awareness()?;
            opt_out_of_power_throttling();
            Ok(())
        })
        .clone()
        .map_err(PlatformError::Os)
}

fn set_dpi_awareness() -> std::result::Result<(), String> {
    // SAFETY: takes a constant handle value and touches only process state.
    let result =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if let Err(error) = result {
        // ERROR_ACCESS_DENIED means the awareness was already set (by the
        // app's manifest or an earlier call). That is fine if it is
        // per-monitor; anything else is a real failure.
        if error.code() != ERROR_ACCESS_DENIED.to_hresult() {
            return Err(format!("SetProcessDpiAwarenessContext failed: {error}"));
        }
    }
    // SAFETY: `None` asks about the current process; no pointers involved.
    let awareness = unsafe { GetProcessDpiAwareness(None) }
        .map_err(|error| format!("GetProcessDpiAwareness failed: {error}"))?;
    if awareness != PROCESS_PER_MONITOR_DPI_AWARE {
        return Err(
            "this process is not per-monitor DPI aware (something set a different DPI awareness \
             before Hop started), so Windows would report scaled coordinates Hop cannot use"
                .into(),
        );
    }
    Ok(())
}

/// Asks Windows not to throttle Hop to save power (EcoQoS): a throttled
/// process answers its input hooks late, and Windows removes hooks that
/// answer late. Best effort: older Windows versions reject the request, and
/// Hop still works there.
fn opt_out_of_power_throttling() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        // Take control of execution-speed throttling...
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        // ...and turn it off.
        StateMask: 0,
    };
    // SAFETY: the pointer and size describe `state`, which outlives the call.
    let _ = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast::<c_void>(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
}

/// This program's module handle, for window classes and hooks.
pub(crate) fn module_instance() -> Result<HINSTANCE> {
    // SAFETY: a null name asks for the module of the running .exe, which
    // stays loaded for the life of the process.
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|error| os_error("GetModuleHandleW", &error))?;
    Ok(module.into())
}

/// Registers a window class. Registering a name twice is fine: the first
/// registration stays in effect.
pub(crate) fn register_class(
    name: PCWSTR,
    procedure: WNDPROC,
    cursor: HCURSOR,
    background: HBRUSH,
) -> Result<()> {
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: procedure,
        hInstance: module_instance()?,
        hCursor: cursor,
        hbrBackground: background,
        lpszClassName: name,
        ..Default::default()
    };
    // SAFETY: `class` is fully initialized; the name is a 'static literal and
    // the procedure a 'static function, so both outlive the class.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        let error = WinError::from_thread();
        if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
            return Err(os_error("RegisterClassExW", &error));
        }
    }
    Ok(())
}

/// Every monitor, as Windows reports it now.
pub(crate) fn monitors() -> Result<Vec<Monitor>> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `handles`, which outlives the
    // call: EnumDisplayMonitors calls back synchronously and returns after
    // the last monitor.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&raw mut handles) as isize),
        )
    };
    if !ok.as_bool() {
        return Err(last_error("EnumDisplayMonitors"));
    }
    Ok(handles.into_iter().filter_map(describe_monitor).collect())
}

/// `EnumDisplayMonitors` callback: collects the handles, nothing more.
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let pushed = guarded(
        false,
        || {},
        || {
            // SAFETY: `data` is the `&mut Vec<HMONITOR>` that `monitors` passed
            // in; it is alive and not otherwise borrowed during the enumeration.
            let handles = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
            handles.push(monitor);
            true
        },
    );
    // FALSE stops the enumeration.
    BOOL::from(pushed)
}

fn describe_monitor(handle: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is a MONITORINFOEXW whose cbSize says so, which lets
    // Windows fill in the device name too; it outlives the call.
    let ok = unsafe { GetMonitorInfoW(handle, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }
    let rect = info.monitorInfo.rcMonitor;
    Some(Monitor {
        device: info
            .szDevice
            .iter()
            .copied()
            .take_while(|&unit| unit != 0)
            .collect(),
        rect: PhysRect::new(rect.left, rect.top, rect.right, rect.bottom),
        dpi: monitor_dpi(handle).unwrap_or(BASE_DPI),
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

/// A monitor's effective DPI (96 is 100% scaling), if Windows reports one.
pub(crate) fn monitor_dpi(monitor: HMONITOR) -> Option<u32> {
    let (mut dpi_x, mut dpi_y) = (0_u32, 0_u32);
    // SAFETY: both out-pointers are valid locals for the call.
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.ok()?;
    (dpi_x != 0).then_some(dpi_x)
}

/// The primary monitor's handle.
fn primary_monitor() -> HMONITOR {
    // SAFETY: plain call; the primary monitor's top-left is always (0, 0),
    // and MONITOR_DEFAULTTOPRIMARY returns it even if something odd happens.
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

/// The primary monitor's scale: physical pixels per logical unit everywhere.
pub(crate) fn primary_scale() -> f64 {
    scale_from_dpi(monitor_dpi(primary_monitor()).unwrap_or(BASE_DPI))
}

/// The primary monitor's work area (without the taskbar), in physical
/// pixels, and its DPI.
pub(crate) fn primary_work_area() -> (PhysRect, u32) {
    let primary = primary_monitor();
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a MONITORINFO with cbSize set; it outlives the call.
    let ok = unsafe { GetMonitorInfoW(primary, &mut info) };
    let work = info.rcWork;
    let area = if ok.as_bool() {
        PhysRect::new(work.left, work.top, work.right, work.bottom)
    } else {
        PhysRect::new(0, 0, 1024, 768)
    };
    (area, monitor_dpi(primary).unwrap_or(BASE_DPI))
}

/// The virtual screen (the bounding box of all monitors), physical pixels.
pub(crate) fn virtual_screen() -> PhysRect {
    // SAFETY: plain calls without pointers.
    let (x, y, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    PhysRect::new(x, y, x.saturating_add(width), y.saturating_add(height))
}

/// Where the cursor is, in physical pixels. Inside a low-level mouse hook
/// this is still the position *before* the event being decided on.
pub(crate) fn cursor_pos() -> Result<PhysPoint> {
    let mut point = POINT::default();
    // SAFETY: `point` is a valid out-pointer for the call.
    unsafe { GetCursorPos(&mut point) }.map_err(|error| os_error("GetCursorPos", &error))?;
    Ok(PhysPoint::new(point.x, point.y))
}

/// Milliseconds since boot (`GetTickCount`), the clock of `GetLastInputInfo`.
pub(crate) fn tick() -> u32 {
    // SAFETY: plain call.
    unsafe { GetTickCount() }
}

/// When the system last saw input from anyone, as a [`tick`] value.
pub(crate) fn last_input_tick() -> Option<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set.
    unsafe { GetLastInputInfo(&mut info) }
        .as_bool()
        .then_some(info.dwTime)
}
