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
//!   A cursor hidden with [`CursorControl::conceal`] comes back by itself
//!   at the first physical mouse event and when the cursor control is
//!   dropped.
//! - **Never wait on the clipboard.** Every [`TextClipboard`] call returns
//!   within [`CLIPBOARD_TIMEOUT`] or fails, whatever the app that copied
//!   is doing.
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

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hop_types::{DisplayInfo, Edge, InputEvent, Point, Rect};

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

/// The longest a [`TextClipboard`] call may take. Reading the clipboard can
/// wait on the app that copied (macOS asks lazy providers for their data,
/// Windows asks the owner to render delayed formats, for up to 30 seconds),
/// so backends do the work on a thread of their own and give up after this.
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(250);

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

/// Hides and restores the local cursor while we control a peer, and hides
/// the cursor a peer's session left behind.
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
    /// Hide the cursor where it is: the cursor of a peer's session left this
    /// computer through an edge, and would otherwise stay visible there.
    ///
    /// Nothing is frozen, moved or suppressed. The backend shows the cursor
    /// again by itself at the first physical mouse event (motion, a button
    /// or the wheel; keys do not count) and when the cursor control is
    /// dropped, so a stalled runtime never leaves an invisible cursor.
    /// Parking and restoring end a conceal.
    ///
    /// The default returns [`PlatformError::Unsupported`]: the cursor then
    /// simply stays visible.
    fn conceal(&mut self) -> Result<()> {
        Err(PlatformError::Unsupported)
    }
    /// Show a cursor hidden by [`CursorControl::conceal`] again. Does nothing
    /// when it is not hidden, which is all the default does.
    fn reveal(&mut self) -> Result<()> {
        Ok(())
    }
}

pub trait Displays: Send {
    fn displays(&self) -> Result<Vec<DisplayInfo>>;
}

/// Plain-text clipboard access (milestone M4; backends may return
/// [`PlatformError::Unsupported`] until then).
///
/// Every method returns within [`CLIPBOARD_TIMEOUT`] or fails. Text has LF
/// line endings both ways: backends convert from and to the OS's own (see
/// [`to_lf`] and [`to_crlf`]).
pub trait TextClipboard: Send {
    /// A number that changes whenever the clipboard's contents change,
    /// [`TextClipboard::write_text`] included. Cheap to call.
    fn change_count(&self) -> Result<u64>;
    /// The clipboard's text. `None` when the clipboard holds no text, when
    /// the text is longer than `max_bytes` of UTF-8 (it is never cut short),
    /// or when the item comes from a password manager (marked concealed,
    /// transient or auto-generated), whose text is never read.
    fn read_text(&self, max_bytes: usize) -> Result<Option<String>>;
    /// Puts `text` on the clipboard and returns the change count right after
    /// the write, so the caller can tell its own write from the next copy.
    fn write_text(&mut self, text: &str) -> Result<u64>;
}

/// `text` with every CRLF and every lone CR made LF, the line ending
/// clipboard text has inside Hop. Borrowed when nothing changes.
pub fn to_lf(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    let mut lf = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            // CRLF is one line ending, not two.
            chars.next_if_eq(&'\n');
            lf.push('\n');
        } else {
            lf.push(c);
        }
    }
    Cow::Owned(lf)
}

/// `text` with CRLF line endings, as Windows expects on its clipboard. Line
/// endings are made LF first, so a CRLF already there is never doubled.
pub fn to_crlf(text: &str) -> String {
    to_lf(text).replace('\n', "\r\n")
}

/// The part of one display's edge that leads to the other computer, where
/// files dragged there can be dropped (milestone M5).
///
/// In logical coordinates. `start..end` runs along the edge: y for a left or
/// right edge, x for a top or bottom edge.
#[derive(Clone, Debug, PartialEq)]
pub struct DropZone {
    /// The display's frame.
    pub display: Rect,
    /// The edge of `display` the zone lies along.
    pub edge: Edge,
    /// Where the zone starts along the edge.
    pub start: f64,
    /// Where the zone ends along the edge, after `start`.
    pub end: f64,
}

