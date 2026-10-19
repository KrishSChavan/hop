//! Hop's Windows backend. The contract it implements, and the rules it
//! follows, are in the `hop-platform` crate.
//!
//! On other operating systems this crate is empty.
//!
//! # How it works
//!
//! - **Capture** (`hooks`, `capture`): `WH_MOUSE_LL` and `WH_KEYBOARD_LL`
//!   hooks plus Raw Input for the mouse, all on one dedicated input thread
//!   (`engine`) with a message-only window. Suppression is a lease the hooks
//!   check on every event; a watchdog on the same thread ends a lapsed
//!   lease, frees the cursor, and reinstalls hooks Windows has removed.
//! - **Injection** (`inject`): `SendInput`, every event tagged with
//!   `INJECTED_TAG` in `dwExtraInfo`; absolute moves over the whole virtual
//!   desktop; keys as scancodes.
//! - **Cursor** (`cursor`): parking pins the cursor in the middle of the
//!   primary display (the mouse hook swallows its physical motion, which is
//!   still reported) and hides it under an invisible window whose cursor is
//!   transparent; it never touches the system cursors. A lapsed lease,
//!   stopping capture, or a park no lease has covered for 5 seconds gives
//!   the cursor back.
//! - **Displays** (`simple`): monitors from `EnumDisplayMonitors`, in the
//!   contract's logical units: physical pixels divided by the primary
//!   display's scale.
//! - **Test window** (`test_window`): a small Win32 window on its own thread
//!   for `hop selftest`.
//!
//! The rules behind the numbers (coordinate rounding, edge pushes, key
//! fallbacks, the watchdog's thresholds) are pure functions in `logic`,
//! with unit tests.

#![cfg(windows)]
#![warn(missing_docs)]

mod capture;
mod cursor;
mod engine;
mod hooks;
mod inject;
mod logic;
mod os;
mod shared;
mod simple;
mod test_window;
mod worker;

use hop_platform::{NoDrops, Platform, TestWindow};

/// The Windows backend, ready to use.
///
/// Makes the process per-monitor DPI aware first (so Windows reports
/// physical pixels everywhere; an awareness already set to per-monitor, for
/// example by the app manifest, is fine) and opts it out of EcoQoS power
/// throttling. Nothing starts capturing or moving until the returned
/// components are used. Hop's input thread starts on first use and is
/// shared by every `Platform` this process creates; only one of them can
/// capture at a time.
pub fn native() -> hop_platform::Result<Platform> {
    os::prepare_process()?;
    Ok(Platform {
        capture: Box::new(capture::WinCapture::default()),
        injector: Box::new(inject::WinInjector::default()),
        cursor: Box::new(cursor::WinCursor::default()),
        displays: Box::new(simple::WinDisplays),
        clipboard: Box::new(simple::NoClipboard),
        drops: Box::new(NoDrops),
        permissions: Box::new(simple::WinPermissions),
        system: Box::new(simple::WinSystemState),
    })
}

/// Opens the self-test window (hidden until `TestWindow::show`) on a UI
/// thread of its own.
pub fn open_test_window() -> hop_platform::Result<Box<dyn TestWindow>> {
    os::prepare_process()?;
    Ok(Box::new(test_window::WinTestWindow::open()?))
}

/// Runs `f` and returns its result.
///
/// On macOS, AppKit needs the main thread, so the macOS backend runs `f` on
/// another thread while the main thread serves the UI. Windows has no such
/// rule: each of Hop's windows has its own thread with its own message
/// loop, so `f` simply runs here.
pub fn run_with_ui<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    f()
}
