//! OS integration for Hop: the only crate that touches OS input, cursor,
//! display, clipboard and permission APIs.
//!
//! This file defines the traits the rest of Hop programs against. The macOS
//! implementation (Quartz event taps and event posting, AppKit) and the
//! Windows implementation (low-level hooks, SendInput, the Win32 clipboard)
//! arrive in milestone M1. Everything above this crate is OS-independent.

use std::fmt;

use hop_types::{DisplayInfo, InputEvent, MouseButton, Point};

pub type Result<T> = std::result::Result<T, PlatformError>;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("not supported on this platform yet")]
    Unsupported,
    #[error("permission missing: {0}")]
    PermissionDenied(Permission),
    #[error("{0}")]
    Os(String),
}

/// OS permissions Hop may need.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Permission {
    /// macOS Privacy & Security > Accessibility: capture and inject input.
    Accessibility,
    /// macOS Privacy & Security > Local Network: find and reach the other device.
    LocalNetwork,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Permission::Accessibility => "Accessibility",
            Permission::LocalNetwork => "Local Network",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
}

/// How captured physical input is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// Observe only: events still reach local apps.
    Passive,
    /// Swallow physical events so local apps don't see them (we are
    /// controlling a peer). Events Hop injects itself always pass through.
    Suppress,
}

/// One captured input event and the cursor position right after it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturedEvent {
    pub event: InputEvent,
    pub cursor: Point,
    /// False for events Hop injected itself.
    pub physical: bool,
}

/// Captures local input. Implementations run their own thread and must
/// never block it: callbacks only hand events off.
pub trait InputCapture: Send {
    /// Start delivering events to `sink`.
    fn start(&mut self, sink: Box<dyn FnMut(CapturedEvent) + Send>) -> Result<()>;
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()>;
    fn stop(&mut self);
}

/// Injects input into the local OS.
pub trait InputInjector: Send {
    /// Move the cursor to an absolute position, as a drag while any of
    /// `buttons_down` is held.
    fn move_cursor_to(&mut self, to: Point, buttons_down: &[MouseButton]) -> Result<()>;
    /// Inject a button, key or scroll event. Motion goes through
    /// [`InputInjector::move_cursor_to`].
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Hides and restores the local cursor while we control a peer.
pub trait CursorControl: Send {
    fn position(&self) -> Result<Point>;
    /// Hide the cursor and keep it from drifting toward any screen edge.
    fn park(&mut self) -> Result<()>;
    /// Show the cursor again at `at`.
    fn restore(&mut self, at: Point) -> Result<()>;
}

pub trait Displays: Send {
    fn displays(&self) -> Result<Vec<DisplayInfo>>;
}

/// Plain-text clipboard access.
pub trait TextClipboard: Send {
    /// Changes whenever the clipboard contents change; cheap to call.
    fn change_count(&self) -> u64;
    /// The clipboard text, or `None` if it holds no text or the item is
    /// marked as concealed (password managers).
    fn read_text(&self) -> Result<Option<String>>;
    fn write_text(&mut self, text: &str) -> Result<()>;
}

pub trait Permissions: Send {
    fn state(&self, permission: Permission) -> PermissionState;
    /// Show the OS permission prompt, or open the matching Settings pane.
    fn request(&self, permission: Permission) -> Result<()>;
}
