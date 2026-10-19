//! Catching files dropped on the edge that leads to the other computer
//! (docs/M4-M5-DESIGN.md, "The drop zone").
//!
//! The drop target is a borderless, non-activating panel along the zone.
//! It shows only while files are dragged near the zone, so it never gets in
//! the way of ordinary clicks or of window snapping at the edge. Three
//! threads take part, and none of them ever waits for another:
//!
//! - **The event tap** watches the left button ([`TapDrops`]), Hop's own
//!   injected presses included: the other computer may be dragging a file
//!   here. At a press it asks the helper queue to note the drag
//!   pasteboard's change count; while the button is dragged it asks, at
//!   most every 50 ms, whether a drag of files has started since. Once one
//!   has, it compares the cursor with the zones (plain arithmetic) and asks
//!   the main thread to show the target within 150 points of a zone, to
//!   hide it beyond 300, and to hide it about a second after the button
//!   goes up, which leaves time for a drop on it to land.
//! - **A serial helper queue** reads the drag pasteboard. That means asking
//!   the pasteboard server, which can take a moment, and the tap must never
//!   wait.
//! - **The main thread** owns the panels (AppKit allows nothing else) and
//!   gets its work through the main dispatch queue, never waited on. The
//!   app and `hop run` both run the main event loop that serves it. The
//!   panel's view takes the drop: it reads the dropped file URLs into the
//!   queue that [`DropCatcher::take_drops`] empties, and hides the panel.
//!   Only file URLs count: no web links, and no file promises (Photos and
//!   Mail drag those).
//!
//! The target answers every drag with the copy operation, so a drop never
//! moves or deletes the files it came from.
//!
//! Nothing here logs. File names leave this module only through
//! `take_drops`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError, Weak};
use std::time::{Duration, Instant};

use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained, DispatchTime};
use hop_platform::{DropCatcher, DropZone, Result};
use hop_types::{Edge, Point, Rect};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSDragOperation, NSDraggingDestination,
    NSDraggingInfo, NSFont, NSPanel, NSPasteboard, NSPasteboardNameDrag, NSPasteboardTypeFileURL,
    NSPasteboardURLReadingFileURLsOnlyKey, NSResponder, NSStatusWindowLevel, NSTextAlignment,
    NSTextField, NSView, NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::{CGEvent, CGEventType};
use objc2_foundation::{
    NSArray, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    NSURL,
};

use crate::displays::main_display_frame;
use crate::shared::lock;
use crate::ui::MainOnly;

/// A drag of files this close to a zone, in points, shows the target.
const SHOW_WITHIN: f64 = 150.0;

/// A drag this far from every zone, in points, hides the target again.
/// Between the two distances it stays as it is, so it never flickers.
const HIDE_BEYOND: f64 = 300.0;

/// How often, at most, a drag asks the helper whether it carries files.
const CHECK_EVERY: Duration = Duration::from_millis(50);

/// How long the target stays after the button goes up. macOS hands a drop
/// to the target a moment after the button is let go.
const LINGER: Duration = Duration::from_secs(1);

/// How far the target reaches into the display from the edge, in points.
const DEPTH: f64 = 120.0;

/// The space between the target's edges and its rounded strip, in points.
const STRIP_INSET: f64 = 8.0;

/// The space between the target's edges and its label, in points.
const LABEL_INSET: f64 = 16.0;

/// The label's font size, in points.
const LABEL_SIZE: f64 = 13.0;

/// Finds the pasteboard drags go through: the system's, except in tests.
type Opener = dyn Fn() -> Option<Retained<NSPasteboard>> + Send + Sync;

/// [`DropCatcher`] for macOS.
pub(crate) struct MacDrops {
    state: Arc<DropState>,
}

impl MacDrops {
    /// A catcher working with the event tap through `state`. It catches
    /// nothing until zones are set.
    pub(crate) fn new(state: Arc<DropState>) -> Self {
        Self { state }
    }
}

impl DropCatcher for MacDrops {
    /// Never fails: the zones take effect at the tap's next event, and the
    /// target is made the first time it is needed.
    fn set_zones(&mut self, zones: Vec<DropZone>, label: &str) -> Result<()> {
        self.state.set_zones(zones, label);
        Ok(())
    }

    fn take_drops(&mut self) -> Vec<Vec<PathBuf>> {
        lock(&self.state.dropped).drain(..).collect()
    }
}

impl Drop for MacDrops {
    /// Stops catching and takes the target down.
    fn drop(&mut self) {
        self.state.set_zones(Vec::new(), "");
    }
}

/// What the drop catcher, the event tap, the helper queue and the main
/// thread share. One per platform, in `Shared`.
pub(crate) struct DropState {
    /// The zones in force, replaced whole by `set_zones`.
    zones: Mutex<Arc<Zones>>,
    /// The version of the zones in force, so the tap can tell that its copy
    /// is out of date without waiting for the lock above.
    version: AtomicU64,
    /// Drops not collected yet, oldest first.
    dropped: Mutex<VecDeque<Vec<PathBuf>>>,
    /// The press whose drag the helper found carrying files; 0 for none.
    file_drag: AtomicU64,
    /// Counts the taps that watched for drops, so each numbers its presses
    /// apart from the ones before it.
    taps: AtomicU64,
    /// A check is waiting on the helper queue, so the tap adds no other.
    checking: AtomicBool,
    /// What the helper queue knows about the drag pasteboard.
    probe: Mutex<DragProbe>,
    /// Finds the drag pasteboard, on the helper queue.
    pasteboard: Arc<Opener>,
    /// The serial helper queue, made when first needed.
    helper: OnceLock<DispatchRetained<DispatchQueue>>,
    /// The panels, made on the main thread when first needed.
    panels: OnceLock<MainOnly<RefCell<Panels>>>,
    /// Counts the times the target was shown or took a drop (on the main
    /// thread): a hide planned at a mouse-up is called off if either
    /// happened since.
    generation: AtomicU64,
}

impl Default for DropState {
    fn default() -> Self {
        Self::with_pasteboard(drag_pasteboard)
    }
}

