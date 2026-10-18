//! AppKit: the app's event loop and the self-test window.
//!
//! AppKit objects may only be used on the main thread, while the self-test
//! runs on a background thread (see [`run_with_ui`]). Every AppKit call in
//! the test window is therefore sent to the main thread through the main
//! dispatch queue, and waited for with a timeout, so a main thread that is
//! not running its event loop produces an error instead of a hang.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use hop_platform::{PlatformError, Result, TestWindow};
use hop_types::Point;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSControl, NSEvent,
    NSEventModifierFlags, NSEventType, NSFloatingWindowLevel, NSMenu, NSMenuItem, NSResponder,
    NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string};

use crate::displays::main_display_frame;

/// How long a test-window call waits for the main thread.
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `focus_text` waits for macOS to hand the test window the
/// keyboard. Activating an app is asynchronous; it normally takes a small
/// fraction of this.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(2);

/// How often `focus_text` looks in the meantime.
const FOCUS_POLL: Duration = Duration::from_millis(50);

/// The window's content size, in points.
///
/// The instructions get everything above the text field. The self-test's
/// longest step text, with its heading, the countdown and the line saying
/// how to stop, runs to about a dozen lines; started through LaunchServices,
/// Hop's own output goes to a log file, so this window is the only place the
/// user can read it while the step runs. A label clips what does not fit.
const WINDOW_WIDTH: f64 = 520.0;
const WINDOW_HEIGHT: f64 = 400.0;

/// The text field's frame (Cocoa view coordinates: y grows upward).
const FIELD_FRAME: (f64, f64, f64, f64) = (20.0, 70.0, WINDOW_WIDTH - 40.0, 24.0);

/// The instruction label's frame: from a little above the text field to a
/// little below the window's top edge.
const LABEL_FRAME: (f64, f64, f64, f64) = (
    20.0,
    110.0,
    WINDOW_WIDTH - 40.0,
    WINDOW_HEIGHT - 110.0 - 20.0,
);

/// Runs AppKit's event loop on the main thread while `f` runs on a
/// background thread, and returns `f`'s result once it finishes.
///
/// **Call this from the main thread** (the thread `main` started on), and
/// do all test-window work inside `f`. It sets the app up as an accessory
/// (no Dock icon, no menu bar), runs the event loop until `f` returns, then
/// stops the loop. Called on any other thread it cannot run AppKit, so it
/// just runs `f` there; test-window calls inside `f` then fail with a
/// timeout error.
///
/// If `f` panics, the panic is passed on to the caller once the loop has
/// stopped.
pub fn run_with_ui<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let Some(mtm) = MainThreadMarker::new() else {
        return f();
    };
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_edit_menu(mtm, &app);
    let done = Arc::new(AtomicBool::new(false));
    let worker = thread::spawn({
        let stop = StopLoopOnDrop(Arc::clone(&done));
        move || {
            // Dropped when `f` returns or panics, so the loop always stops.
            let _stop = stop;
            f()
        }
    });
    // `run` returns only when stopped; if anything other than the worker
    // stops it early, run it again until the worker is done.
    while !done.load(Ordering::SeqCst) {
        app.run();
    }
    match worker.join() {
        Ok(result) => result,
        Err(panic) => resume_unwind(panic),
    }
}

/// Gives the app a main menu holding the standard Edit commands.
///
/// Hop runs as an accessory app, so this menu is never shown. AppKit still
/// looks up key equivalents in it, though, and without it a text field
/// ignores Cmd+A, Cmd+C, Cmd+V and the rest: in every Mac app those
/// shortcuts belong to the Edit menu, not to the text field. The first
/// self-test on a real Mac failed its select-all check for this reason.
fn install_edit_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    // An upper-case key equivalent includes Shift: Redo is Shift+Cmd+Z.
    let commands = [
        ("Undo", sel!(undo:), "z"),
        ("Redo", sel!(redo:), "Z"),
        ("Cut", sel!(cut:), "x"),
        ("Copy", sel!(copy:), "c"),
        ("Paste", sel!(paste:), "v"),
        ("Select All", sel!(selectAll:), "a"),
    ];
    for (title, action, key) in commands {
        // SAFETY: each selector is a standard responder action. AppKit sends
        // it along the responder chain to the first object that implements
        // it, and disables the item where none does, so it is never sent to
        // an object that cannot handle it.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        edit.addItem(&item);
    }
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setSubmenu(Some(&edit));
    let main = NSMenu::new(mtm);
    // AppKit treats a main menu's first item as the application menu.
    main.addItem(&NSMenuItem::new(mtm));
    main.addItem(&edit_item);
    app.setMainMenu(Some(&main));
}

