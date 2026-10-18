//! A running self-test: platform access, waits that watch for the emergency
//! chord and the time limit, and the guard that always gives the mouse and
//! keyboard back.
//!
//! [`Session`] is the guard. Its `Drop` runs on every way out of a run
//! (normal return, an early `?`, or a panic unwinding through it) and sets
//! capture to passive, releases every injected key and button, shows the
//! cursor again if it is parked, stops capture and closes the test window.

use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use hop_platform::{CaptureMode, Permission, PermissionState, Platform, PlatformError, TestWindow};
use hop_types::{InputEvent, Os, Point};

use crate::keys::chord_name;
use crate::observe::{self, Observation, Tally};
use crate::options::{SelfTestOptions, Timing};
use crate::report::StepId;

/// Why a step stopped before it finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// The user pressed the emergency chord.
    Aborted,
    /// The run's overall time limit ran out.
    TimeLimit,
}

/// Why a step could not finish.
#[derive(Debug)]
pub(crate) enum StepError {
    Interrupted(Interrupt),
    /// A platform call failed while the step was `doing` something (in words
    /// for the report).
    Platform {
        doing: &'static str,
        error: PlatformError,
    },
}

impl From<Interrupt> for StepError {
    fn from(interrupt: Interrupt) -> Self {
        StepError::Interrupted(interrupt)
    }
}

/// Adds what the step was doing to a platform error.
pub(crate) trait Doing<T> {
    fn doing(self, what: &'static str) -> Result<T, StepError>;
}

impl<T> Doing<T> for hop_platform::Result<T> {
    fn doing(self, what: &'static str) -> Result<T, StepError> {
        self.map_err(|error| StepError::Platform { doing: what, error })
    }
}

/// Whether something the run sets up lazily is working.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    NotStarted,
    Running,
    /// It could not start; the text is the error.
    Failed(String),
}

/// What a step needs before it may run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Needs {
    /// macOS Accessibility, for capture and injection.
    pub accessibility: bool,
    /// Capture running: the step watches input, or acts on the machine and
    /// so must be stoppable with the emergency chord.
    pub capture: bool,
    /// The test window: its button or its text box.
    pub window: bool,
    /// The keyboard visible to capture (macOS Secure Input off), so the
    /// emergency chord works while the step acts.
    pub keyboard: bool,
}

impl StepId {
    pub(crate) fn needs(self) -> Needs {
        let input = Needs {
            accessibility: true,
            capture: true,
            ..Needs::default()
        };
        match self {
            StepId::Environment => Needs::default(),
            StepId::Capture => Needs {
                window: true,
                ..input
            },
            StepId::Typing | StepId::Clicking => Needs {
                window: true,
                keyboard: true,
                ..input
            },
            StepId::Suppression | StepId::Park => Needs {
                keyboard: true,
                ..input
            },
            StepId::EdgePush => input,
        }
    }
}

/// What the test window says when a step waits for the user to start it.
pub(crate) const START_PROMPT: &str =
    "When you have read this, click the button below to start this step.";

/// The state of one self-test run. Dropping it gives everything back.
pub(crate) struct Session<'a> {
    pub(crate) platform: &'a mut Platform,
    pub(crate) window: &'a mut dyn TestWindow,
    pub(crate) os: Os,
    pub(crate) timing: Timing,
    print: bool,
    /// Each step waits for the user to click the button before it starts.
    wait_for_start: bool,
    pub(crate) time_limit: Duration,
    deadline: Instant,
    /// Set by the capture sink when the emergency chord is pressed.
    abort: Arc<AtomicBool>,
    /// Observations from the capture sink, once capture runs.
    events: Option<Receiver<Observation>>,
    pub(crate) capture: Status,
    pub(crate) window_status: Status,
    /// Read by the environment step; until then nothing that needs it runs.
    pub(crate) accessibility: PermissionState,
    /// Where the cursor goes back to, while it is parked.
    parked_at: Option<Point>,
    /// While true, every wait renews the suppression lease.
    renewing: bool,
    /// When the lease was last renewed.
    pub(crate) last_renew: Instant,
}