impl fmt::Debug for DropState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the dropped paths: they name the user's files.
        f.debug_struct("DropState")
            .field("version", &self.version.load(Ordering::Relaxed))
            .field("file_drag", &self.file_drag.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The zones `set_zones` gave, and the label for their target.
#[derive(Debug, Default, PartialEq)]
struct Zones {
    /// Counts the changes; 0 before the first.
    version: u64,
    zones: Vec<DropZone>,
    label: String,
}

impl DropState {
    /// State whose helper queue reads the pasteboard `open` finds.
    fn with_pasteboard(
        open: impl Fn() -> Option<Retained<NSPasteboard>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            zones: Mutex::default(),
            version: AtomicU64::new(0),
            dropped: Mutex::default(),
            file_drag: AtomicU64::new(0),
            taps: AtomicU64::new(0),
            checking: AtomicBool::new(false),
            probe: Mutex::default(),
            pasteboard: Arc::new(open),
            helper: OnceLock::new(),
            panels: OnceLock::new(),
            generation: AtomicU64::new(0),
        }
    }

    /// Puts `zones` and `label` in force, unless they are already. Zones
    /// that are empty, back to front or not finite are left out. The target
    /// is taken down, since it was made for the zones before.
    fn set_zones(self: &Arc<Self>, zones: Vec<DropZone>, label: &str) {
        let zones: Vec<DropZone> = zones.into_iter().filter(usable).collect();
        let mut current = lock(&self.zones);
        if current.zones == zones && current.label == label {
            return;
        }
        let version = current.version + 1;
        *current = Arc::new(Zones {
            version,
            zones,
            label: label.to_owned(),
        });
        self.version.store(version, Ordering::Release);
        drop(current);
        self.on_main(|_, panels, _| panels.clear());
    }

    /// Queues the files of one drop for `take_drops`. A drop without files
    /// is no drop; returns whether there were any.
    fn add_drop(&self, paths: Vec<PathBuf>) -> bool {
        if paths.is_empty() {
            return false;
        }
        lock(&self.dropped).push_back(paths);
        true
    }

    /// Hands what one event asks for to the helper queue and the main
    /// thread, without waiting for either. `zones` are the tap's copy, the
    /// ones the request's zone index refers to.
    fn carry_out(self: &Arc<Self>, requests: Requests, zones: &Arc<Zones>) {
        self.ask_helper(requests);
        if requests.hide {
            self.on_main(|_, panels, _| panels.hide());
        }
        if let Some(index) = requests.show {
            let zones = Arc::clone(zones);
            self.on_main(move |state, panels, mtm| panels.show(state, &zones, index, mtm));
        }
        if requests.hide_later {
            self.on_main(|state, _, _| state.hide_later());
        }
    }

    /// The helper queue's part of `requests`.
    fn ask_helper(self: &Arc<Self>, requests: Requests) {
        if let Some(press) = requests.snapshot {
            self.on_helper(move |state, pasteboard| state.note_press(press, pasteboard));
        }
        // One check at a time: a slow pasteboard server must not pile them
        // up, and the tap asks again soon enough.
        if let Some(press) = requests.check
            && !self.checking.swap(true, Ordering::AcqRel)
        {
            let done = Checked(Arc::downgrade(self));
            self.on_helper(move |state, pasteboard| {
                let _done = done;
                state.check_drag(press, pasteboard);
            });
        }
    }

    /// Runs `work` with the drag pasteboard on the helper queue, later.
    fn on_helper(self: &Arc<Self>, work: impl FnOnce(&DropState, &NSPasteboard) + Send + 'static) {
        let state = Arc::downgrade(self);
        let helper = self
            .helper
            .get_or_init(|| DispatchQueue::new("hop-drop-drag", DispatchQueueAttr::SERIAL));
        helper.exec_async(move || {
            // A panic must not unwind into libdispatch, which would abort.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                autoreleasepool(|_| {
                    if let Some(pasteboard) = (state.pasteboard)() {
                        work(&state, &pasteboard);
                    }
                });
            }));
        });
    }

    /// On the helper queue: press `press` began.
    fn note_press(&self, press: u64, pasteboard: &NSPasteboard) {
        lock(&self.probe).begin(press, pasteboard.changeCount());
    }

    /// On the helper queue: whether the drag of press `press` carries files,
    /// published for the tap in `file_drag`.
    fn check_drag(&self, press: u64, pasteboard: &NSPasteboard) {
        let carries = lock(&self.probe).carries_files(press, pasteboard.changeCount(), || {
            has_file_urls(pasteboard)
        });
        if carries {
            self.file_drag.store(press, Ordering::Release);
        }
    }

    /// Runs `f` with the panels on the main thread, later.
    fn on_main(
        self: &Arc<Self>,
        f: impl FnOnce(&Arc<DropState>, &mut Panels, MainThreadMarker) + Send + 'static,
    ) {
        let state = Arc::downgrade(self);
        DispatchQueue::main().exec_async(move || {
            // A panic must not unwind into libdispatch, which would abort.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                // SAFETY: work on the main queue runs on the main thread.
                let mtm = unsafe { MainThreadMarker::new_unchecked() };
                state.with_panels(mtm, f);
            }));
        });
    }

    /// Runs `f` with the panels, on the main thread. Should AppKit call back
    /// into the target while the panels are in use, it finds them busy and
    /// leaves them be.
    fn with_panels(
        self: &Arc<Self>,
        mtm: MainThreadMarker,
        f: impl FnOnce(&Arc<DropState>, &mut Panels, MainThreadMarker),
    ) {
        let panels = self
            .panels
            .get_or_init(|| MainOnly::new(RefCell::new(Panels::default()), mtm));
        if let Some(Ok(mut panels)) = panels.get(mtm).map(RefCell::try_borrow_mut) {
            f(self, &mut panels, mtm);
        }
    }

    /// On the main thread: hides the target after [`LINGER`], unless it took
    /// a drop or was shown again by then.
    fn hide_later(self: &Arc<Self>) {
        let generation = self.generation.load(Ordering::Acquire);
        let state = Arc::downgrade(self);
        let when = DispatchTime::try_from(LINGER).unwrap_or(DispatchTime::NOW);
        let _ = DispatchQueue::main().after(when, move || {
            // A panic must not unwind into libdispatch, which would abort.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                if state.generation.load(Ordering::Acquire) != generation {
                    return;
                }
                // SAFETY: work on the main queue runs on the main thread.
                let mtm = unsafe { MainThreadMarker::new_unchecked() };
                state.with_panels(mtm, |_, panels, _| panels.hide());
            }));
        });
    }
}

/// Marks the check done when the helper has run it (or dropped it), so the
/// tap can ask again.
struct Checked(Weak<DropState>);

impl Drop for Checked {
    fn drop(&mut self) {
        if let Some(state) = self.0.upgrade() {
            state.checking.store(false, Ordering::Release);
        }
    }
}

/// Whether a zone can be used: finite, on a display with an area, and
/// running forward along the edge.
fn usable(zone: &DropZone) -> bool {
    let display = &zone.display;
    let numbers = [
        display.origin.x,
        display.origin.y,
        display.size.width,
        display.size.height,
        zone.start,
        zone.end,
    ];
    numbers.iter().all(|n| n.is_finite())
        && display.size.width > 0.0
        && display.size.height > 0.0
        && zone.end > zone.start
}

// ----- The event tap's part ---------------------------------------------------------------

/// The left button, as far as dropping files goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Left {
    Down,
    Dragged,
    Up,
}

/// The left-button event an event of type `ty` is, if any.
fn left_button(ty: CGEventType) -> Option<Left> {
    match ty {
        CGEventType::LeftMouseDown => Some(Left::Down),
        CGEventType::LeftMouseDragged => Some(Left::Dragged),
        CGEventType::LeftMouseUp => Some(Left::Up),
        _ => None,
    }
}

/// What one event asks of the helper queue and the main thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Requests {
    /// Note the drag pasteboard's change count as this press begins.
    snapshot: Option<u64>,
    /// Find out whether the drag of this press carries files.
    check: Option<u64>,
    /// Show the target on the zone with this index (in the tap's copy).
    show: Option<usize>,
    /// Take the target down now.
    hide: bool,
    /// Take the target down after [`LINGER`], unless it takes a drop or is
    /// shown again first.
    hide_later: bool,
}

/// The event tap's part in catching drops. It lives on the tap thread and
/// only ever does arithmetic and hands work on: it never waits.
pub(crate) struct TapDrops {
    /// The tap's copy of the zones in force.
    zones: Arc<Zones>,
    /// Numbers the left presses: the current or last one.
    press: u64,
    /// The left button is down, and the tap saw it go down.
    held: bool,
    /// When the helper was last asked about the drag of this press.
    checked_at: Option<Instant>,
    /// The zone the target was asked to show on, until asked to hide.
    target: Option<usize>,
}

