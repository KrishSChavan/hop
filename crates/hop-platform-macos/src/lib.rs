//! Hop's macOS backend. The contract it implements, and the rules it
//! follows, are in the `hop-platform` crate.
//!
//! On other operating systems this crate is empty.
//!
//! # What it uses
//!
//! - **Capture:** a Quartz event tap at the HID level, first in line and
//!   able to swallow events, on a thread with its own run loop. Pointer
//!   motion comes from the events' delta fields, which keep counting when
//!   the cursor is stopped at a screen edge.
//! - **Suppression** is a lease (`hop_platform::SUPPRESS_LEASE`). A timer on
//!   the tap thread checks it every 100 ms; when it lapses, suppression ends
//!   and the cursor is put back where it was parked from and shown again.
//! - **Injection:** events from one private event source, tagged with
//!   `hop_platform::INJECTED_TAG`, posted at the HID level.
//! - **Cursor:** parking warps the cursor to the middle of the main display,
//!   disconnects it from the mouse and hides it. Hiding from the background
//!   needs a private WindowServer switch; without it the cursor is still
//!   parked, just not hidden.
//! - **Displays:** Quartz display bounds, which are already Hop's
//!   coordinates on a Mac (points, origin at the main display's top-left).
//! - **Permissions:** Accessibility, through `AXIsProcessTrustedWithOptions`.
//!   Ask for it ([`hop_platform::Permissions::request`]) before injecting
//!   anything: posting first can leave Hop with a grant for posting only.
//!   Until it is granted, `InputCapture::start` and every injection return
//!   `PermissionDenied(Accessibility)` without prompting, and nothing is
//!   posted. Local Network arrives in milestone M2, the clipboard in M4.
//! - **Suppression needs capture:** `set_mode(Suppress)` fails unless
//!   capture is running, since only the running tap can swallow input and
//!   end the lease. `stop` (and dropping the capture) also gives a parked
//!   cursor back.
//!
//! # Threads
//!
//! [`native`] and everything in the [`hop_platform::Platform`] it returns
//! work from any thread. The test window needs AppKit's event loop running
//! on the main thread: wrap the work in [`run_with_ui`].
//!
//! # Privacy
//!
//! Nothing here logs. Captured keys go only to the sink given to
//! `InputCapture::start`.

#![cfg(target_os = "macos")]
#![warn(missing_docs)]

mod capture;
mod convert;
mod cursor;
mod displays;
mod ffi;
mod flags;
mod inject;
mod permissions;
mod shared;
mod ui;

pub use ui::{open_test_window, run_with_ui};

use hop_platform::{Platform, Result};

/// The macOS implementation of every `hop-platform` trait.
///
/// Creating it touches nothing on screen and asks for no permission: the
/// tap starts with `InputCapture::start`, and events are posted only when
/// injected.
pub fn native() -> Result<Platform> {
    let shared = shared::Shared::new();
    Ok(Platform {
        capture: Box::new(capture::MacCapture::new(shared.clone())),
        injector: Box::new(inject::MacInjector::new()?),
        cursor: Box::new(cursor::MacCursor::new(shared)),
        displays: Box::new(displays::MacDisplays),
        clipboard: Box::new(permissions::MacClipboard),
        permissions: Box::new(permissions::MacPermissions),
        system: Box::new(permissions::MacSystem),
    })
}

#[cfg(test)]
mod tests {
    use hop_platform::{CaptureMode, Permission, PermissionState};

    use super::*;

    /// Builds the whole platform and drops it again. Creating it only makes
    /// a private event source in memory; dropping it releases nothing,
    /// because nothing was pressed, and un-parks nothing, because nothing
    /// was parked. No event is posted and no tap is created.
    #[test]
    fn the_platform_builds_and_drops_without_touching_input() {
        let platform = native().expect("platform");
        assert_eq!(platform.capture.mode(), CaptureMode::Passive);
        assert_eq!(
            platform.permissions.state(Permission::LocalNetwork),
            PermissionState::NotDetermined
        );
        drop(platform);
    }
}
