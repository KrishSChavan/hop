//! Hop's input thread: one thread that owns the low-level hooks, the Raw
//! Input registration, the watchdog timer and the window that hides the
//! parked cursor.
//!
//! Low-level hooks are called on the thread that installed them, between
//! that thread's message-loop iterations, so this thread does nothing slow:
//! it pumps messages, runs the short jobs other threads hand it (install
//! hooks, park the cursor), and every 500 ms runs the watchdog. It starts on
//! first use and lives as long as the process.

use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hop_platform::{PlatformError, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, HCURSOR, HWND_MESSAGE, PostMessageW, SetTimer,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT, WM_TIMER,
};
use windows::core::{PCWSTR, w};

use crate::logic::watchdog;
use crate::worker::{self, WM_APP_WAKE, Worker};
use crate::{cursor, hooks, os, shared};

/// The input thread, once started.
static ENGINE: Mutex<Option<Worker>> = Mutex::new(None);

const CLASS: PCWSTR = w!("HopInputThread");
const WATCHDOG_TIMER: usize = 1;

/// Posted by a hook that noticed the lease ran out, so the cursor is freed
/// at once rather than at the next watchdog tick.
const WM_APP_LEASE_CHECK: u32 = WM_APP + 0x11;

/// How long callers wait for the input thread. Its jobs take microseconds
/// to milliseconds; a thread that takes this long is stuck.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(5);

thread_local! {
    /// On the input thread: its message-only window, which receives Raw
    /// Input. Null on every other thread.
    static WINDOW: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs `job` on the input thread, starting the thread if needed.
pub(crate) fn run<R: Send + 'static>(job: impl FnOnce() -> R + Send + 'static) -> Result<R> {
    worker()?.run(CALL_TIMEOUT, job)
}

/// The input thread's message-only window. Only meaningful on that thread.
pub(crate) fn window() -> HWND {
    os::raw_to_hwnd(WINDOW.with(std::cell::Cell::get))
}

/// Asks the input thread to run the lease check soon. Called by the hooks
/// (on the input thread), so it only posts a message and returns.
pub(crate) fn request_lease_check() {
    // SAFETY: posting to this thread's own window; it never blocks.
    let _ = unsafe { PostMessageW(Some(window()), WM_APP_LEASE_CHECK, WPARAM(0), LPARAM(0)) };
}

fn worker() -> Result<Worker> {
    let mut slot = ENGINE.lock().unwrap_or_else(PoisonError::into_inner);
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
        .name("hop-input".into())
        .spawn(move || main(&ready))
        .map_err(|error| {
            PlatformError::Os(format!("could not start Hop's input thread: {error}"))
        })?;
    started
        .recv_timeout(START_TIMEOUT)
        .map_err(|_| PlatformError::Os("Hop's input thread did not start".into()))?
        .map_err(PlatformError::Os)
}

/// The input thread.
fn main(ready: &SyncSender<std::result::Result<Worker, String>>) {
    let hwnd = match create_window() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    WINDOW.with(|window| window.set(os::hwnd_to_raw(hwnd)));
    // Hooks run on this thread; answering them promptly keeps Windows from
    // removing them, so it runs above normal priority. Best effort.
    // SAFETY: GetCurrentThread's pseudo-handle is always valid here.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    // SAFETY: `hwnd` is this thread's window; no timer procedure, so the
    // timer arrives as WM_TIMER in `procedure`.
    if unsafe { SetTimer(Some(hwnd), WATCHDOG_TIMER, watchdog::TICK_MS, None) } == 0 {
        let _ = ready.send(Err(os::last_error("SetTimer").to_string()));
        return;
    }
    let worker = Worker::install(hwnd, "Hop's input thread");
    if ready.send(Ok(worker)).is_err() {
        return;
    }
    worker::message_loop();
}

fn create_window() -> Result<HWND> {
    os::register_class(
        CLASS,
        Some(procedure),
        HCURSOR::default(),
        HBRUSH::default(),
    )?;
    let instance = os::module_instance()?;
    // SAFETY: the class was just registered with a 'static procedure.
    // HWND_MESSAGE makes this a message-only window: never shown, never
    // enumerated, only there to receive messages.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Hop input"),
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
    .map_err(|error| os::os_error("CreateWindowExW (input thread)", &error))
}

/// The message-only window's procedure.
unsafe extern "system" fn procedure(
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
        WM_TIMER if wparam.0 == WATCHDOG_TIMER => {
            watchdog_tick();
            Some(LRESULT(0))
        }
        WM_APP_LEASE_CHECK => {
            lease_check();
            Some(LRESULT(0))
        }
        // Not "handled": DefWindowProcW must still see WM_INPUT so Windows
        // can free the input's data.
        WM_INPUT => {
            hooks::on_raw_input(lparam);
            None
        }
        _ => None,
    });
    match handled {
        Some(result) => result,
        // SAFETY: the arguments are passed on unchanged.
        None => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Every [`watchdog::TICK_MS`]: end a lapsed lease (and free the cursor),
/// free a parked cursor no lease has covered for a while, refresh the
/// monitor layout the hooks use, keep a parked cursor pinned, and reinstall
/// hooks Windows has removed.
fn watchdog_tick() {
    lease_check();
    cursor::release_if_orphaned(Instant::now());
    hooks::refresh_geometry();
    cursor::reassert();
    hooks::check_health();
}

/// A lease that was renewed but has run out: back to passive (the hooks
/// already stopped swallowing at the expiry) and free the cursor.
fn lease_check() {
    if shared::lease_lapsed(Instant::now()) {
        cursor::unpark_after_lapse();
    }
}

/// After a panic on the input thread: give local control back. Stop
/// suppressing and free the cursor; nothing here may panic.
pub(crate) fn fail_open() {
    shared::end_suppression();
    cursor::release_after_panic();
}