impl TapDrops {
    /// A new tap's part, with no zones yet. Its presses are numbered apart
    /// from those of every tap before it on `state`, so an answer the helper
    /// gave about an earlier tap's press is never taken for one of its own.
    pub(crate) fn new(state: &DropState) -> Self {
        Self::numbering_from(state.taps.fetch_add(1, Ordering::Relaxed) << 32)
    }

    /// Watching for drops with no zones yet, numbering presses from just
    /// after `press`.
    fn numbering_from(press: u64) -> Self {
        Self {
            zones: Arc::default(),
            press,
            held: false,
            checked_at: None,
            target: None,
        }
    }

    /// Handles one event of type `ty` that reaches the apps (physical or
    /// Hop's own), seen at `now`.
    pub(crate) fn on_event(
        &mut self,
        state: &Arc<DropState>,
        ty: CGEventType,
        event: &CGEvent,
        now: Instant,
    ) {
        let Some(left) = left_button(ty) else {
            return;
        };
        self.refresh(state);
        let location = CGEvent::location(Some(event));
        let at = Point::new(location.x, location.y);
        let requests = self.step(left, at, now, state.file_drag.load(Ordering::Acquire));
        if requests != Requests::default() {
            state.carry_out(requests, &self.zones);
        }
    }

    /// Takes the zones in force if they changed, unless another thread is
    /// setting them right now: the next event looks again.
    fn refresh(&mut self, state: &DropState) {
        if state.version.load(Ordering::Acquire) == self.zones.version {
            return;
        }
        let zones = match state.zones.try_lock() {
            Ok(zones) => Arc::clone(&zones),
            // As `lock` does: carry on after a panic elsewhere.
            Err(TryLockError::Poisoned(poisoned)) => Arc::clone(&poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => return,
        };
        self.zones = zones;
        // A target shown for the old zones is gone: `set_zones` took it
        // down.
        self.target = None;
    }

    /// What a left-button event at `at`, at `now`, asks for. `file_drag` is
    /// the press whose drag the helper found carrying files.
    fn step(&mut self, left: Left, at: Point, now: Instant, file_drag: u64) -> Requests {
        let mut requests = Requests::default();
        let watching = !self.zones.zones.is_empty();
        match left {
            Left::Down => {
                self.press += 1;
                self.held = true;
                self.checked_at = None;
                if watching {
                    requests.snapshot = Some(self.press);
                }
            }
            Left::Dragged => {
                // A drag whose press the tap missed has nothing to compare
                // the drag pasteboard with.
                if !self.held || !watching {
                    return requests;
                }
                if file_drag != self.press {
                    if self
                        .checked_at
                        .is_none_or(|checked| now.duration_since(checked) >= CHECK_EVERY)
                    {
                        requests.check = Some(self.press);
                        self.checked_at = Some(now);
                    }
                    return requests;
                }
                match nearest(&self.zones.zones, at) {
                    Some((zone, distance)) if distance <= SHOW_WITHIN => {
                        if self.target != Some(zone) {
                            requests.show = Some(zone);
                            self.target = Some(zone);
                        }
                    }
                    Some((_, distance)) if distance <= HIDE_BEYOND => {}
                    _ => requests.hide = self.target.take().is_some(),
                }
            }
            Left::Up => {
                self.held = false;
                requests.hide_later = self.target.take().is_some();
            }
        }
        requests
    }
}

// ----- The helper queue's part ------------------------------------------------------------

/// What the helper queue knows about the drag pasteboard.
#[derive(Debug, Default)]
struct DragProbe {
    /// The press under way and the drag pasteboard's change count when it
    /// began.
    start: Option<(u64, isize)>,
}

impl DragProbe {
    /// Press `press` began with the drag pasteboard at change count `count`.
    fn begin(&mut self, press: u64, count: isize) {
        self.start = Some((press, count));
    }

    /// Whether the drag of press `press` carries files, now that the drag
    /// pasteboard is at change count `count`: a drag has started since the
    /// press began (the pasteboard changed), and `has_files` says it holds
    /// file URLs. Whatever an earlier drag left on the pasteboard does not
    /// count, and neither does a press this probe did not see begin.
    fn carries_files(&self, press: u64, count: isize, has_files: impl FnOnce() -> bool) -> bool {
        self.start
            .is_some_and(|(began, before)| began == press && before != count)
            && has_files()
    }
}

/// The pasteboard drags go through. It can be missing: a process without a
/// window-server session has none.
fn drag_pasteboard() -> Option<Retained<NSPasteboard>> {
    // SAFETY: reading an immutable AppKit constant.
    let name = unsafe { NSPasteboardNameDrag };
    // SAFETY: `pasteboardWithName:` takes a pasteboard name and returns that
    // pasteboard, or nil, which the `Option` covers.
    unsafe { msg_send![NSPasteboard::class(), pasteboardWithName: name] }
}

/// Whether `pasteboard` holds file URLs (`public.file-url`).
fn has_file_urls(pasteboard: &NSPasteboard) -> bool {
    // SAFETY: reading an immutable AppKit constant.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    pasteboard
        .types()
        .is_some_and(|types| types.iter().any(|ty| ty.isEqualToString(file_url)))
}

/// The paths of the file URLs on `pasteboard`, in order. Web links and
/// file promises are left out; file reference URLs (Finder drags those) are
/// resolved to paths.
fn dropped_paths(pasteboard: &NSPasteboard) -> Vec<PathBuf> {
    let classes = NSArray::from_slice(&[NSURL::class()]);
    // SAFETY: reading an immutable AppKit constant.
    let file_urls_only = unsafe { NSPasteboardURLReadingFileURLsOnlyKey };
    let yes = NSNumber::new_bool(true);
    let yes: &AnyObject = &yes;
    let options = NSDictionary::from_slices(&[file_urls_only], &[yes]);
    // SAFETY: NSURL can be read from a pasteboard (it conforms to
    // NSPasteboardReading), and the option maps its documented key to an
    // NSNumber holding a boolean, as the method expects.
    let objects = unsafe { pasteboard.readObjectsForClasses_options(&classes, Some(&options)) };
    let Some(objects) = objects else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|object| object.downcast::<NSURL>().ok())
        .filter_map(|url| file_path(&url))
        .collect()
}

