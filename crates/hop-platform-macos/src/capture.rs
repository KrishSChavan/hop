//! Capturing local input with a Quartz event tap.
//!
//! The tap sits at the HID level, first in line (head insert), and is an
//! active tap, so it can swallow events while Hop controls the other
//! computer. It runs on a thread of its own with its own run loop, which
//! also runs the lease watchdog: a timer that ends suppression and gives
//! the cursor back when the runtime stops renewing the lease.
//!
//! The tap also does two small jobs of its own, neither of which ever
//! waits: it shows a concealed cursor again at the first physical mouse
//! event (see `cursor`), and it watches the left button for files dragged
//! toward a drop zone (see `catcher`).
//!
//! Both callbacks are `extern "C-unwind"` functions called by macOS. Each
//! wraps its body in `catch_unwind`, so a panic never unwinds into macOS,
//! and on a panic the event passes through untouched (fail open).

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hop_platform::{
    CaptureMode, CapturedEvent, INJECTED_TAG, InputCapture, Permission, PlatformError, Result,
};
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFMachPort, CFRetained, CFRunLoop, CFRunLoopRunResult,
    CFRunLoopSource, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};

use crate::catcher::TapDrops;
use crate::convert::{Converter, tap_mask};
use crate::permissions::accessibility_trusted;
use crate::shared::Shared;
use crate::{cursor, ffi};

/// How often the watchdog checks the lease, in seconds.
const WATCHDOG_INTERVAL: f64 = 0.1;

/// How long `start` waits for the tap thread to report in.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one run of the tap thread's run loop lasts before it checks
/// whether it should stop. `stop` also wakes it at once; this is a backstop.
const RUN_SLICE: f64 = 0.5;

/// What receives captured events.
type Sink = Box<dyn FnMut(CapturedEvent) + Send>;

/// [`InputCapture`] for macOS.
pub(crate) struct MacCapture {
    shared: Arc<Shared>,
    running: Option<Running>,
}

/// A started capture thread.
struct Running {
    thread: JoinHandle<()>,
    run_loop: RunLoopHandle,
    stop: Arc<AtomicBool>,
}

/// The tap thread's run loop, kept only to stop and wake it.
struct RunLoopHandle(CFRetained<CFRunLoop>);

// SAFETY: the handle is used for nothing but `CFRunLoopStop` and
// `CFRunLoopWakeUp`, which Apple documents as safe to call on another
// thread's run loop ("Threading Programming Guide": CFRunLoop's functions are
// thread-safe), and for releasing it, which CoreFoundation allows from any
// thread.
unsafe impl Send for RunLoopHandle {}

impl MacCapture {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            running: None,
        }
    }
}

impl InputCapture for MacCapture {
    fn start(&mut self, sink: Sink) -> Result<()> {
        if self.running.is_some() {
            return Err(PlatformError::Os("capture is already running".into()));
        }
        // An active tap needs the Accessibility permission. Checking first
        // (without a prompt) keeps the permission flow where the caller
        // puts it, and gives a clear error instead of a failed tap.
        if !accessibility_trusted(false) {
            return Err(PlatformError::PermissionDenied(Permission::Accessibility));
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("hop-capture".into())
            .spawn({
                let shared = Arc::clone(&self.shared);
                let stop = Arc::clone(&stop);
                move || run_tap(shared, sink, stop, ready_tx)
            })
            .map_err(|error| {
                PlatformError::Os(format!("could not start the capture thread: {error}"))
            })?;
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(run_loop)) => {
                self.running = Some(Running {
                    thread,
                    run_loop,
                    stop,
                });
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                // The thread is stuck somewhere in macOS; tell it to give up
                // whenever it gets going, and leave it be.
                stop.store(true, Ordering::SeqCst);
                Err(PlatformError::Os("the capture thread did not start".into()))
            }
        }
    }

    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        match mode {
            CaptureMode::Passive => {
                self.shared.clear_lease();
                Ok(())
            }
            CaptureMode::Suppress => {
                // Without a running tap nothing would be swallowed and no
                // watchdog would end the lease, so refuse rather than
                // pretend.
                if self.running.is_none() {
                    return Err(PlatformError::Os(
                        "capture is not running, so input cannot be suppressed".into(),
                    ));
                }
                self.shared.renew_lease(Instant::now());
                Ok(())
            }
        }
    }

    fn mode(&self) -> CaptureMode {
        self.shared.mode(Instant::now())
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop.store(true, Ordering::SeqCst);
            running.run_loop.0.stop();
            running.run_loop.0.wake_up();
            // A sink that calls `stop` runs on the tap thread itself, which
            // cannot wait for itself; it exits right after the callback.
            if running.thread.thread().id() != thread::current().id() {
                let _ = running.thread.join();
            }
        }
        // Fail open: with capture gone nothing is suppressed, and no
        // watchdog is left to give the cursor back.
        self.shared.clear_lease();
        cursor::unpark(&self.shared);
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Everything the callbacks need. It lives on the tap thread for as long as
/// the tap and the timer exist, and only that thread's callbacks touch it.
struct TapContext {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    /// The tap itself, to switch it back on when macOS turns it off.
    port: OnceCell<CFRetained<CFMachPort>>,
    state: RefCell<TapState>,
    /// A lapsed lease still owes the user their cursor: the watchdog found
    /// the cursor control busy on another thread and will try again.
    unpark_owed: Cell<bool>,
    /// A physical mouse event found the cursor concealed and the cursor
    /// control busy on another thread: the next event or the watchdog's
    /// next tick shows it instead.
    reveal_owed: Cell<bool>,
    /// The tap's part in catching dropped files.
    drops: RefCell<TapDrops>,
}