/// Marks the work done and stops the main event loop when dropped.
struct StopLoopOnDrop(Arc<AtomicBool>);

impl Drop for StopLoopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        DispatchQueue::main().exec_async(|| {
            // SAFETY: work submitted to the main queue runs on the main
            // thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            let _ = catch_unwind(AssertUnwindSafe(|| stop_event_loop(mtm)));
        });
    }
}

/// Stops `NSApplication::run`. The stop takes effect only after the loop
/// handles an event, so a harmless application-defined event follows it.
fn stop_event_loop(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);
    let wake_up =
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags(0),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
    if let Some(event) = wake_up {
        app.postEvent_atStart(&event, true);
    }
}

/// Runs `f` on the main thread and returns its result: directly when
/// already there, otherwise through the main dispatch queue, waiting at
/// most [`MAIN_THREAD_TIMEOUT`].
fn on_main<R: Send + 'static>(f: impl FnOnce(MainThreadMarker) -> R + Send + 'static) -> Result<R> {
    let panicked = || PlatformError::Os("the test window's AppKit code panicked".into());
    if let Some(mtm) = MainThreadMarker::new() {
        return catch_unwind(AssertUnwindSafe(|| f(mtm))).map_err(|_| panicked());
    }
    let (reply, result) = mpsc::sync_channel(1);
    DispatchQueue::main().exec_async(move || {
        // SAFETY: work submitted to the main queue runs on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let outcome = catch_unwind(AssertUnwindSafe(|| f(mtm)));
        // The caller may have timed out and gone; that is fine.
        let _ = reply.send(outcome.ok());
    });
    match result.recv_timeout(MAIN_THREAD_TIMEOUT) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(panicked()),
        Err(_) => Err(PlatformError::Os(
            "the main thread did not respond; run the test window inside run_with_ui".into(),
        )),
    }
}

/// A value that may only be touched on the main thread, but can be held,
/// shared and dropped on any thread.
struct MainOnly<T: 'static>(Option<T>);

// SAFETY: the value is reachable only through `get`, which takes a
// `MainThreadMarker`, so it is only ever used on the main thread; `Drop`
// moves it to the main thread before dropping it.
unsafe impl<T: 'static> Send for MainOnly<T> {}
// SAFETY: as above; shared references give no access off the main thread.
unsafe impl<T: 'static> Sync for MainOnly<T> {}

impl<T: 'static> MainOnly<T> {
    fn new(value: T, _mtm: MainThreadMarker) -> Self {
        Self(Some(value))
    }

    fn get(&self, _mtm: MainThreadMarker) -> Option<&T> {
        self.0.as_ref()
    }
}

impl<T: 'static> Drop for MainOnly<T> {
    fn drop(&mut self) {
        let Some(value) = self.0.take() else {
            return;
        };
        if MainThreadMarker::new().is_some() {
            drop(value);
            return;
        }
        // Dropped elsewhere: release the AppKit objects on the main thread,
        // without waiting. If the main loop never runs again they leak,
        // which beats a deadlock.
        let value = SendToMain(value);
        DispatchQueue::main().exec_async(move || {
            let _ = catch_unwind(AssertUnwindSafe(move || drop(value)));
        });
    }
}

/// Carries a main-thread value to the main queue.
struct SendToMain<T>(T);

// SAFETY: only `MainOnly::drop` creates this, to move its value onto the
// main queue, where the value is dropped without being used.
unsafe impl<T> Send for SendToMain<T> {}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `ClickTarget`
    // does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HopSelfTestClickTarget"]
    #[ivars = Arc<AtomicU32>]
    /// The test window button's target: counts clicks.
    struct ClickTarget;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for ClickTarget {}

    impl ClickTarget {
        // SAFETY: an action method takes the sender and returns nothing,
        // which is this signature.
        #[unsafe(method(buttonClicked:))]
        fn button_clicked(&self, _sender: Option<&AnyObject>) {
            // AppKit calls this; a panic must not unwind into it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.ivars().fetch_add(1, Ordering::SeqCst);
            }));
        }
    }
);

