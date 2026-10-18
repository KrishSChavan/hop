//! Picks this OS's platform backend.
//!
//! - [`native`]: this computer's platform layer (capture, injection, the
//!   cursor, displays, permissions).
//! - [`open_test_window`]: the self-test's window.
//! - [`run_with_ui`]: runs work on another thread while the main thread
//!   serves AppKit on a Mac (elsewhere it simply runs the work).
//! - [`stay_responsive`]: keeps macOS from napping Hop while it shares.

#[cfg(target_os = "macos")]
pub use hop_platform_macos::{native, open_test_window, run_with_ui};
#[cfg(windows)]
pub use hop_platform_windows::{native, open_test_window, run_with_ui};

#[cfg(not(any(target_os = "macos", windows)))]
pub use unsupported::{native, open_test_window, run_with_ui};

/// Linux has no backend yet, so everything that needs one says so.
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use hop_platform::{Platform, PlatformError, Result, TestWindow};

    /// There is no backend for this OS yet.
    pub fn native() -> Result<Platform> {
        Err(PlatformError::Unsupported)
    }

    /// There is no backend for this OS yet.
    pub fn open_test_window() -> Result<Box<dyn TestWindow>> {
        Err(PlatformError::Unsupported)
    }

    /// Runs `f` here: there is no UI loop to serve.
    pub fn run_with_ui<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        f()
    }
}

/// What [`stay_responsive`] returns: hold it for as long as Hop shares. On a
/// Mac it is an App Nap exemption, which must stay on the thread that made
/// it (the main thread); elsewhere it holds nothing.
#[cfg(target_os = "macos")]
pub type Responsive = hop_platform_macos::AppNapExemption;

/// Keeps macOS from throttling Hop's timers (App Nap) while the agent runs:
/// its 10 ms tick renews the input lease and sends heartbeats. Hold the
/// returned guard on the main thread for as long as Hop runs. Elsewhere
/// there is nothing to hold (the Windows backend opts out of power
/// throttling when capture starts).
#[cfg(target_os = "macos")]
#[must_use = "the exemption ends when the guard is dropped"]
pub fn stay_responsive() -> Responsive {
    hop_platform_macos::AppNapExemption::begin("Hop is sharing this Mac's keyboard and mouse")
}

/// Nothing to hold outside macOS.
#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct Responsive;

/// Nothing to do outside macOS: the guard holds nothing.
#[cfg(not(target_os = "macos"))]
#[must_use = "the exemption ends when the guard is dropped"]
pub fn stay_responsive() -> Responsive {
    Responsive
}