/// The path of a file URL; `None` for any other URL. The path keeps the
/// name's characters as the URL has them: the file system's own spelling
/// (`fileSystemRepresentation`) would decompose accented letters, and the
/// name is what the other computer saves the file as.
fn file_path(url: &NSURL) -> Option<PathBuf> {
    if !url.isFileURL() {
        return None;
    }
    let url = if url.isFileReferenceURL() {
        url.filePathURL()?
    } else {
        url.retain()
    };
    let path = url.path()?.to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

// ----- The main thread's part -------------------------------------------------------------

/// The drop targets, on the main thread: one panel per zone, each made the
/// first time it is shown.
#[derive(Default)]
struct Panels {
    /// The version of the zones the panels were made for.
    version: u64,
    /// By zone index.
    panels: Vec<Option<Panel>>,
}

impl Panels {
    /// Shows the target on zone `index` of `zones`, unless they are no longer
    /// the zones in force (they changed after the tap asked).
    fn show(&mut self, state: &Arc<DropState>, zones: &Zones, index: usize, mtm: MainThreadMarker) {
        if zones.version != state.version.load(Ordering::Acquire) {
            return;
        }
        let Some(zone) = zones.zones.get(index) else {
            return;
        };
        if self.version != zones.version {
            self.clear();
            self.version = zones.version;
        }
        self.panels.resize_with(zones.zones.len(), || None);
        for (i, panel) in self.panels.iter().enumerate() {
            if let Some(panel) = panel
                && i != index
            {
                panel.window.orderOut(None);
            }
        }
        let Some(slot) = self.panels.get_mut(index) else {
            return;
        };
        let panel =
            slot.get_or_insert_with(|| Panel::new(zone, &zones.label, Arc::downgrade(state), mtm));
        panel.view.set_highlighted(false);
        panel.window.orderFrontRegardless();
        state.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Takes every target down.
    fn hide(&self) {
        for panel in self.panels.iter().flatten() {
            panel.window.orderOut(None);
        }
    }

    /// Takes every target down and lets the panels go.
    fn clear(&mut self) {
        self.hide();
        self.panels.clear();
    }
}

impl Drop for Panels {
    /// Only ever on the main thread (`MainOnly` sees to that): a panel is
    /// taken off the screen before it is released.
    fn drop(&mut self) {
        self.hide();
    }
}

/// One drop target.
struct Panel {
    window: Retained<NSPanel>,
    view: Retained<DropView>,
}

impl Panel {
    /// A drop target for `zone` with `label` on it, hidden until shown.
    fn new(zone: &DropZone, label: &str, state: Weak<DropState>, mtm: MainThreadMarker) -> Self {
        let frame = to_cocoa(target_rect(zone), main_display_frame().size.height);
        let frame = ns_rect(frame);
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: Rust owns the panel through `Retained`, so AppKit must not
        // also release it when it closes.
        unsafe { window.setReleasedWhenClosed(false) };
        // Hop is never the active app while the user drags from Finder: a
        // panel that hid on deactivating would never show.
        window.setHidesOnDeactivate(false);
        window.setFloatingPanel(true);
        window.setBecomesKeyOnlyIfNeeded(true);
        // Explicitly not ignored, so the transparent parts take the drop too.
        window.setIgnoresMouseEvents(false);
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        // Above the Dock and floating windows (after `setFloatingPanel`,
        // which sets a level of its own).
        window.setLevel(NSStatusWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setAnimationBehavior(NSWindowAnimationBehavior::None);
        let view = DropView::new(NSRect::new(NSPoint::new(0.0, 0.0), frame.size), state, mtm);
        // SAFETY: reading an immutable AppKit constant.
        let file_url = unsafe { NSPasteboardTypeFileURL };
        view.registerForDraggedTypes(&NSArray::from_slice(&[file_url]));
        view.add_label(label, mtm);
        window.setContentView(Some(&view));
        Panel { window, view }
    }
}

/// What the drop target's view keeps.
struct ViewIvars {
    /// Where drops go. Weak, because the state owns the panels and so this
    /// view.
    state: Weak<DropState>,
    /// Files are being dragged over the view.
    highlighted: Cell<bool>,
}

define_class!(
    // SAFETY: NSView can be subclassed to draw and to take drops, the chain
    // below is NSView's real superclass chain, and `DropView` does not
    // implement `Drop`.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HopDropTargetView"]
    #[ivars = ViewIvars]
    /// The drop target's view: a translucent rounded strip with the label,
    /// which takes dropped files as a copy.
    struct DropView;

    impl DropView {
        // SAFETY: `drawRect:` takes the rectangle to draw and returns
        // nothing, which is this signature.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            // AppKit calls this; a panic must not unwind into it.
            let _ = catch_unwind(AssertUnwindSafe(|| self.draw()));
        }
    }

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for DropView {}

    // SAFETY: the protocol asks for nothing beyond its methods, which are
    // all optional. Each method below catches panics, so none unwinds into
    // AppKit.
    unsafe impl NSDraggingDestination for DropView {
        // SAFETY: `draggingEntered:` takes the dragging info and returns a
        // drag operation, which is this signature.
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(
            &self,
            _sender: &ProtocolObject<dyn NSDraggingInfo>,
        ) -> NSDragOperation {
            catch_unwind(AssertUnwindSafe(|| {
                self.set_highlighted(true);
                NSDragOperation::Copy
            }))
            .unwrap_or(NSDragOperation::None)
        }

        // SAFETY: as `draggingEntered:`.
        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(
            &self,
            _sender: &ProtocolObject<dyn NSDraggingInfo>,
        ) -> NSDragOperation {
            catch_unwind(AssertUnwindSafe(|| NSDragOperation::Copy))
                .unwrap_or(NSDragOperation::None)
        }

        // SAFETY: `draggingExited:` takes the dragging info, possibly nil,
        // and returns nothing, which is this signature.
        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            let _ = catch_unwind(AssertUnwindSafe(|| self.set_highlighted(false)));
        }

        // SAFETY: `prepareForDragOperation:` takes the dragging info and
        // returns a BOOL, which is this signature.
        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag_operation(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            catch_unwind(AssertUnwindSafe(|| true)).unwrap_or(false)
        }

        // SAFETY: as `prepareForDragOperation:`.
        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            catch_unwind(AssertUnwindSafe(|| self.take_drop(sender))).unwrap_or(false)
        }

        // SAFETY: as `draggingExited:`.
        #[unsafe(method(concludeDragOperation:))]
        fn conclude_drag_operation(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            let _ = catch_unwind(AssertUnwindSafe(|| self.set_highlighted(false)));
        }
    }
);

impl DropView {
    /// A view of size `frame` sending drops to `state`.
    fn new(frame: NSRect, state: Weak<DropState>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            state,
            highlighted: Cell::new(false),
        });
        // SAFETY: NSView's designated initializer `initWithFrame:` takes the
        // frame and returns the initialized view.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Adds the label, centred in the view.
    fn add_label(&self, text: &str, mtm: MainThreadMarker) {
        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
        label.setSelectable(false);
        label.setAlignment(NSTextAlignment::Center);
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFont(Some(&NSFont::boldSystemFontOfSize(LABEL_SIZE)));
        let size = self.bounds().size;
        let width = (size.width - 2.0 * LABEL_INSET).max(0.0);
        label.setPreferredMaxLayoutWidth(width);
        let fits = label.sizeThatFits(NSSize::new(width, size.height));
        label.setFrame(ns_rect(label_frame(size.width, size.height, fits.height)));
        self.addSubview(&label);
    }

    /// Draws a faint tint over the whole target (so no part of it is fully
    /// transparent) and the rounded strip, stronger while files are over it.
    fn draw(&self) {
        let bounds = self.bounds();
        let accent = NSColor::controlAccentColor();
        accent.colorWithAlphaComponent(0.18).setFill();
        NSBezierPath::fillRect(bounds);
        let strip = NSRect::new(
            NSPoint::new(bounds.origin.x + STRIP_INSET, bounds.origin.y + STRIP_INSET),
            NSSize::new(
                (bounds.size.width - 2.0 * STRIP_INSET).max(0.0),
                (bounds.size.height - 2.0 * STRIP_INSET).max(0.0),
            ),
        );
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(strip, 12.0, 12.0);
        let alpha = if self.ivars().highlighted.get() {
            0.85
        } else {
            0.55
        };
        accent.colorWithAlphaComponent(alpha).setFill();
        path.fill();
        NSColor::whiteColor()
            .colorWithAlphaComponent(0.9)
            .setStroke();
        path.setLineWidth(2.0);
        path.stroke();
    }

    /// Highlights the strip, or not, redrawing it when that changes.
    fn set_highlighted(&self, highlighted: bool) {
        if self.ivars().highlighted.replace(highlighted) != highlighted {
            self.setNeedsDisplay(true);
        }
    }

    /// Takes a drop: queues its files, calls off the hide planned at the
    /// mouse-up and takes the target down instead, once AppKit is done with
    /// the drop. Returns whether there were files, so a drop of nothing
    /// usable (every file gone by now, say) slides back to where it came
    /// from instead of looking delivered.
    fn take_drop(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
        self.set_highlighted(false);
        let paths = dropped_paths(&sender.draggingPasteboard());
        let Some(state) = self.ivars().state.upgrade() else {
            return false;
        };
        let taken = state.add_drop(paths);
        state.generation.fetch_add(1, Ordering::AcqRel);
        state.on_main(|_, panels, _| panels.hide());
        taken
    }
}