impl<'a> Session<'a> {
    pub(crate) fn new(
        platform: &'a mut Platform,
        window: &'a mut dyn TestWindow,
        options: &SelfTestOptions,
    ) -> Self {
        let now = Instant::now();
        Self {
            platform,
            window,
            os: Os::current(),
            timing: options.timing.clone().sanitized(),
            print: options.print_to_stdout,
            wait_for_start: options.wait_for_start,
            time_limit: options.time_limit,
            // A limit too large for the clock means no limit.
            deadline: now
                .checked_add(options.time_limit)
                .unwrap_or(now + Duration::from_secs(86_400 * 365)),
            abort: Arc::new(AtomicBool::new(false)),
            events: None,
            capture: Status::NotStarted,
            window_status: Status::NotStarted,
            accessibility: PermissionState::NotDetermined,
            parked_at: None,
            renewing: false,
            last_renew: now,
        }
    }

    // ----- stopping -------------------------------------------------------

    /// Fails if the user pressed the emergency chord or time ran out.
    pub(crate) fn check(&self) -> Result<(), Interrupt> {
        if self.abort.load(Ordering::SeqCst) {
            Err(Interrupt::Aborted)
        } else if Instant::now() >= self.deadline {
            Err(Interrupt::TimeLimit)
        } else {
            Ok(())
        }
    }

    /// Waits `duration`, watching for the emergency chord and the time limit
    /// and renewing the suppression lease while Hop suppresses input.
    pub(crate) fn sleep(&mut self, duration: Duration) -> Result<(), StepError> {
        self.sleep_until(Instant::now() + duration)
    }

    /// Like [`Session::sleep`], until `until`.
    pub(crate) fn sleep_until(&mut self, until: Instant) -> Result<(), StepError> {
        loop {
            self.check()?;
            self.renew_if_due()?;
            let now = Instant::now();
            if now >= until {
                return Ok(());
            }
            thread::sleep((until - now).min(self.timing.poll_interval));
        }
    }

    /// Calls `attempt` until it reports done or `timeout` passes, waiting a
    /// poll interval between tries; returns the last value either way.
    pub(crate) fn poll<T>(
        &mut self,
        timeout: Duration,
        mut attempt: impl FnMut(&mut Self) -> Result<(bool, T), StepError>,
    ) -> Result<T, StepError> {
        let end = Instant::now() + timeout;
        loop {
            let (done, value) = attempt(self)?;
            if done || Instant::now() >= end {
                return Ok(value);
            }
            self.sleep(self.timing.poll_interval)?;
        }
    }

    // ----- suppression and parking -----------------------------------------

    /// Starts suppressing physical input and keeps renewing the lease during
    /// every wait until [`Session::stop_renewing`] or [`Session::passive`].
    pub(crate) fn suppress(&mut self) -> Result<(), StepError> {
        self.check()?;
        // Renew from now on even if this call fails half way: fail_open
        // clears the flag, and a lease nobody renews lapses by itself.
        self.renewing = true;
        self.platform
            .capture
            .set_mode(CaptureMode::Suppress)
            .doing("starting to suppress input")?;
        self.last_renew = Instant::now();
        Ok(())
    }

    fn renew_if_due(&mut self) -> Result<(), StepError> {
        if self.renewing && self.last_renew.elapsed() >= self.timing.renew_interval {
            self.platform
                .capture
                .set_mode(CaptureMode::Suppress)
                .doing("renewing the suppression lease")?;
            self.last_renew = Instant::now();
        }
        Ok(())
    }

    /// Stops renewing the lease without ending it, so it lapses by itself.
    pub(crate) fn stop_renewing(&mut self) {
        self.renewing = false;
    }

    /// Ends suppression now.
    pub(crate) fn passive(&mut self) -> Result<(), StepError> {
        self.renewing = false;
        self.platform
            .capture
            .set_mode(CaptureMode::Passive)
            .doing("ending suppression")
    }

    /// Hides and pins the cursor; the guard shows it again at `restore_to`
    /// however the run ends.
    pub(crate) fn park(&mut self, restore_to: Point) -> Result<(), StepError> {
        self.check()?;
        // Recorded first, so a park that fails or panics half way is still
        // undone.
        self.parked_at = Some(restore_to);
        self.platform.cursor.park().doing("hiding the cursor")
    }