struct TapState {
    converter: Converter,
    sink: Sink,
}

/// What the tap does with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Swallow,
}

/// The tap thread: create the tap and the watchdog, report to `start`, run
/// until stopped, then take everything down before freeing the context.
fn run_tap(
    shared: Arc<Shared>,
    sink: Sink,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<RunLoopHandle>>,
) {
    ffi::make_current_thread_interactive();
    // A warp noted while no tap ran (failing open after an earlier capture
    // stopped, say) belongs to no event this tap will see.
    let _ = shared.take_warp();
    let start_cursor = cursor::current_position().ok();
    let drops = TapDrops::new(shared.drops());
    // The callbacks reach the context through this raw pointer. It is freed
    // below, only after the tap and the timer are gone.
    let raw = Box::into_raw(Box::new(TapContext {
        shared,
        stop,
        port: OnceCell::new(),
        state: RefCell::new(TapState {
            converter: Converter::new(start_cursor),
            sink,
        }),
        unpark_owed: Cell::new(false),
        reveal_owed: Cell::new(false),
        drops: RefCell::new(drops),
    }));
    // SAFETY: `raw` came from `Box::into_raw` just above and is freed only at
    // the end of this function. Everything, here and in the callbacks, uses
    // it through shared references; the mutable parts are in a `OnceCell`
    // and a `RefCell`, and all of it stays on this thread.
    let context = unsafe { &*raw };
    match Tap::create(raw.cast()) {
        Ok(tap) => {
            let _ = context.port.set(tap.port.clone());
            let _ = ready.send(Ok(RunLoopHandle(tap.run_loop.clone())));
            // SAFETY: reading an immutable CoreFoundation constant.
            let default_mode = unsafe { kCFRunLoopDefaultMode };
            while !context.stop.load(Ordering::SeqCst) {
                let result = CFRunLoop::run_in_mode(default_mode, RUN_SLICE, false);
                // "Finished" means the run loop has nothing left to run, so
                // the tap is gone; looping on would only spin.
                if result == CFRunLoopRunResult::Finished {
                    break;
                }
            }
            // After this no callback can run.
            tap.remove();
        }
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    }
    // SAFETY: `raw` came from `Box::into_raw`, nothing uses `context` past
    // this point, and no callback can run any more (the tap was never made,
    // or `remove` invalidated it and the timer).
    drop(unsafe { Box::from_raw(raw) });
}

/// The tap, its run-loop source and the watchdog timer, added to the
/// current thread's run loop.
struct Tap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    timer: CFRetained<CFRunLoopTimer>,
    run_loop: CFRetained<CFRunLoop>,
}