/// Catches files dropped on the edge that leads to the other computer
/// (milestone M5).
///
/// The drop target shows only while files are dragged near a zone, so it
/// never gets in the way of ordinary clicks or of window snapping at the
/// edge. It always accepts a drop as a copy, never a move: the files stay
/// where they were. Drops wait here until [`DropCatcher::take_drops`]
/// collects them.
pub trait DropCatcher: Send {
    /// Catch drops on `zones` from now on, in place of any zones set before,
    /// with `label` on the target (such as "Drop to send to Windows PC"). An
    /// empty list stops catching.
    fn set_zones(&mut self, zones: Vec<DropZone>, label: &str) -> Result<()>;
    /// The files dropped since the last call: one list of paths per drop,
    /// oldest drop first.
    fn take_drops(&mut self) -> Vec<Vec<PathBuf>>;
}

/// The [`DropCatcher`] of backends and fakes without a drop target:
/// clearing the zones works, setting any fails with
/// [`PlatformError::Unsupported`], and nothing is ever dropped.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDrops;

impl DropCatcher for NoDrops {
    fn set_zones(&mut self, zones: Vec<DropZone>, _label: &str) -> Result<()> {
        if zones.is_empty() {
            Ok(())
        } else {
            Err(PlatformError::Unsupported)
        }
    }

    fn take_drops(&mut self) -> Vec<Vec<PathBuf>> {
        Vec::new()
    }
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
    /// Files dropped on the edge that leads to the other computer.
    pub drops: Box<dyn DropCatcher>,
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

    #[test]
    fn line_endings_become_lf() {
        assert!(matches!(to_lf(""), Cow::Borrowed("")));
        assert!(matches!(to_lf("no line break"), Cow::Borrowed(_)));
        assert!(matches!(to_lf("one\ntwo\n"), Cow::Borrowed(_)));
        assert_eq!(to_lf("one\r\ntwo\r\n"), "one\ntwo\n");
        // A lone CR, as old Mac apps wrote it.
        assert_eq!(to_lf("one\rtwo\r"), "one\ntwo\n");
        // Mixed, including a lone CR right before a CRLF.
        assert_eq!(to_lf("a\r\nb\nc\rd\r\r\ne"), "a\nb\nc\nd\n\ne");
    }

    #[test]
    fn line_endings_become_crlf_and_are_never_doubled() {
        assert_eq!(to_crlf(""), "");
        assert_eq!(to_crlf("no line break"), "no line break");
        assert_eq!(to_crlf("one\ntwo\n"), "one\r\ntwo\r\n");
        assert_eq!(to_crlf("one\rtwo"), "one\r\ntwo");
        assert_eq!(to_crlf("a\r\nb\nc\rd"), "a\r\nb\r\nc\r\nd");
        // Text that already has CRLF keeps it, there and back again.
        let windows = "first line\r\nsecond line\r\n";
        assert_eq!(to_crlf(windows), windows);
        assert_eq!(to_crlf(&to_lf(windows)), windows);
        assert_eq!(to_lf(&to_crlf("first\nsecond")), "first\nsecond");
    }

    /// A cursor control with only the methods every backend must write.
    struct PlainCursor;

    impl CursorControl for PlainCursor {
        fn position(&self) -> Result<Point> {
            Ok(Point::default())
        }

        fn park(&mut self) -> Result<()> {
            Ok(())
        }

        fn restore(&mut self, _at: Point) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_backend_that_cannot_conceal_the_cursor_says_so() {
        let mut cursor = PlainCursor;
        assert!(matches!(cursor.conceal(), Err(PlatformError::Unsupported)));
        assert!(cursor.reveal().is_ok(), "nothing to show again");
    }

    #[test]
    fn without_a_drop_target_only_clearing_the_zones_works() {
        let mut drops = NoDrops;
        assert!(
            drops
                .set_zones(Vec::new(), "Drop to send to Windows PC")
                .is_ok()
        );
        let zone = DropZone {
            display: Rect::new(0.0, 0.0, 1512.0, 982.0),
            edge: Edge::Right,
            start: 59.0,
            end: 923.0,
        };
        assert!(matches!(
            drops.set_zones(vec![zone], "Drop to send to Windows PC"),
            Err(PlatformError::Unsupported)
        ));
        assert!(drops.take_drops().is_empty());
    }
}