    /// Shows the cursor again where [`Session::park`] said.
    pub(crate) fn unpark(&mut self) -> Result<(), StepError> {
        match self.parked_at.take() {
            Some(at) => self
                .platform
                .cursor
                .restore(at)
                .doing("showing the cursor again"),
            None => Ok(()),
        }
    }

    // ----- injection --------------------------------------------------------

    /// Injects `events` in order, a key interval apart, stopping at once on
    /// the emergency chord. Returns how many were injected.
    pub(crate) fn inject_all(&mut self, events: &[InputEvent]) -> Result<u32, StepError> {
        let mut injected = 0;
        for event in events {
            self.check()?;
            self.renew_if_due()?;
            self.platform
                .injector
                .inject(event)
                .doing("injecting input")?;
            injected += 1;
            if !self.timing.key_interval.is_zero() {
                self.sleep(self.timing.key_interval)?;
            }
        }
        Ok(injected)
    }

    // ----- capture ------------------------------------------------------------

    /// Everything capture saw since the last drain.
    pub(crate) fn drain(&mut self) -> Vec<Observation> {
        self.events
            .as_ref()
            .map(|events| events.try_iter().collect())
            .unwrap_or_default()
    }

    /// Forgets what capture saw so far, so a step counts only its own events.
    pub(crate) fn discard(&mut self) {
        self.drain();
    }

    /// Adds captured events to `tally` until `done` says it has what it
    /// waits for, or `timeout` passes.
    pub(crate) fn collect_until(
        &mut self,
        tally: &mut Tally,
        timeout: Duration,
        done: impl Fn(&Tally) -> bool,
    ) -> Result<(), StepError> {
        self.poll(timeout, |session| {
            tally.add_all(&session.drain());
            Ok((done(tally), ()))
        })
    }