// ----- Geometry ---------------------------------------------------------------------------

/// How far `p` is from the zone's stretch of edge, in points.
fn distance(zone: &DropZone, p: Point) -> f64 {
    let line = zone.display.edge_line(zone.edge);
    let (across, along) = if zone.edge.is_vertical() {
        (p.x - line, p.y)
    } else {
        (p.y - line, p.x)
    };
    let beyond = if along < zone.start {
        zone.start - along
    } else if along > zone.end {
        along - zone.end
    } else {
        0.0
    };
    across.hypot(beyond)
}

/// The zone nearest to `p`, by index, and how far it is.
fn nearest(zones: &[DropZone], p: Point) -> Option<(usize, f64)> {
    zones
        .iter()
        .map(|zone| distance(zone, p))
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// The target's frame for `zone`, in Quartz coordinates: the zone's stretch
/// of the edge, [`DEPTH`] points into the display (all of a display less
/// deep than that). It covers the display's last column or row of points,
/// where macOS stops a cursor pushed against the edge.
fn target_rect(zone: &DropZone) -> Rect {
    let display = &zone.display;
    let length = zone.end - zone.start;
    match zone.edge {
        Edge::Left => {
            let depth = DEPTH.min(display.size.width);
            Rect::new(display.min_x(), zone.start, depth, length)
        }
        Edge::Right => {
            let depth = DEPTH.min(display.size.width);
            Rect::new(display.max_x() - depth, zone.start, depth, length)
        }
        Edge::Top => {
            let depth = DEPTH.min(display.size.height);
            Rect::new(zone.start, display.min_y(), length, depth)
        }
        Edge::Bottom => {
            let depth = DEPTH.min(display.size.height);
            Rect::new(zone.start, display.max_y() - depth, length, depth)
        }
    }
}

/// `rect`, given in Quartz coordinates (origin at the top-left corner of
/// the primary display, y down), in Cocoa's screen coordinates: origin at
/// the bottom-left corner of the primary display, `primary_height` points
/// tall, y up.
fn to_cocoa(rect: Rect, primary_height: f64) -> Rect {
    Rect::new(
        rect.origin.x,
        primary_height - rect.max_y(),
        rect.size.width,
        rect.size.height,
    )
}

/// The label's frame in a target `width` by `height` points (Cocoa view
/// coordinates), for text `text_height` points tall: centred, clear of the
/// target's edges.
fn label_frame(width: f64, height: f64, text_height: f64) -> Rect {
    let label_width = (width - 2.0 * LABEL_INSET).max(0.0);
    let label_height = text_height.min(height - 2.0 * LABEL_INSET).max(0.0);
    Rect::new(
        LABEL_INSET,
        (height - label_height) / 2.0,
        label_width,
        label_height,
    )
}

fn ns_rect(rect: Rect) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.origin.x, rect.origin.y),
        NSSize::new(rect.size.width, rect.size.height),
    )
}

#[cfg(test)]
mod tests {
    //! Nothing here opens a window or touches the real drag pasteboard. The
    //! panels are only ever made on the main thread, which runs no event
    //! loop in tests, so work queued for it never runs; the pasteboard
    //! tests use private pasteboards.

    use std::fs;
    use std::thread;

    use objc2::sel;
    use objc2_foundation::ns_string;

    use super::*;
    use crate::private_pasteboard::PrivatePasteboard;

    /// The MacBook's built-in display, as Quartz lists it.
    const MAC: Rect = Rect::new(0.0, 0.0, 1512.0, 982.0);

    fn zone(display: Rect, edge: Edge, start: f64, end: f64) -> DropZone {
        DropZone {
            display,
            edge,
            start,
            end,
        }
    }

    /// The Mac's right edge, which leads to the PC, less the corners.
    fn right() -> DropZone {
        zone(MAC, Edge::Right, 59.0, 923.0)
    }

    #[test]
    fn the_target_lies_along_the_zone_inside_the_display() {
        assert_eq!(target_rect(&right()), Rect::new(1392.0, 59.0, 120.0, 864.0));
        // The last column of points, where a cursor pushed against the edge
        // stops, is inside it.
        assert!(target_rect(&right()).contains(Point::new(1511.0, 500.0)));
        assert_eq!(
            target_rect(&zone(MAC, Edge::Left, 59.0, 923.0)),
            Rect::new(0.0, 59.0, 120.0, 864.0)
        );
        assert_eq!(
            target_rect(&zone(MAC, Edge::Top, 59.0, 1453.0)),
            Rect::new(59.0, 0.0, 1394.0, 120.0)
        );
        let bottom = target_rect(&zone(MAC, Edge::Bottom, 59.0, 1453.0));
        assert_eq!(bottom, Rect::new(59.0, 862.0, 1394.0, 120.0));
        assert!(bottom.contains(Point::new(700.0, 981.0)));
        // A display to the right of the Mac's, lower down.
        let monitor = Rect::new(1512.0, 200.0, 1920.0, 1080.0);
        assert_eq!(
            target_rect(&zone(monitor, Edge::Right, 260.0, 1220.0)),
            Rect::new(3312.0, 260.0, 120.0, 960.0)
        );
        // A display less deep than the target gets covered whole.
        let strip = Rect::new(0.0, 0.0, 80.0, 50.0);
        assert_eq!(
            target_rect(&zone(strip, Edge::Right, 10.0, 40.0)),
            Rect::new(0.0, 10.0, 80.0, 30.0)
        );
        assert_eq!(
            target_rect(&zone(strip, Edge::Bottom, 10.0, 70.0)),
            Rect::new(10.0, 0.0, 60.0, 50.0)
        );
    }

    #[test]
    fn quartz_frames_flip_into_cocoa_coordinates() {
        let height = MAC.size.height;
        // Along the right edge, 59 points from the top and from the bottom.
        assert_eq!(
            to_cocoa(target_rect(&right()), height),
            Rect::new(1392.0, 59.0, 120.0, 864.0)
        );
        // At the top of the display: Cocoa's y counts up from the bottom.
        assert_eq!(
            to_cocoa(target_rect(&zone(MAC, Edge::Top, 59.0, 1453.0)), height),
            Rect::new(59.0, 862.0, 1394.0, 120.0)
        );
        assert_eq!(
            to_cocoa(target_rect(&zone(MAC, Edge::Bottom, 59.0, 1453.0)), height),
            Rect::new(59.0, 0.0, 1394.0, 120.0)
        );
        // A display above the Mac's has negative Quartz y, and Cocoa y above
        // the Mac's height.
        let above = Rect::new(0.0, -1080.0, 1920.0, 1080.0);
        assert_eq!(
            to_cocoa(target_rect(&zone(above, Edge::Top, 60.0, 1860.0)), height),
            Rect::new(60.0, 1942.0, 1800.0, 120.0)
        );
        // A display to the right and lower down reaches below Cocoa's zero.
        let monitor = Rect::new(1512.0, 200.0, 1920.0, 1080.0);
        assert_eq!(
            to_cocoa(
                target_rect(&zone(monitor, Edge::Right, 260.0, 1220.0)),
                height
            ),
            Rect::new(3312.0, -238.0, 120.0, 960.0)
        );
        // And back again: flipping twice changes nothing.
        let rect = Rect::new(5.0, -20.0, 30.0, 40.0);
        assert_eq!(to_cocoa(to_cocoa(rect, height), height), rect);
    }

