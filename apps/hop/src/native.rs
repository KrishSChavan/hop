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
