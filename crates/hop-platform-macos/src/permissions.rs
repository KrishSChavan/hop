//! Permissions, Secure Event Input and the clipboard placeholder.

use hop_platform::{
    Permission, PermissionState, Permissions, PlatformError, Result, SystemState, TextClipboard,
};
use objc2::rc::autoreleasepool;
use objc2_application_services::{AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, kCFBooleanFalse, kCFBooleanTrue};

use crate::ffi;

/// [`Permissions`] for macOS.
pub(crate) struct MacPermissions;

impl Permissions for MacPermissions {
    /// Accessibility is `Granted` or `Denied`: macOS does not say whether a
    /// process that is not trusted was never asked or was refused. Local
    /// Network arrives with the network link (milestone M2).
    fn state(&self, permission: Permission) -> PermissionState {
        match permission {
            Permission::Accessibility => {
                if accessibility_trusted(false) {
                    PermissionState::Granted
                } else {
                    PermissionState::Denied
                }
            }
            Permission::LocalNetwork => PermissionState::NotDetermined,
        }
    }

    /// Accessibility: shows macOS's "Hop would like to control this
    /// computer" prompt, which offers to open System Settings. Ask before
    /// injecting anything: posting first can leave Hop with a grant for
    /// posting events only.
    fn request(&self, permission: Permission) -> Result<()> {
        match permission {
            Permission::Accessibility => {
                accessibility_trusted(true);
                Ok(())
            }
            Permission::LocalNetwork => Err(PlatformError::Unsupported),
        }
    }
}

/// Whether Hop is a trusted Accessibility client; with `prompt`, macOS also
/// asks the user (asynchronously) if it is not.
pub(crate) fn accessibility_trusted(prompt: bool) -> bool {
    // SAFETY: reading immutable CoreFoundation and HIServices constants.
    let (key, yes, no) = unsafe { (kAXTrustedCheckOptionPrompt, kCFBooleanTrue, kCFBooleanFalse) };
    let Some(value) = (if prompt { yes } else { no }) else {
        return false;
    };
    let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[value]);
    // The check runs on whatever thread asks (the runtime may poll it), and
    // such a thread has no autorelease pool; give the framework one, in
    // case it autoreleases anything.
    autoreleasepool(|_| {
        // SAFETY: the dictionary maps the documented key to a CFBoolean,
        // which is the type the function expects.
        unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
    })
}

/// [`SystemState`] for macOS.
pub(crate) struct MacSystem;

impl SystemState for MacSystem {
    fn secure_input_active(&self) -> bool {
        ffi::secure_event_input_enabled()
    }
}

/// The clipboard arrives in milestone M4.
pub(crate) struct MacClipboard;

impl TextClipboard for MacClipboard {
    fn change_count(&self) -> Result<u64> {
        Err(PlatformError::Unsupported)
    }

    fn read_text(&self, _max_bytes: usize) -> Result<Option<String>> {
        Err(PlatformError::Unsupported)
    }

    fn write_text(&mut self, _text: &str) -> Result<u64> {
        Err(PlatformError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_network_waits_for_the_network_link() {
        assert_eq!(
            MacPermissions.state(Permission::LocalNetwork),
            PermissionState::NotDetermined
        );
        assert!(matches!(
            MacPermissions.request(Permission::LocalNetwork),
            Err(PlatformError::Unsupported)
        ));
    }

    #[test]
    fn the_clipboard_is_not_there_yet() {
        let mut clipboard = MacClipboard;
        assert!(matches!(
            clipboard.change_count(),
            Err(PlatformError::Unsupported)
        ));
        assert!(matches!(
            clipboard.read_text(1_048_576),
            Err(PlatformError::Unsupported)
        ));
        assert!(matches!(
            clipboard.write_text("x"),
            Err(PlatformError::Unsupported)
        ));
    }

    /// Reads whether Secure Event Input is on; this changes nothing.
    #[test]
    fn secure_input_can_be_queried() {
        let _ = MacSystem.secure_input_active();
    }
}