    #[test]
    fn distance_is_measured_to_the_zone_s_stretch_of_edge() {
        let right = right();
        assert_eq!(distance(&right, Point::new(1511.0, 500.0)), 1.0);
        assert_eq!(distance(&right, Point::new(1362.0, 500.0)), 150.0);
        // Past either end of the stretch, the end counts.
        assert_eq!(
            distance(&right, Point::new(1511.0, 30.0)),
            1.0_f64.hypot(29.0)
        );
        assert_eq!(
            distance(&right, Point::new(1511.0, 950.0)),
            1.0_f64.hypot(27.0)
        );
        let left = zone(MAC, Edge::Left, 59.0, 923.0);
        assert_eq!(distance(&left, Point::new(0.0, 500.0)), 0.0);
        assert_eq!(distance(&left, Point::new(150.0, 500.0)), 150.0);
        let top = zone(MAC, Edge::Top, 100.0, 1400.0);
        assert_eq!(distance(&top, Point::new(700.0, 0.0)), 0.0);
        assert_eq!(distance(&top, Point::new(50.0, 10.0)), 10.0_f64.hypot(50.0));
        let bottom = zone(MAC, Edge::Bottom, 100.0, 1400.0);
        assert_eq!(distance(&bottom, Point::new(700.0, 981.0)), 1.0);
    }

    #[test]
    fn the_nearest_zone_wins() {
        assert_eq!(nearest(&[], Point::new(0.0, 0.0)), None);
        let left_monitor = Rect::new(-1920.0, 0.0, 1920.0, 1080.0);
        let zones = [right(), zone(left_monitor, Edge::Left, 60.0, 1020.0)];
        assert_eq!(nearest(&zones, Point::new(1500.0, 500.0)), Some((0, 12.0)));
        assert_eq!(nearest(&zones, Point::new(-1900.0, 500.0)), Some((1, 20.0)));
    }

    #[test]
    fn unusable_zones_are_left_out() {
        assert!(usable(&right()));
        assert!(!usable(&zone(MAC, Edge::Right, 500.0, 500.0)), "empty");
        assert!(!usable(&zone(MAC, Edge::Right, 900.0, 100.0)), "backward");
        assert!(!usable(&zone(MAC, Edge::Right, f64::NAN, 100.0)));
        assert!(!usable(&zone(MAC, Edge::Right, 0.0, f64::INFINITY)));
        let flat = Rect::new(0.0, 0.0, 1512.0, 0.0);
        assert!(!usable(&zone(flat, Edge::Right, 0.0, 10.0)), "no area");
    }

    #[test]
    fn only_left_button_events_count() {
        assert_eq!(left_button(CGEventType::LeftMouseDown), Some(Left::Down));
        assert_eq!(
            left_button(CGEventType::LeftMouseDragged),
            Some(Left::Dragged)
        );
        assert_eq!(left_button(CGEventType::LeftMouseUp), Some(Left::Up));
        for ty in [
            CGEventType::MouseMoved,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
        ] {
            assert_eq!(left_button(ty), None, "{ty:?}");
        }
    }

    // ----- The tap's decisions -----

    /// A tap watching `zones`.
    fn watching(zones: Vec<DropZone>) -> TapDrops {
        let mut tap = TapDrops::numbering_from(0);
        tap.zones = Arc::new(Zones {
            version: 1,
            zones,
            label: "Drop to send to Windows PC".into(),
        });
        tap
    }

