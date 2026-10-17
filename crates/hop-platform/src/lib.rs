//! The contract between Hop and the operating system.
//!
//! This crate defines the traits the rest of Hop programs against. The
//! backends implement them: `hop-platform-macos` (Quartz event taps and event
//! posting, AppKit) and `hop-platform-windows` (low-level hooks, Raw Input,
//! SendInput, Win32 windows). Everything above the backends is
//! OS-independent.
//!
//! # Rules every backend follows
//!
//! - **Fail open.** Suppressing local input is a lease ([`SUPPRESS_LEASE`])
//!   that the runtime must keep renewing. When it lapses, the backend goes
//!   back to passive capture and shows and frees the cursor by itself, so a
//!   hang anywhere in Hop can never leave the user without a mouse or
//!   keyboard. Dropping an injector releases every key and button it holds.
//! - **Tag what Hop injects.** Every injected event carries [`INJECTED_TAG`]
//!   (macOS: `kCGEventSourceUserData`; Windows: `dwExtraInfo`), and capture
//!   reports tagged events with `physical: false`. Suppression never
//!   swallows tagged events.
//! - **Report attempted motion.** At a screen edge the OS stops the cursor,
//!   but the user is still pushing. Captured motion must carry the delta the
//!   user attempted (macOS: the event's delta fields; Windows: Raw Input
//!   deltas when the hook's position stops changing), so the core can tell a
//!   push past the edge from standing still.
//! - **Never block the capture thread.** Callbacks hand events off and
//!   return; Windows silently removes a low-level hook that takes too long.
//! - **Never log keystrokes, clipboard contents or file contents.**
//!
//! # Coordinates
//!
//! Logical units with the origin at the top-left of the primary display and
//! y growing downward. macOS uses Quartz global coordinates in points.
//! Windows uses physical pixels divided by the primary display's scale, so
//! the whole desktop scales uniformly and adjacent monitors stay adjacent.

use std::fmt;
use std::time::{Duration, Instant};

use hop_types::{DisplayInfo, InputEvent, Point};

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

/// Marks events Hop injects, so capture can tell them from physical input.
/// The bytes spell "HOP!".
pub const INJECTED_TAG: i64 = 0x484F_5021;

/// How long [`CaptureMode::Suppress`] lasts unless it is renewed.
pub const SUPPRESS_LEASE: Duration = Duration::from_millis(1000);

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
    /// This OS has no such permission.
    NotNeeded,
}

/// How captured physical input is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// Observe only: events still reach local apps.
    Passive,
    /// Swallow physical events so local apps don't see them (we are
    /// controlling a peer). A lease: see [`SUPPRESS_LEASE`].
    Suppress,
}

/// One captured input event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturedEvent {
    pub event: InputEvent,
    /// The cursor position *before* this event, in logical coordinates: the
    /// position after the previous event. For motion, `cursor + (dx, dy)` is
    /// where the user tried to go, even when the OS stopped the real cursor
    /// at a screen edge.
    pub cursor: Point,
    /// False for events Hop injected itself (tagged with [`INJECTED_TAG`]).
    pub physical: bool,
}

/// Captures local input on a thread of its own.
pub trait InputCapture: Send {
    /// Start delivering events to `sink`, which runs on the capture thread
    /// and must only hand events off.
    fn start(&mut self, sink: Box<dyn FnMut(CapturedEvent) + Send>) -> Result<()>;
    /// `Passive` is permanent. `Suppress` lasts [`SUPPRESS_LEASE`] and is
    /// renewed by calling this again; when it lapses the backend returns to
    /// `Passive` and shows and frees the cursor by itself.
    ///
    /// A lapse means something in Hop stalled, and the cursor has already
    /// been given back. Before renewing, the runtime checks
    /// [`InputCapture::mode`]: if it reads `Passive` while the core still
    /// believes it is controlling a peer, the runtime ends that session
    /// instead of starting a new lease.
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()>;
    /// The mode in effect now, after any lapse.
    fn mode(&self) -> CaptureMode;
    fn stop(&mut self);
}

