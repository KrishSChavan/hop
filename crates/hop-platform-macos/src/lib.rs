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
//! - **Cursor:** parking leaves the cursor where it crossed, disconnects it
//!   from the mouse and hides it (no warp: macOS would add the warp to the
//!   next mouse delta). Hiding from the background
//!   needs a private WindowServer switch; without it the cursor is still
//!   parked, just not hidden. Concealing (the cursor a peer's session left
//!   at the edge) only hides it, and the tap shows it again at the first
//!   physical mouse event.
//! - **Displays:** Quartz display bounds, which are already Hop's
//!   coordinates on a Mac (points, origin at the main display's top-left).
//! - **Permissions:** Accessibility, through `AXIsProcessTrustedWithOptions`.
//!   Ask for it ([`hop_platform::Permissions::request`]) before injecting
//!   anything: posting first can leave Hop with a grant for posting only.
//!   Until it is granted, `InputCapture::start` and every injection return
//!   `PermissionDenied(Accessibility)` without prompting, and nothing is
//!   posted. Local Network arrives in milestone M2.
//! - **Clipboard:** `NSPasteboard.generalPasteboard`, used on a helper
//!   thread of its own, so every call returns within
//!   `hop_platform::CLIPBOARD_TIMEOUT` whatever the app that copied does.
//!   Items a password manager marks as concealed, transient or
//!   auto-generated are never read.
//! - **Drop zone:** while files are dragged near an edge that leads to the
//!   other computer, a thin band of borderless, non-activating panel at the
//!   very edge takes the drop, always as a copy, so icons, the Dock and
//!   windows near the edge keep their own drops. While files are over the
//!   band, a sign beside it, which the mouse passes through, says where
//!   they go. The event tap notices the drag without ever waiting: a serial
//!   helper queue reads the drag pasteboard, and the main dispatch queue
//!   shows and hides the panels.
//! - **Suppression needs capture:** `set_mode(Suppress)` fails unless
//!   capture is running, since only the running tap can swallow input and
//!   end the lease. `stop` (and dropping the capture) also gives a parked
//!   cursor back.
//!
//! # Threads
//!
//! [`native`] and everything in the [`hop_platform::Platform`] it returns
//! work from any thread. The test window needs AppKit's event loop running
//! on the main thread: wrap the work in [`run_with_ui`]. So does the drop
//! zone's panel, which is never waited for: without the event loop it
//! simply never shows.
//!
//! # Privacy
//!
//! Nothing here logs. Captured keys go only to the sink given to
//! `InputCapture::start`, and clipboard text only to the caller that asked
//! for it.

#![cfg(target_os = "macos")]
#![warn(missing_docs)]

mod activity;
mod capture;
mod catcher;
mod clipboard;
mod convert;
mod cursor;
mod displays;
mod ffi;
mod flags;
mod inject;
mod permissions;
#[cfg(test)]
mod private_pasteboard;
mod shared;
mod ui;

pub use activity::AppNapExemption;
pub use ui::{open_test_window, run_with_ui};

use std::sync::Arc;

use hop_platform::{Platform, Result};
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSProcessInfo, NSString};

/// Sets the name AppKit knows this process by. The app menu's "About",
/// "Hide" and "Quit" items use it; without this it is the executable's
/// name, which Hop.app keeps lower-case (`Contents/MacOS/hop`). Call it on
/// the main thread, before the menu is built (before the window's event
/// loop starts): macOS does not make changing the name thread-safe.
pub fn set_process_name(name: &str) {
    autoreleasepool(|_| NSProcessInfo::processInfo().setProcessName(&NSString::from_str(name)));
}

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
        cursor: Box::new(cursor::MacCursor::new(Arc::clone(&shared))),
        displays: Box::new(displays::MacDisplays),
        clipboard: Box::new(clipboard::MacClipboard::new()),
        drops: Box::new(catcher::MacDrops::new(Arc::clone(shared.drops()))),
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
    /// was parked. No event is posted, no tap is created, the clipboard is
    /// never opened and no drop target is made.
    #[test]
    fn the_platform_builds_and_drops_without_touching_input() {
        let mut platform = native().expect("platform");
        assert_eq!(platform.capture.mode(), CaptureMode::Passive);
        assert_eq!(
            platform.permissions.state(Permission::LocalNetwork),
            PermissionState::NotDetermined
        );
        // No zones yet, so clearing them changes nothing, and nothing has
        // been dropped.
        assert!(platform.drops.set_zones(Vec::new(), "").is_ok());
        assert!(platform.drops.take_drops().is_empty());
        drop(platform);
    }
}