impl Tap {
    /// `info` must point to a [`TapContext`] that outlives the returned
    /// tap's [`Tap::remove`].
    fn create(info: *mut c_void) -> Result<Tap> {
        // SAFETY: `on_event` has the callback signature Quartz expects and
        // never unwinds; `info` points to the context, which `run_tap` keeps
        // alive until after `remove` has invalidated the tap.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                tap_mask(),
                Some(on_event),
                info,
            )
        }
        .ok_or_else(|| {
            if accessibility_trusted(false) {
                PlatformError::Os("macOS refused to create the event tap".into())
            } else {
                PlatformError::PermissionDenied(Permission::Accessibility)
            }
        })?;
        let fail = |what: &str| {
            CGEvent::tap_enable(&port, false);
            port.invalidate();
            PlatformError::Os(format!("could not set up the event tap: {what}"))
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            return Err(fail("no run-loop source"));
        };
        let Some(run_loop) = CFRunLoop::current() else {
            return Err(fail("no run loop"));
        };
        let mut timer_context = CFRunLoopTimerContext {
            version: 0,
            info,
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: `on_timer` has the callback signature CoreFoundation
        // expects and never unwinds; the context struct is copied by the
        // call, and its `info` stays valid as above.
        let timer = unsafe {
            CFRunLoopTimer::new(
                None,
                CFAbsoluteTimeGetCurrent() + WATCHDOG_INTERVAL,
                WATCHDOG_INTERVAL,
                0,
                0,
                Some(on_timer),
                &mut timer_context,
            )
        };
        let Some(timer) = timer else {
            return Err(fail("no watchdog timer"));
        };
        // SAFETY: reading an immutable CoreFoundation constant.
        let common = unsafe { kCFRunLoopCommonModes };
        run_loop.add_source(Some(&source), common);
        run_loop.add_timer(Some(&timer), common);
        CGEvent::tap_enable(&port, true);
        Ok(Tap {
            port,
            source,
            timer,
            run_loop,
        })
    }

    /// Switches the tap off and invalidates it, its source and the timer.
    fn remove(self) {
        CGEvent::tap_enable(&self.port, false);
        self.timer.invalidate();
        self.source.invalidate();
        self.port.invalidate();
    }
}

/// The tap callback.
unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    let verdict = catch_unwind(AssertUnwindSafe(|| {
        if info.is_null() {
            return Verdict::Pass;
        }
        // SAFETY: `info` is the TapContext that `run_tap` keeps alive until
        // the tap is invalidated, and the callbacks only ever take shared
        // references to it (mutable state sits in a RefCell).
        let context = unsafe { &*info.cast::<TapContext>() };
        // SAFETY: Quartz passes a valid event that lives for this call.
        let event = unsafe { event.as_ref() };
        context.on_event(ty, event)
    }));
    match verdict {
        Ok(Verdict::Swallow) => ptr::null_mut(),
        // On a panic the event passes untouched.
        Ok(Verdict::Pass) | Err(_) => event.as_ptr(),
    }
}

/// The watchdog timer callback.
unsafe extern "C-unwind" fn on_timer(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if info.is_null() {
            return;
        }
        // SAFETY: as in `on_event`.
        let context = unsafe { &*info.cast::<TapContext>() };
        context.on_timer();
    }));
}