    fn at(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn show(zone: usize) -> Requests {
        Requests {
            show: Some(zone),
            ..Requests::default()
        }
    }

    const HIDE: Requests = Requests {
        snapshot: None,
        check: None,
        show: None,
        hide: true,
        hide_later: false,
    };

    const HIDE_LATER: Requests = Requests {
        snapshot: None,
        check: None,
        show: None,
        hide: false,
        hide_later: true,
    };

    const NOTHING: Requests = Requests {
        snapshot: None,
        check: None,
        show: None,
        hide: false,
        hide_later: false,
    };

    #[test]
    fn without_zones_the_tap_asks_for_nothing() {
        let mut tap = TapDrops::numbering_from(0);
        let t = Instant::now();
        assert_eq!(tap.step(Left::Down, at(1500.0, 500.0), t, 0), NOTHING);
        // Even a drag the helper vouches for.
        assert_eq!(tap.step(Left::Dragged, at(1511.0, 500.0), t, 1), NOTHING);
        assert_eq!(tap.step(Left::Up, at(1511.0, 500.0), t, 1), NOTHING);
        assert_eq!(tap.press, 1, "presses still count");
    }

    #[test]
    fn a_press_notes_the_pasteboard_and_a_drag_asks_about_it_every_50_ms() {
        let mut tap = watching(vec![right()]);
        let t = Instant::now();
        let snapshot = |press| Requests {
            snapshot: Some(press),
            ..Requests::default()
        };
        let check = |press| Requests {
            check: Some(press),
            ..Requests::default()
        };
        assert_eq!(tap.step(Left::Down, at(800.0, 500.0), t, 0), snapshot(1));
        assert_eq!(tap.step(Left::Dragged, at(801.0, 500.0), t, 0), check(1));
        let drag =
            |tap: &mut TapDrops, after| tap.step(Left::Dragged, at(805.0, 500.0), t + after, 0);
        assert_eq!(drag(&mut tap, ms(10)), NOTHING);
        assert_eq!(drag(&mut tap, ms(49)), NOTHING);
        assert_eq!(drag(&mut tap, ms(50)), check(1));
        assert_eq!(drag(&mut tap, ms(60)), NOTHING);
        assert_eq!(drag(&mut tap, ms(100)), check(1));
        assert_eq!(
            tap.step(Left::Up, at(805.0, 500.0), t + ms(110), 0),
            NOTHING
        );
        // A new press is asked about at once.
        assert_eq!(
            tap.step(Left::Down, at(805.0, 500.0), t + ms(120), 0),
            snapshot(2)
        );
        assert_eq!(drag(&mut tap, ms(121)), check(2));
    }

    #[test]
    fn a_drag_of_files_shows_the_target_near_a_zone_and_hides_it_far_away() {
        let mut tap = watching(vec![right()]);
        let t = Instant::now();
        tap.step(Left::Down, at(800.0, 500.0), t, 0);
        let mut drag = |x: f64| tap.step(Left::Dragged, at(x, 500.0), t, 1);
        // Confirmed, so no more checks; still 712 points away.
        assert_eq!(drag(800.0), NOTHING);
        // Within 150 points: once.
        assert_eq!(drag(1400.0), show(0));
        assert_eq!(drag(1450.0), NOTHING);
        assert_eq!(drag(1511.0), NOTHING);
        // Between 150 and 300 points it stays up.
        assert_eq!(drag(1300.0), NOTHING);
        // Beyond 300 points it goes.
        assert_eq!(drag(1100.0), HIDE);
        assert_eq!(drag(1000.0), NOTHING);
        // And between 150 and 300 it stays down.
        assert_eq!(drag(1300.0), NOTHING);
        assert_eq!(drag(1362.0), show(0));
        // Letting go plans the hide, for after the drop.
        assert_eq!(tap.step(Left::Up, at(1511.0, 500.0), t, 1), HIDE_LATER);
        assert_eq!(tap.step(Left::Up, at(1511.0, 500.0), t, 1), NOTHING);
    }

    #[test]
    fn letting_go_without_a_target_plans_no_hide() {
        let mut tap = watching(vec![right()]);
        let t = Instant::now();
        tap.step(Left::Down, at(800.0, 500.0), t, 0);
        assert_eq!(tap.step(Left::Dragged, at(900.0, 500.0), t, 1), NOTHING);
        assert_eq!(tap.step(Left::Up, at(900.0, 500.0), t, 1), NOTHING);
    }

    #[test]
    fn only_a_file_drag_of_this_press_shows_the_target() {
        let mut tap = watching(vec![right()]);
        let t = Instant::now();
        // Press 1 dragged files.
        tap.step(Left::Down, at(800.0, 500.0), t, 0);
        assert_eq!(tap.step(Left::Dragged, at(1500.0, 500.0), t, 1), show(0));
        tap.step(Left::Up, at(1500.0, 500.0), t, 1);
        // Press 2 drags a window to the same edge: the helper's answer is
        // still about press 1, so the tap only asks.
        tap.step(Left::Down, at(800.0, 500.0), t, 1);
        let asked = tap.step(Left::Dragged, at(1500.0, 500.0), t, 1);
        assert_eq!(
            asked,
            Requests {
                check: Some(2),
                ..Requests::default()
            }
        );
        assert_eq!(tap.step(Left::Up, at(1500.0, 500.0), t, 1), NOTHING);
    }

    #[test]
    fn a_drag_whose_press_the_tap_missed_is_left_alone() {
        let mut tap = watching(vec![right()]);
        let t = Instant::now();
        // No press seen (it happened before the tap, or was swallowed).
        assert_eq!(tap.step(Left::Dragged, at(1500.0, 500.0), t, 0), NOTHING);
        assert_eq!(tap.step(Left::Up, at(1500.0, 500.0), t, 0), NOTHING);
        // After a release, a stray drag event changes nothing either.
        tap.step(Left::Down, at(800.0, 500.0), t, 0);
        tap.step(Left::Up, at(800.0, 500.0), t, 1);
        assert_eq!(tap.step(Left::Dragged, at(1500.0, 500.0), t, 1), NOTHING);
    }

    #[test]
    fn the_target_follows_the_drag_to_the_nearer_zone() {
        let left_monitor = Rect::new(-1920.0, 0.0, 1920.0, 1080.0);
        let mut tap = watching(vec![right(), zone(left_monitor, Edge::Left, 60.0, 1020.0)]);
        let t = Instant::now();
        tap.step(Left::Down, at(800.0, 500.0), t, 0);
        assert_eq!(tap.step(Left::Dragged, at(1500.0, 500.0), t, 1), show(0));
        // All the way across, to the other zone.
        assert_eq!(tap.step(Left::Dragged, at(-1900.0, 500.0), t, 1), show(1));
        assert_eq!(tap.step(Left::Up, at(-1900.0, 500.0), t, 1), HIDE_LATER);
    }

    #[test]
    fn each_tap_numbers_its_presses_apart_from_the_taps_before() {
        let state = Arc::new(DropState::with_pasteboard(|| None));
        state.set_zones(vec![right()], "Drop to send to Windows PC");
        let t = Instant::now();
        // The first tap's first press is 1, as the helper's "none" is 0.
        let mut first = TapDrops::new(&state);
        first.zones = Arc::clone(&lock(&state.zones));
        let press = first.step(Left::Down, at(800.0, 500.0), t, 0).snapshot;
        assert_eq!(press, Some(1));
        // A later tap (capture started again) never reuses a number: the
        // helper's answer about press 1 must not count for the new tap.
        let mut second = TapDrops::new(&state);
        second.zones = Arc::clone(&lock(&state.zones));
        let press = second.step(Left::Down, at(800.0, 500.0), t, 1).snapshot;
        assert_eq!(press, Some((1 << 32) + 1));
        let dragged = second.step(Left::Dragged, at(1500.0, 500.0), t, 1);
        assert_eq!(dragged.show, None);
        assert_eq!(dragged.check, press);
    }

    #[test]
    fn the_tap_takes_new_zones_without_waiting_for_them() {
        let state = Arc::new(DropState::with_pasteboard(|| None));
        let mut tap = TapDrops::numbering_from(0);
        tap.refresh(&state);
        assert!(tap.zones.zones.is_empty());

        state.set_zones(vec![right()], "Drop to send to Windows PC");
        tap.refresh(&state);
        assert_eq!(tap.zones.version, 1);
        assert_eq!(tap.zones.zones, [right()]);
        let t = Instant::now();
        tap.step(Left::Down, at(800.0, 500.0), t, 0);
        assert_eq!(tap.step(Left::Dragged, at(1500.0, 500.0), t, 1), show(0));

        // Another thread is setting zones right now: the tap keeps its copy
        // and its target rather than wait.
        let mut setting = lock(&state.zones);
        *setting = Arc::new(Zones {
            version: 2,
            zones: Vec::new(),
            label: String::new(),
        });
        state.version.store(2, Ordering::Release);
        tap.refresh(&state);
        assert_eq!(tap.zones.version, 1);
        assert_eq!(tap.target, Some(0));
        drop(setting);
        // The next event takes the new zones and forgets the target, which
        // `set_zones` takes down itself.
        tap.refresh(&state);
        assert_eq!(tap.zones.version, 2);
        assert_eq!(tap.target, None);
        assert_eq!(tap.step(Left::Up, at(1500.0, 500.0), t, 1), NOTHING);
    }

    // ----- The helper's part -----

    #[test]
    fn only_a_drag_started_during_the_press_counts() {
        let mut probe = DragProbe::default();
        let never = || -> bool { panic!("asked about files without a new drag") };
        assert!(!probe.carries_files(1, 10, never), "no press seen");
        probe.begin(1, 10);
        // Whatever an earlier drag left there does not count.
        assert!(!probe.carries_files(1, 10, never));
        // A drag started: it counts if it holds files.
        assert!(probe.carries_files(1, 11, || true));
        assert!(!probe.carries_files(1, 11, || false));
        // An answer is only ever about the press the probe saw begin.
        assert!(!probe.carries_files(2, 11, never));
        probe.begin(2, 11);
        assert!(!probe.carries_files(2, 11, never));
        assert!(probe.carries_files(2, 12, || true));
    }

    /// A file URL for `path`, which need not exist.
    fn file_url(path: &str) -> Retained<NSURL> {
        NSURL::from_file_path(path).expect("file URL")
    }

    fn web_url() -> Retained<NSURL> {
        NSURL::URLWithString(ns_string!("https://example.com/page")).expect("web URL")
    }

    #[test]
    fn file_urls_are_told_from_other_drags() {
        let private = PrivatePasteboard::new();
        assert!(!has_file_urls(&private.pasteboard), "empty");
        private.write(vec![file_url("/tmp/hop-report.txt")]);
        assert!(has_file_urls(&private.pasteboard));
        private.write(vec![web_url()]);
        assert!(!has_file_urls(&private.pasteboard), "a link");
        private.write(vec![NSString::from_str("dragged text")]);
        assert!(!has_file_urls(&private.pasteboard), "text");
    }

    #[test]
    fn a_drop_reads_file_urls_only_in_order() {
        let private = PrivatePasteboard::new();
        assert!(dropped_paths(&private.pasteboard).is_empty(), "empty");
        private.write(vec![
            file_url("/tmp/hop a.txt"),
            web_url(),
            file_url("/tmp/Hop Ünïcode/b.pdf"),
        ]);
        assert_eq!(
            dropped_paths(&private.pasteboard),
            [
                PathBuf::from("/tmp/hop a.txt"),
                PathBuf::from("/tmp/Hop Ünïcode/b.pdf")
            ]
        );
        private.write(vec![web_url()]);
        assert!(dropped_paths(&private.pasteboard).is_empty(), "links only");
    }

    /// A folder of this test's own under the system's temporary folder,
    /// removed when dropped.
    struct TempFolder(PathBuf);

    impl TempFolder {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("hop-catcher-{name}-{}", std::process::id()));
            fs::create_dir_all(&path).expect("temporary folder");
            TempFolder(path)
        }
    }

    impl Drop for TempFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_reference_url_is_read_as_its_path() {
        // Finder drags file reference URLs (file:///.file/id=...), which
        // name a file by its number on the disk rather than by its path.
        let folder = TempFolder::new("reference");
        let path = folder.0.join("dropped.txt");
        fs::write(&path, b"x").expect("file");
        let url = NSURL::from_file_path(&path).expect("file URL");
        let reference = url.fileReferenceURL().expect("reference URL");
        assert!(reference.isFileReferenceURL());
        assert_eq!(
            file_path(&reference).map(|p| p.canonicalize().expect("path")),
            Some(path.canonicalize().expect("path"))
        );
        let private = PrivatePasteboard::new();
        private.write(vec![reference]);
        let dropped = dropped_paths(&private.pasteboard);
        assert_eq!(dropped.len(), 1, "{}", dropped.len());
        assert_eq!(
            dropped[0].canonicalize().expect("path"),
            path.canonicalize().expect("path")
        );
        assert_eq!(file_path(&web_url()), None);
    }

    /// Waits until `done` holds, for at most a few seconds.
    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(ms(1));
        }
    }

    #[test]
    fn the_helper_finds_a_new_drag_of_files_and_not_an_old_one() {
        let private = PrivatePasteboard::new();
        let state = Arc::new(DropState::with_pasteboard(private.opener()));
        // An earlier drag left files on the drag pasteboard.
        private.write(vec![file_url("/tmp/old.txt")]);
        let ask = |snapshot, check| {
            state.ask_helper(Requests {
                snapshot,
                check,
                ..Requests::default()
            });
            // The helper queue is serial: once the check is done, so is
            // everything asked before it.
            wait_for("the helper", || !state.checking.load(Ordering::Acquire));
        };
        ask(Some(1), Some(1));
        assert!(
            lock(&state.probe)
                .start
                .is_some_and(|(press, _)| press == 1)
        );
        assert_eq!(state.file_drag.load(Ordering::Acquire), 0, "an old drag");
        // A drag of files starts.
        private.write(vec![file_url("/tmp/new.txt")]);
        ask(None, Some(1));
        assert_eq!(state.file_drag.load(Ordering::Acquire), 1);
        // The next press drags text: its answer is never yes.
        ask(Some(2), None);
        private.write(vec![NSString::from_str("dragged text")]);
        ask(None, Some(2));
        assert_eq!(
            state.file_drag.load(Ordering::Acquire),
            1,
            "still press 1's"
        );
    }

    #[test]
    fn a_helper_without_a_drag_pasteboard_finds_nothing() {
        let state = Arc::new(DropState::with_pasteboard(|| None));
        state.ask_helper(Requests {
            snapshot: Some(1),
            check: Some(1),
            ..Requests::default()
        });
        wait_for("the helper", || !state.checking.load(Ordering::Acquire));
        assert!(lock(&state.probe).start.is_none());
        assert_eq!(state.file_drag.load(Ordering::Acquire), 0);
    }

    // ----- The catcher -----

    #[test]
    fn drops_are_collected_oldest_first_and_once() {
        let state = Arc::new(DropState::with_pasteboard(|| None));
        let mut catcher = MacDrops::new(Arc::clone(&state));
        assert!(catcher.take_drops().is_empty());
        let first = vec![PathBuf::from("/Users/me/a.txt")];
        let second = vec![
            PathBuf::from("/Users/me/b.png"),
            PathBuf::from("/Users/me/c.pdf"),
        ];
        assert!(state.add_drop(first.clone()));
        assert!(!state.add_drop(Vec::new()), "a drop without files");
        assert!(state.add_drop(second.clone()));
        assert_eq!(catcher.take_drops(), [first, second]);
        assert!(catcher.take_drops().is_empty());
    }

    #[test]
    fn zones_take_effect_once_per_change() {
        let state = Arc::new(DropState::with_pasteboard(|| None));
        let mut catcher = MacDrops::new(Arc::clone(&state));
        let label = "Drop to send to Windows PC";
        let backward = zone(MAC, Edge::Right, 900.0, 100.0);
        assert!(catcher.set_zones(vec![right(), backward], label).is_ok());
        let zones = Arc::clone(&lock(&state.zones));
        assert_eq!((zones.version, zones.label.as_str()), (1, label));
        assert_eq!(zones.zones, [right()], "the unusable zone is left out");
        assert_eq!(state.version.load(Ordering::Acquire), 1);
        // The same again changes nothing.
        assert!(catcher.set_zones(vec![right()], label).is_ok());
        assert_eq!(state.version.load(Ordering::Acquire), 1);
        // A new label is a change.
        assert!(
            catcher
                .set_zones(vec![right()], "Drop to send to PC")
                .is_ok()
        );
        assert_eq!(state.version.load(Ordering::Acquire), 2);
        // Clearing them stops the catching.
        assert!(catcher.set_zones(Vec::new(), "").is_ok());
        assert_eq!(state.version.load(Ordering::Acquire), 3);
        assert!(lock(&state.zones).zones.is_empty());
        // So does dropping the catcher.
        assert!(catcher.set_zones(vec![right()], label).is_ok());
        drop(catcher);
        assert!(lock(&state.zones).zones.is_empty());
        assert_eq!(state.version.load(Ordering::Acquire), 5);
    }

    #[test]
    fn the_label_is_centred_clear_of_the_edges() {
        assert_eq!(
            label_frame(120.0, 864.0, 48.0),
            Rect::new(16.0, 408.0, 88.0, 48.0)
        );
        // Text too tall for the target is cut to fit, from the middle out.
        assert_eq!(
            label_frame(120.0, 100.0, 200.0),
            Rect::new(16.0, 16.0, 88.0, 68.0)
        );
        // A target too small for a label gets an empty one.
        let tiny = label_frame(20.0, 20.0, 10.0);
        assert_eq!((tiny.size.width, tiny.size.height), (0.0, 0.0));
    }

    #[test]
    fn the_drop_view_class_answers_every_drag_message() {
        // Registering the class makes no view and no window. With debug
        // assertions on, objc2 also checks each method against the
        // signature AppKit declares.
        let class = DropView::class();
        assert_eq!(class.name().to_str(), Ok("HopDropTargetView"));
        for selector in [
            sel!(drawRect:),
            sel!(draggingEntered:),
            sel!(draggingUpdated:),
            sel!(draggingExited:),
            sel!(prepareForDragOperation:),
            sel!(performDragOperation:),
            sel!(concludeDragOperation:),
        ] {
            assert!(class.instance_method(selector).is_some(), "{selector:?}");
        }
    }
}