define_class!(
    // SAFETY: NSButton can be subclassed to override `acceptsFirstMouse:`,
    // the chain below is NSButton's real superclass chain, and
    // `FirstClickButton` does not implement `Drop`.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HopSelfTestButton"]
    /// A button that takes a click even while its window is not the key
    /// window. A plain button's first click on an inactive window only
    /// brings the window forward, which would lose an injected click.
    struct FirstClickButton;

    impl FirstClickButton {
        // SAFETY: `acceptsFirstMouse:` takes the event (possibly nil) and
        // returns a BOOL, which is this signature. The body cannot panic.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl FirstClickButton {
    /// A push button titled `title` that sends `action` to `target`.
    ///
    /// # Safety
    ///
    /// `target` must implement `action` as an action method (one object
    /// argument, no result) and stay alive as long as the button.
    unsafe fn new(
        title: &NSString,
        target: &AnyObject,
        action: Sel,
        _mtm: MainThreadMarker,
    ) -> Retained<Self> {
        // SAFETY: NSButton's `buttonWithTitle:target:action:` takes these
        // argument types and returns an autoreleased button; sent to this
        // subclass, it makes an instance of the subclass. The caller
        // vouches for the target and action.
        unsafe {
            msg_send![
                Self::class(),
                buttonWithTitle: title,
                target: Some(target),
                action: Some(action)
            ]
        }
    }
}

impl ClickTarget {
    fn new(mtm: MainThreadMarker, clicks: Arc<AtomicU32>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(clicks);
        // SAFETY: NSObject's `init` takes no arguments and returns the
        // initialized object.
        unsafe { msg_send![super(this), init] }
    }
}

/// The window and its views.
struct Ui {
    window: Retained<NSWindow>,
    label: Retained<NSTextField>,
    field: Retained<NSTextField>,
    button: Retained<FirstClickButton>,
    /// A control does not keep its target alive, so the window does.
    _target: Retained<ClickTarget>,
}

/// Creates the self-test window, hidden until [`TestWindow::show`].
///
/// It is a small floating window titled "Hop self-test" with an instruction
/// line, an editable text field and a "Click me" button. It must be used
/// inside [`run_with_ui`], whose event loop runs its AppKit work.
pub fn open_test_window() -> Result<Box<dyn TestWindow>> {
    let clicks = Arc::new(AtomicU32::new(0));
    let ui = on_main({
        let clicks = Arc::clone(&clicks);
        move |mtm| build_ui(mtm, clicks).map(|ui| Arc::new(MainOnly::new(ui, mtm)))
    })??;
    Ok(Box::new(MacTestWindow {
        ui: Some(ui),
        clicks,
    }))
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn build_ui(mtm: MainThreadMarker, clicks: Arc<AtomicU32>) -> Result<Ui> {
    // SAFETY: the window is told right below not to release itself when
    // closed, as a window owned through `Retained` must be.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, WINDOW_WIDTH, WINDOW_HEIGHT),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: Rust owns the window, so AppKit must not also release it.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("Hop self-test"));
    window.setLevel(NSFloatingWindowLevel);
    window.setHidesOnDeactivate(false);
    let content = window
        .contentView()
        .ok_or_else(|| PlatformError::Os("the test window has no content view".into()))?;

    let label = NSTextField::wrappingLabelWithString(ns_string!(""), mtm);
    let (x, y, width, height) = LABEL_FRAME;
    label.setFrame(rect(x, y, width, height));
    content.addSubview(&label);

    let field = NSTextField::textFieldWithString(ns_string!(""), mtm);
    let (x, y, width, height) = FIELD_FRAME;
    field.setFrame(rect(x, y, width, height));
    content.addSubview(&field);

    let target = ClickTarget::new(mtm, clicks);
    let target_object: &AnyObject = &target;
    // SAFETY: the target implements `buttonClicked:` as an action method,
    // and `Ui` keeps the target alive as long as the button.
    let button = unsafe {
        FirstClickButton::new(
            ns_string!("Click me"),
            target_object,
            sel!(buttonClicked:),
            mtm,
        )
    };
    button.setFrame(rect((WINDOW_WIDTH - 120.0) / 2.0, 20.0, 120.0, 32.0));
    content.addSubview(&button);

    window.center();
    Ok(Ui {
        window,
        label,
        field,
        button,
        _target: target,
    })
}

/// Brings the app forward and puts the keyboard focus in the text field.
fn focus_text_field(ui: &Ui, mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    // `activate` (macOS 14+) only asks; the self-test needs the focus now,
    // which the older call still grants.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    ui.window.makeKeyAndOrderFront(None);
    ui.window.orderFrontRegardless();
    let field: &NSResponder = &ui.field;
    ui.window.makeFirstResponder(Some(field));
}

/// Whether keys posted now would reach the text field: Hop is the active
/// app, the test window is its key window, and the field is being edited.
fn has_keyboard_focus(ui: &Ui, mtm: MainThreadMarker) -> bool {
    NSApplication::sharedApplication(mtm).isActive()
        && ui.window.isKeyWindow()
        && ui.field.currentEditor().is_some()
}

/// Asks `check` until it says yes or `timeout` has passed, `interval`
/// apart, and returns its last answer. An error from `check` ends the wait
/// at once.
fn wait_until(
    timeout: Duration,
    interval: Duration,
    mut check: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(interval);
    }
}

/// A point in Cocoa screen coordinates (origin at the bottom-left of the
/// primary display, y up) in Quartz global coordinates (origin at its
/// top-left, y down). `primary_height` is the primary display's height.
fn cocoa_to_quartz(point: Point, primary_height: f64) -> Point {
    Point::new(point.x, primary_height - point.y)
}

/// [`TestWindow`] for macOS.
struct MacTestWindow {
    /// `None` once closed.
    ui: Option<Arc<MainOnly<Ui>>>,
    clicks: Arc<AtomicU32>,
}

impl MacTestWindow {
    /// Runs `f` with the window's views on the main thread.
    fn with_ui<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Ui, MainThreadMarker) -> R + Send + 'static,
    ) -> Result<R> {
        let closed = || PlatformError::Os("the test window is closed".into());
        let ui = self.ui.clone().ok_or_else(closed)?;
        on_main(move |mtm| ui.get(mtm).map(|ui| f(ui, mtm)))?.ok_or_else(closed)
    }
}