impl TapContext {
    fn on_event(&self, ty: CGEventType, event: &CGEvent) -> Verdict {
        if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
            // macOS switched the tap off; switch it back on.
            if let Some(port) = self.port.get() {
                CGEvent::tap_enable(port, true);
            }
            return Verdict::Pass;
        }
        let physical = CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData)
            != INJECTED_TAG;
        // The user's own mouse shows a concealed cursor again, without ever
        // waiting for it: if another thread holds the cursor control right
        // now, the reveal is owed, and the next event or the watchdog's next
        // tick makes it.
        let wanted = reveals(ty, physical) && self.shared.concealed();
        let owed = settle_reveal(self.reveal_owed.get(), wanted, || {
            cursor::try_reveal(&self.shared)
        });
        self.reveal_owed.set(owed);
        // Only a sink that re-enters the run loop could find the state
        // borrowed; its event is then passed on unreported.
        if let Ok(mut state) = self.state.try_borrow_mut() {
            let state = &mut *state;
            if let Some(warp) = self.shared.take_warp() {
                state.converter.note_warp(warp);
            }
            let converted = state.converter.convert(ty, event);
            for event in converted.events() {
                (state.sink)(CapturedEvent {
                    event,
                    cursor: converted.cursor,
                    physical,
                });
            }
        }
        let now = Instant::now();
        // Hop's own injected events always get through.
        if physical && self.shared.suppressing(now) {
            return Verdict::Swallow;
        }
        // Files dragged toward a drop zone. Only events that reach the apps
        // can be part of a drag, Hop's own included: the other computer may
        // be dragging a file here.
        if let Ok(mut drops) = self.drops.try_borrow_mut() {
            drops.on_event(self.shared.drops(), ty, event, now);
        }
        Verdict::Pass
    }

    fn on_timer(&self) {
        if self.stop.load(Ordering::SeqCst) {
            if let Some(run_loop) = CFRunLoop::current() {
                run_loop.stop();
            }
            return;
        }
        // The lease ran out: suppression has already ended (the tap checks
        // the lease on every event); now give the cursor back, without ever
        // waiting for it on this thread.
        let owed = settle_cursor(&self.shared, self.unpark_owed.get(), Instant::now(), || {
            cursor::try_unpark(&self.shared)
        });
        self.unpark_owed.set(owed);
        // A reveal a mouse event could not make: try again, still without
        // waiting.
        let owed = settle_reveal(self.reveal_owed.get(), false, || {
            cursor::try_reveal(&self.shared)
        });
        self.reveal_owed.set(owed);
        // macOS normally announces when it switches a tap off, but a tap
        // found off anyway is switched back on here.
        if let Some(port) = self.port.get()
            && !CGEvent::tap_is_enabled(port)
        {
            CGEvent::tap_enable(port, true);
        }
    }
}

/// Whether an event of type `ty` shows a concealed cursor again: the user's
/// own mouse does (motion, drags, buttons and the wheel), keys do not, and
/// neither does anything Hop injected, which can still reach the tap after
/// the session that injected it ended.
fn reveals(ty: CGEventType, physical: bool) -> bool {
    physical
        && matches!(
            ty,
            CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged
                | CGEventType::LeftMouseDown
                | CGEventType::LeftMouseUp
                | CGEventType::RightMouseDown
                | CGEventType::RightMouseUp
                | CGEventType::OtherMouseDown
                | CGEventType::OtherMouseUp
                | CGEventType::ScrollWheel
        )
}

/// Shows a concealed cursor again if that is `wanted` now (a physical mouse
/// event found it concealed) or still `owed` from earlier; returns whether
/// the reveal is still owed afterwards. `try_reveal` returns `false` while
/// another thread holds the cursor control, and is not called at all when
/// there is nothing to do.
fn settle_reveal(owed: bool, wanted: bool, try_reveal: impl FnOnce() -> bool) -> bool {
    (owed || wanted) && !try_reveal()
}

/// One watchdog tick's work on the cursor; returns whether the cursor is
/// still owed back afterwards.
///
/// A lapse makes the cursor owed. It is handed back once `try_unpark` gets
/// the cursor control (it returns `false` while another thread holds it). A
/// new lease taken out before then settles the debt instead: that session
/// is parked again or about to be, and its own lapse or restore ends it.
fn settle_cursor(
    shared: &Shared,
    owed: bool,
    now: Instant,
    try_unpark: impl FnOnce() -> bool,
) -> bool {
    let owed = shared.take_lapse(now) || owed;
    if !owed || shared.suppressing(now) {
        return false;
    }
    !try_unpark()
}

#[cfg(test)]
mod tests {
    use hop_platform::SUPPRESS_LEASE;

    use super::*;
    use crate::shared::Shared;

    /// Runs one watchdog tick with a pretend cursor control that is `free`
    /// or busy; returns whether the cursor is still owed and whether an
    /// un-park was tried.
    fn tick(shared: &Shared, owed: bool, now: Instant, free: bool) -> (bool, bool) {
        let mut tried = false;
        let owed = settle_cursor(shared, owed, now, || {
            tried = true;
            free
        });
        (owed, tried)
    }

