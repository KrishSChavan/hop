//! Picks this OS's platform backend.

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

    pub fn native() -> Result<Platform> {
        Err(PlatformError::Unsupported)
    }

    pub fn open_test_window() -> Result<Box<dyn TestWindow>> {
        Err(PlatformError::Unsupported)
    }

    pub fn run_with_ui<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        f()
    }
}

/// Keeps macOS from throttling Hop's timers (App Nap) while the agent runs:
/// its 10 ms tick renews the input lease and sends heartbeats. Hold the
/// returned guard on the main thread for as long as Hop runs. Elsewhere
/// there is nothing to hold (the Windows backend opts out of power
/// throttling when capture starts).
#[cfg(target_os = "macos")]
pub fn stay_responsive() -> hop_platform_macos::AppNapExemption {
    hop_platform_macos::AppNapExemption::begin("Hop is sharing this Mac's keyboard and mouse")
}

/// Nothing to hold outside macOS.
#[cfg(not(target_os = "macos"))]
pub struct Responsive;

#[cfg(not(target_os = "macos"))]
pub fn stay_responsive() -> Responsive {
    Responsive
}