impl TestWindow for MacTestWindow {
    fn show(&mut self, instructions: &str) -> Result<()> {
        let instructions = instructions.to_owned();
        self.with_ui(move |ui, mtm| {
            ui.label.setStringValue(&NSString::from_str(&instructions));
            focus_text_field(ui, mtm);
        })
    }

    fn set_instructions(&mut self, text: &str) -> Result<()> {
        let text = text.to_owned();
        self.with_ui(move |ui, _| ui.label.setStringValue(&NSString::from_str(&text)))
    }

    /// Brings the window forward with the keyboard in its text field, and
    /// fails unless that really happened. Activating is only a request:
    /// macOS can leave another app in front, and keys typed then would go
    /// to that app (a terminal, say) instead of the test window.
    fn focus_text(&mut self) -> Result<()> {
        self.with_ui(focus_text_field)?;
        let focused = if MainThreadMarker::new().is_some() {
            // Waiting here would stall the very event loop that completes
            // the activation, so look once.
            self.with_ui(has_keyboard_focus)?
        } else {
            wait_until(FOCUS_TIMEOUT, FOCUS_POLL, || {
                self.with_ui(has_keyboard_focus)
            })?
        };
        if focused {
            Ok(())
        } else {
            Err(PlatformError::Os(
                "the test window could not take the keyboard focus (another app stayed in \
                 front); click the test window, then try again"
                    .into(),
            ))
        }
    }

    fn text(&self) -> Result<String> {
        self.with_ui(|ui, _| {
            // While the field is being edited, the live text is in its
            // field editor; the field's own value catches up only later.
            match ui.field.currentEditor() {
                Some(editor) => editor.string().to_string(),
                None => ui.field.stringValue().to_string(),
            }
        })
    }