    #[test]
    fn a_lapse_gives_the_cursor_back_at_once_when_it_can() {
        let shared = Shared::new();
        let start = Instant::now();
        shared.renew_lease(start);
        assert_eq!(tick(&shared, false, start, true), (false, false));
        let lapsed = start + SUPPRESS_LEASE;
        assert_eq!(tick(&shared, false, lapsed, true), (false, true));
        // Given back once only.
        assert_eq!(tick(&shared, false, lapsed, true), (false, false));
    }

    #[test]
    fn a_busy_cursor_is_given_back_on_a_later_tick() {
        let shared = Shared::new();
        let start = Instant::now();
        shared.renew_lease(start);
        let lapsed = start + SUPPRESS_LEASE;
        assert_eq!(tick(&shared, false, lapsed, false), (true, true));
        let next = lapsed + Duration::from_millis(100);
        assert_eq!(tick(&shared, true, next, false), (true, true));
        assert_eq!(tick(&shared, true, next, true), (false, true));
    }

    #[test]
    fn a_new_lease_settles_a_cursor_still_owed() {
        let shared = Shared::new();
        let start = Instant::now();
        shared.renew_lease(start);
        let lapsed = start + SUPPRESS_LEASE;
        assert_eq!(tick(&shared, false, lapsed, false), (true, true));
        // The runtime starts suppressing again (a new session parks its
        // own cursor): the watchdog must not un-park that one.
        shared.renew_lease(lapsed);
        assert_eq!(tick(&shared, true, lapsed, true), (false, false));
    }

    /// Runs [`settle_reveal`] with a pretend cursor control that is `free`
    /// or busy; returns whether the reveal is still owed and whether it was
    /// tried.
    fn reveal(owed: bool, wanted: bool, free: bool) -> (bool, bool) {
        let mut tried = false;
        let owed = settle_reveal(owed, wanted, || {
            tried = true;
            free
        });
        (owed, tried)
    }

    #[test]
    fn a_reveal_the_cursor_control_was_too_busy_for_is_made_later() {
        // Nothing wanted or owed: the cursor control is left alone.
        assert_eq!(reveal(false, false, true), (false, false));
        // A mouse event finds the cursor concealed and the control free.
        assert_eq!(reveal(false, true, true), (false, true));
        // Busy: never waited for, but owed.
        assert_eq!(reveal(false, true, false), (true, true));
        // The next watchdog tick finds it busy again, then free.
        assert_eq!(reveal(true, false, false), (true, true));
        assert_eq!(reveal(true, false, true), (false, true));
        // A later mouse event pays a debt as well as a tick does.
        assert_eq!(reveal(true, true, true), (false, true));
    }

    #[test]
    fn only_the_user_s_own_mouse_shows_a_concealed_cursor() {
        use CGEventType as T;
        let mouse = [
            T::MouseMoved,
            T::LeftMouseDragged,
            T::RightMouseDragged,
            T::OtherMouseDragged,
            T::LeftMouseDown,
            T::LeftMouseUp,
            T::RightMouseDown,
            T::RightMouseUp,
            T::OtherMouseDown,
            T::OtherMouseUp,
            T::ScrollWheel,
        ];
        for ty in mouse {
            assert!(reveals(ty, true), "{ty:?}");
            assert!(!reveals(ty, false), "Hop's own {ty:?}");
        }
        let others = [
            T::KeyDown,
            T::KeyUp,
            T::FlagsChanged,
            T::TapDisabledByTimeout,
            T::TapDisabledByUserInput,
            T::Null,
        ];
        for ty in others {
            assert!(!reveals(ty, true), "{ty:?}");
            assert!(!reveals(ty, false), "{ty:?}");
        }
        // Every event the tap can see is one or the other.
        let mask = tap_mask();
        let seen = mouse.iter().chain(&others[..3]);
        assert_eq!(seen.fold(0, |m, ty| m | (1 << ty.0)), mask);
    }

    #[test]
    fn suppressing_needs_a_running_tap() {
        // Never started, so no tap exists; nothing here touches the OS
        // except clearing a lease that was never set.
        let mut capture = MacCapture::new(Shared::new());
        assert!(capture.set_mode(CaptureMode::Suppress).is_err());
        assert_eq!(capture.mode(), CaptureMode::Passive);
        assert!(capture.set_mode(CaptureMode::Passive).is_ok());
    }
}
