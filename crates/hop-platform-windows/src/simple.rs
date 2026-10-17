//! The parts of the contract that need little or nothing on Windows:
//! displays, permissions, Secure Event Input, and the clipboard (M4).

use hop_platform::{
    Displays, Permission, PermissionState, Permissions, PlatformError, Result, SystemState,
    TextClipboard,
};
use hop_types::DisplayInfo;

use crate::logic::displays::logical_displays;
use crate::os;

/// Monitors as the contract describes displays (see `logic::displays`).
#[derive(Debug, Default)]
pub(crate) struct WinDisplays;

impl Displays for WinDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(logical_displays(&os::monitors()?))
    }
}

/// Windows asks for no permission to capture or inject input, or to use
/// the local network (the firewall prompt for incoming connections comes
/// with networking, in M2).
#[derive(Debug, Default)]
pub(crate) struct WinPermissions;

impl Permissions for WinPermissions {
    fn state(&self, _permission: Permission) -> PermissionState {
        PermissionState::NotNeeded
    }

    fn request(&self, _permission: Permission) -> Result<()> {
        Ok(())
    }
}

/// Windows has no Secure Event Input; password fields do not block
/// low-level hooks.
#[derive(Debug, Default)]
pub(crate) struct WinSystemState;

impl SystemState for WinSystemState {
    fn secure_input_active(&self) -> bool {
        false
    }
}

/// The clipboard arrives in milestone M4.
#[derive(Debug, Default)]
pub(crate) struct NoClipboard;

impl TextClipboard for NoClipboard {
    fn change_count(&self) -> u64 {
        0
    }

    fn read_text(&self) -> Result<Option<String>> {
        Err(PlatformError::Unsupported)
    }

    fn write_text(&mut self, _text: &str) -> Result<()> {
        Err(PlatformError::Unsupported)
    }
}