/// Injects input into the local OS, tagged with [`INJECTED_TAG`].
///
/// Dropping it releases every key and button it still holds down.
pub trait InputInjector: Send {
    /// Move the cursor to an absolute position in logical coordinates. While
    /// a button injected by this injector is held, this is a drag.
    fn move_cursor_to(&mut self, to: Point) -> Result<()>;
    /// Inject a button, key or scroll event. Motion goes through
    /// [`InputInjector::move_cursor_to`]. Keys are converted with
    /// `hop-keymap`, and keys with no code on this OS are skipped.
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
    /// Release every key and button this injector holds down.
    fn release_all(&mut self) -> Result<()>;
}

/// Hides and restores the local cursor while we control a peer.
pub trait CursorControl: Send {
    fn position(&self) -> Result<Point>;
    /// Hide the cursor and pin it so it can't drift toward an edge, whatever
    /// the capture mode; relative motion keeps arriving through capture.
    ///
    /// The core always parks together with [`CaptureMode::Suppress`]. The
    /// backend gives a parked cursor back by itself when that lease lapses
    /// and when the cursor control is dropped. The Windows backend also
    /// frees a park that no lease has covered for 5 seconds.
    fn park(&mut self) -> Result<()>;
    /// Show the cursor again at `at`.
    fn restore(&mut self, at: Point) -> Result<()>;
}

pub trait Displays: Send {
    fn displays(&self) -> Result<Vec<DisplayInfo>>;
}

/// Plain-text clipboard access (milestone M4; backends may return
/// [`PlatformError::Unsupported`] until then).
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

pub trait SystemState: Send {
    /// macOS Secure Event Input is on (a password field has focus), so the
    /// keyboard cannot be captured. Always false where it doesn't exist.
    fn secure_input_active(&self) -> bool;
}

/// A small window Hop opens during `hop selftest` to check injection
/// without touching any other app.
pub trait TestWindow: Send {
    /// Show the window with an instruction line, a text field and a button,
    /// and give the text field keyboard focus.
    fn show(&mut self, instructions: &str) -> Result<()>;
    fn set_instructions(&mut self, text: &str) -> Result<()>;
    fn focus_text(&mut self) -> Result<()>;
    fn text(&self) -> Result<String>;
    fn clear_text(&mut self) -> Result<()>;
    /// The button's center in logical coordinates.
    fn button_center(&self) -> Result<Point>;
    /// How many times the button has been clicked since the window opened.
    fn button_clicks(&self) -> Result<u32>;
    fn close(&mut self);
}

/// Everything a backend provides.
pub struct Platform {
    pub capture: Box<dyn InputCapture>,
    pub injector: Box<dyn InputInjector>,
    pub cursor: Box<dyn CursorControl>,
    pub displays: Box<dyn Displays>,
    pub clipboard: Box<dyn TextClipboard>,
    pub permissions: Box<dyn Permissions>,
    pub system: Box<dyn SystemState>,
}

/// Lease bookkeeping for [`CaptureMode::Suppress`], shared by the backends.
#[derive(Clone, Copy, Debug, Default)]
pub struct SuppressLease {
    expires: Option<Instant>,
}

impl SuppressLease {
    /// Start or extend the lease from `now`.
    pub fn renew(&mut self, now: Instant) {
        self.expires = Some(now + SUPPRESS_LEASE);
    }

    /// End the lease now.
    pub fn clear(&mut self) {
        self.expires = None;
    }

    /// Whether suppression is in force at `now`.
    pub fn active(&self, now: Instant) -> bool {
        self.expires.is_some_and(|t| now < t)
    }

    /// The lease was renewed at some point but has since run out: the
    /// backend must go back to passive and show the cursor.
    pub fn lapsed(&self, now: Instant) -> bool {
        self.expires.is_some_and(|t| now >= t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_runs_out_unless_renewed() {
        let start = Instant::now();
        let mut lease = SuppressLease::default();
        assert!(!lease.active(start) && !lease.lapsed(start));
        lease.renew(start);
        assert!(lease.active(start + SUPPRESS_LEASE / 2));
        assert!(lease.lapsed(start + SUPPRESS_LEASE));
        lease.renew(start + SUPPRESS_LEASE / 2);
        assert!(lease.active(start + SUPPRESS_LEASE));
        lease.clear();
        assert!(!lease.active(start) && !lease.lapsed(start + SUPPRESS_LEASE * 2));
    }

    #[test]
    fn the_tag_spells_hop() {
        assert_eq!(&INJECTED_TAG.to_be_bytes()[4..], b"HOP!");
    }
}