    fn clear_text(&mut self) -> Result<()> {
        self.with_ui(|ui, _| {
            let empty = ns_string!("");
            let was_editing = ui.field.currentEditor().is_some();
            ui.field.setStringValue(empty);
            match ui.field.currentEditor() {
                Some(editor) => editor.setString(empty),
                // Setting the value can end editing; give the focus back.
                None if was_editing => {
                    let field: &NSResponder = &ui.field;
                    ui.window.makeFirstResponder(Some(field));
                }
                None => {}
            }
        })
    }

    fn button_center(&self) -> Result<Point> {
        self.with_ui(|ui, _| {
            let in_window = ui.button.convertRect_toView(ui.button.bounds(), None);
            let on_screen = ui.window.convertRectToScreen(in_window);
            let center = Point::new(
                on_screen.origin.x + on_screen.size.width / 2.0,
                on_screen.origin.y + on_screen.size.height / 2.0,
            );
            cocoa_to_quartz(center, main_display_frame().size.height)
        })
    }

    fn button_clicks(&self) -> Result<u32> {
        Ok(self.clicks.load(Ordering::SeqCst))
    }

    fn close(&mut self) {
        if let Some(ui) = self.ui.take() {
            // The views are released on the main thread when this closure
            // (holding the last reference) finishes there.
            let _ = on_main(move |mtm| {
                if let Some(ui) = ui.get(mtm) {
                    ui.window.orderOut(None);
                    ui.window.close();
                }
            });
        }
    }
}

impl Drop for MacTestWindow {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_have_room_for_a_dozen_lines() {
        let (_, label_bottom, label_width, label_height) = LABEL_FRAME;
        let (_, field_bottom, _, field_height) = FIELD_FRAME;
        // The system font's lines are about 16 points apart.
        assert!(label_height >= 13.0 * 16.0, "{label_height}");
        assert!(label_width >= 440.0, "{label_width}");
        assert!(
            label_bottom > field_bottom + field_height,
            "overlaps the field"
        );
        assert!(
            label_bottom + label_height <= WINDOW_HEIGHT,
            "runs off the top"
        );
    }

    // `focus_text` waits with `wait_until`; these check the waiting alone,
    // since a test may not open a window.

    #[test]
    fn waiting_stops_as_soon_as_the_check_passes() {
        let mut calls = 0;
        let outcome = wait_until(Duration::from_secs(5), Duration::from_millis(1), || {
            calls += 1;
            Ok(calls == 3)
        });
        assert!(matches!(outcome, Ok(true)));
        assert_eq!(calls, 3);
    }

    #[test]
    fn waiting_gives_up_when_time_runs_out() {
        let start = Instant::now();
        let outcome = wait_until(Duration::from_millis(30), Duration::from_millis(5), || {
            Ok(false)
        });
        assert!(matches!(outcome, Ok(false)));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn waiting_ends_at_the_first_error() {
        let mut calls = 0;
        let outcome = wait_until(Duration::from_secs(5), Duration::from_millis(1), || {
            calls += 1;
            Err(PlatformError::Os("the test window is closed".into()))
        });
        assert!(outcome.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn cocoa_points_flip_into_quartz_coordinates() {
        // On a 982-point-tall primary display, Cocoa's bottom-left corner is
        // Quartz's (0, 982) and Cocoa's top edge is Quartz's y = 0.
        assert_eq!(
            cocoa_to_quartz(Point::new(0.0, 0.0), 982.0),
            Point::new(0.0, 982.0)
        );
        assert_eq!(
            cocoa_to_quartz(Point::new(100.0, 982.0), 982.0),
            Point::new(100.0, 0.0)
        );
        // A display above the primary one has negative Quartz y.
        assert_eq!(
            cocoa_to_quartz(Point::new(5.0, 1082.0), 982.0),
            Point::new(5.0, -100.0)
        );
        // A display below it has Cocoa y below zero.
        assert_eq!(
            cocoa_to_quartz(Point::new(5.0, -20.0), 982.0),
            Point::new(5.0, 1002.0)
        );
    }
}