    /// Starts capture with the self-test's sink, unless it already ran.
    pub(crate) fn ensure_capture(&mut self) {
        if self.capture != Status::NotStarted {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let sink = observe::sink(tx, Arc::clone(&self.abort), self.os);
        // Should `start` panic half way, the guard still stops whatever it
        // got going: it stops capture unless it was never tried.
        self.capture = Status::Failed("capture did not finish starting".to_string());
        self.capture = match self.platform.capture.start(sink) {
            Ok(()) => {
                self.events = Some(rx);
                // Capture starts passive; say so anyway, in case a backend
                // kept an older mode.
                let _ = self.platform.capture.set_mode(CaptureMode::Passive);
                Status::Running
            }
            Err(error) => Status::Failed(error.to_string()),
        };
    }

    // ----- talking to the user -----------------------------------------------

    /// Opens the test window, unless that was already tried.
    pub(crate) fn ensure_window(&mut self) {
        if self.window_status != Status::NotStarted {
            return;
        }
        let intro = format!(
            "Hop self-test\n\nFollow the steps shown here. To stop at any time, press {}.",
            chord_name(self.os)
        );
        self.window_status = match self.window.show(&intro) {
            Ok(()) => Status::Running,
            Err(error) => Status::Failed(error.to_string()),
        };
    }

    /// Prints a line to stdout, if the options allow it.
    pub(crate) fn say(&self, text: &str) {
        print_line(self.print, text);
    }

    fn say_inline(&self, text: &str) {
        print_text(self.print, text);
    }

    /// Shows `body` in the test window, with how to stop underneath.
    fn show(&mut self, body: &str) {
        if self.window_status == Status::Running {
            let text = format!(
                "{body}\n\nTo stop the self-test, press {}.",
                chord_name(self.os)
            );
            // The same instructions are on stdout, so a window that cannot
            // update costs only that copy.
            let _ = self.window.set_instructions(&text);
        }
    }

    /// Shows a step's instructions in the window and prints them.
    pub(crate) fn instruct(&mut self, step: StepId, text: &str) {
        let heading = step.heading();
        self.say(&format!("\n{heading}\n{text}"));
        self.show(&format!("{heading}\n\n{text}"));
    }

    /// Replaces the instructions in the window while a step runs, keeping
    /// the step's heading.
    pub(crate) fn update(&mut self, step: StepId, text: &str) {
        self.say(text);
        self.show(&format!("{}\n\n{text}", step.heading()));
    }

    /// Counts down before a step acts, so nothing moves by surprise.
    pub(crate) fn countdown(&mut self, step: StepId, text: &str) -> Result<(), StepError> {
        let body = format!("{}\n\n{text}", step.heading());
        if self.wait_for_start {
            self.wait_for_start_click(&body)?;
        }
        let total = self.timing.countdown;
        if !total.is_zero() {
            let ticks = u32::try_from(total.as_millis().div_ceil(1000))
                .unwrap_or(u32::MAX)
                .max(1);
            let tick = total / ticks;
            self.say_inline("Starting in");
            for n in (1..=ticks).rev() {
                self.show(&format!("{body}\n\nStarting in {n}..."));
                self.say_inline(&format!(" {n}..."));
                if let Err(error) = self.sleep(tick) {
                    // End the countdown's line before whatever comes next.
                    self.say("");
                    return Err(error);
                }
            }
            self.say(" go!");
        }
        self.show(&format!("{body}\n\nGo!"));
        Ok(())
    }

    /// Shows a step's instructions and waits until the user clicks the test
    /// window's button, so every step starts only once it has been read.
    /// The wait ends early only by the emergency chord or the overall time
    /// limit. Steps read the button's click count after this, so the start
    /// click is never mistaken for one of theirs.
    fn wait_for_start_click(&mut self, body: &str) -> Result<(), StepError> {
        let Ok(before) = self.window.button_clicks() else {
            // No usable window: carry on, and the steps that need it say so.
            return Ok(());
        };
        self.show(&format!("{body}\n\n{START_PROMPT}"));
        self.say(START_PROMPT);
        let interval = self.timing.poll_interval.max(Duration::from_millis(20));
        loop {
            self.sleep(interval)?;
            match self.window.button_clicks() {
                Ok(now) if now > before => return Ok(()),
                Ok(_) => {}
                Err(_) => return Ok(()),
            }
        }
    }

    // ----- giving everything back ---------------------------------------------

    /// Gives the mouse and keyboard back: passive capture, nothing held
    /// down, the cursor shown where it was. Safe to call any number of
    /// times, and never panics, whatever the backend does.
    pub(crate) fn fail_open(&mut self) {
        self.renewing = false;
        let platform = &mut *self.platform;
        quietly(|| {
            let _ = platform.capture.set_mode(CaptureMode::Passive);
        });
        quietly(|| {
            let _ = platform.injector.release_all();
        });
        if let Some(at) = self.parked_at.take() {
            quietly(|| {
                let _ = platform.cursor.restore(at);
            });
        }
    }

    /// Whether Accessibility allows capture and injection.
    pub(crate) fn accessibility_ok(&self) -> bool {
        matches!(
            self.accessibility,
            PermissionState::Granted | PermissionState::NotNeeded
        )
    }

    /// Reads the Accessibility state; never prompts.
    pub(crate) fn read_accessibility(&mut self) -> PermissionState {
        self.accessibility = self.platform.permissions.state(Permission::Accessibility);
        self.accessibility
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.fail_open();
        // Stopping a capture that failed to start is harmless, and covers
        // one that half started.
        if self.capture != Status::NotStarted {
            let capture = &mut self.platform.capture;
            quietly(|| capture.stop());
        }
        if self.window_status == Status::Running {
            let window = &mut *self.window;
            quietly(|| window.close());
        }
    }
}

/// Runs `f`, swallowing any panic. Cleanup may run while a panic is already
/// unwinding, where a second one escaping would abort the whole process.
fn quietly(f: impl FnOnce()) {
    let _ = panic::catch_unwind(AssertUnwindSafe(f));
}

/// Prints `text` and a newline to stdout when `enabled`.
pub(crate) fn print_line(enabled: bool, text: &str) {
    print_text(enabled, &format!("{text}\n"));
}

/// Prints `text` to stdout when `enabled`. A closed stdout is not worth
/// stopping a self-test for, so errors are ignored (`print!` would panic).
fn print_text(enabled: bool, text: &str) {
    if enabled {
        let mut out = io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}
