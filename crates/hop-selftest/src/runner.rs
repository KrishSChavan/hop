//! Running the steps in order, and what happens when one cannot finish.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::time::SystemTime;

use hop_platform::{Permission, Platform, PlatformError, TestWindow};
use hop_types::Os;

use crate::keys::chord_name;
use crate::options::SelfTestOptions;
use crate::report::{Outcome, Report, StepId, StepReport, Stop};
use crate::session::{Interrupt, Needs, Session, Status, StepError, print_line};
use crate::steps::{self, permission_words, secs};

/// Runs the whole guided self-test on this machine and returns its report,
/// which is also written to [`SelfTestOptions::report_path`].
///
/// The steps, each shown in `window` and printed to stdout, with a countdown
/// before anything moves:
///
/// 1. **Environment**: the OS, the displays, Accessibility and Secure Input.
/// 2. **Capture** (passive): the user moves the mouse, clicks the window's
///    button, scrolls and presses the copy shortcut; Hop counts what it saw.
/// 3. **Typing**: Hop types into the window's text box, then selects it all
///    and types over it while suppressing physical input.
/// 4. **Clicking**: Hop moves the cursor onto the window's button and clicks.
/// 5. **Holding the mouse still**: Hop suppresses the mouse while the user
///    moves it, then stops renewing and checks that suppression lapses.
/// 6. **Hiding the cursor**: Hop parks the cursor, then restores it.
/// 7. **Edge push**: the user pushes against the right edge, and Hop's real
///    core, with a pretend peer on the right, must notice. Nothing the core
///    answers is carried out.
///
/// Steps that need Accessibility are skipped when it is not granted, and
/// nothing here ever asks for a permission. Whatever happens, including an
/// error or a panic in a backend, Hop gives the mouse and keyboard back:
/// capture goes passive, injected keys and buttons are released, and a
/// hidden cursor is shown again. The emergency chord (Ctrl+Option+Shift+Esc
/// on a Mac, Ctrl+Alt+Shift+Esc on a PC) stops the run at any point, and so
/// does [`SelfTestOptions::time_limit`]. Killing the process (Ctrl+C in the
/// terminal) skips all of that, so the instructions point to the chord.
///
/// This blocks the calling thread for about a minute and sleeps on it
/// between checks. If the backend's test window needs the calling thread's
/// event loop (AppKit windows live on the main thread), call this from
/// another thread while that loop runs.
pub fn run(
    platform: &mut Platform,
    window: &mut dyn TestWindow,
    options: &SelfTestOptions,
) -> Report {
    run_selected(platform, window, options, &StepId::ALL)
}

/// Like [`run`], but runs only the `steps` listed, in their usual order.
/// The environment step always runs, since the others depend on what it
/// finds.
pub fn run_selected(
    platform: &mut Platform,
    window: &mut dyn TestWindow,
    options: &SelfTestOptions,
    steps: &[StepId],
) -> Report {
    let started = SystemTime::now();
    let plan: Vec<StepId> = StepId::ALL
        .into_iter()
        .filter(|id| *id == StepId::Environment || steps.contains(id))
        .collect();
    let mut done: Vec<StepReport> = Vec::with_capacity(plan.len());
    let mut stop: Option<Stop> = None;
    // The step running now. It lives out here so that what it measured
    // before a crash still makes it into the report.
    let mut running: Option<StepReport> = None;

    // The session is the guard: however this closure ends, including by a
    // panic unwinding out of a backend, dropping it gives input back.
    let finished = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut session = Session::new(platform, window, options);
        session.say(&format!(
            "Hop self-test. To stop at any time, press {}.",
            chord_name(session.os)
        ));
        for &id in &plan {
            let report = running.insert(StepReport::new(id, Outcome::Pass));
            let interrupt = run_step(&mut session, report);
            session.say(&format!("Result: {}", short_status(&report.outcome)));
            done.extend(running.take());
            // Whatever the step did, the next one starts with the mouse and
            // keyboard given back.
            session.fail_open();
            if let Some(interrupt) = interrupt {
                stop = Some(match interrupt {
                    Interrupt::Aborted => Stop::EmergencyChord(id),
                    Interrupt::TimeLimit => Stop::TimeLimit(id),
                });
                break;
            }
        }
    }));
    if let Err(payload) = finished {
        let mut crashed = running.take().unwrap_or_else(|| {
            // Nothing outside a step can panic, but blame the last step
            // rather than lose the crash.
            let id = plan.last().copied().unwrap_or(StepId::Environment);
            done.retain(|report| report.id != id);
            StepReport::new(id, Outcome::Pass)
        });
        crashed.outcome = Outcome::Fail(format!(
            "the self-test crashed during this step ({}); Hop gave the mouse and keyboard back",
            panic_message(payload.as_ref())
        ));
        stop = Some(Stop::Crashed(crashed.id));
        done.push(crashed);
    }

    let not_run = match stop {
        Some(Stop::EmergencyChord(_)) => {
            "not run: you stopped the self-test with the emergency chord"
        }
        Some(Stop::TimeLimit(_)) => "not run: the self-test's time limit ran out",
        Some(Stop::Crashed(_)) => "not run: the self-test crashed in an earlier step",
        None => "not run",
    };
    for &id in &plan {
        if !done.iter().any(|report| report.id == id) {
            done.push(StepReport::new(id, Outcome::Skipped(not_run.to_string())));
        }
    }
    done.sort_by_key(|report| report.id);

    let mut report = Report {
        os: Os::current(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started,
        steps: done,
        stop,
        saved_to: None,
        save_error: None,
    };
    if let Some(path) = &options.report_path {
        match report.write_to(path) {
            Ok(()) => report.saved_to = Some(path.clone()),
            Err(error) => {
                report.save_error = Some(format!("could not write {}: {error}", path.display()));
            }
        }
    }

    let print = options.print_to_stdout;
    print_line(print, "");
    for step in &report.steps {
        print_line(print, &step.summary_line());
    }
    print_line(print, &format!("Overall: {}", report.verdict_line()));
    match (&report.saved_to, &report.save_error) {
        (Some(path), _) => print_line(print, &format!("Full report: {}", path.display())),
        (None, Some(error)) => print_line(print, &format!("The report was not saved: {error}")),
        (None, None) => {}
    }
    report
}

/// Runs the step `report` is for, or skips it when it cannot run here, and
/// fills in the outcome. Returns why the run must stop, if it must.
fn run_step(s: &mut Session, report: &mut StepReport) -> Option<Interrupt> {
    let id = report.id;
    if let Some(reason) = skip_reason(s, id) {
        s.say(&format!("\n{}", id.heading()));
        report.outcome = Outcome::Skipped(reason);
        return None;
    }
    let (outcome, interrupt) = match steps::run(s, report) {
        Ok(outcome) => (outcome, None),
        Err(StepError::Interrupted(Interrupt::Aborted)) => (
            Outcome::Skipped(
                "stopped: you pressed the emergency chord, so Hop gave the mouse and keyboard \
                 back"
                    .to_string(),
            ),
            Some(Interrupt::Aborted),
        ),
        Err(StepError::Interrupted(Interrupt::TimeLimit)) => (
            Outcome::Fail(format!(
                "stopped: the self-test's overall time limit of {} ran out",
                secs(s.time_limit)
            )),
            Some(Interrupt::TimeLimit),
        ),
        Err(StepError::Platform { doing, error }) => (platform_outcome(doing, &error), None),
    };
    report.outcome = outcome;
    interrupt
}

/// Why step `id` cannot run here, if it cannot. Starts capture and opens the
/// test window the first time a step needs them.
fn skip_reason(s: &mut Session, id: StepId) -> Option<String> {
    let needs = id.needs();
    if needs.accessibility && !s.accessibility_ok() {
        return Some(format!(
            "this step needs the Accessibility permission, which is {}. {}",
            permission_words(s.accessibility),
            grant_hint(Permission::Accessibility)
        ));
    }
    if needs.capture {
        s.ensure_capture();
        if let Status::Failed(error) = &s.capture {
            return Some(format!(
                "input capture could not start ({error}), so this step could neither watch \
                 input nor be stopped with the emergency chord"
            ));
        }
    }
    // Every interactive step shows its instructions in the window.
    if needs != Needs::default() {
        s.ensure_window();
    }
    if needs.window
        && let Status::Failed(error) = &s.window_status
    {
        return Some(format!("the test window could not open ({error})"));
    }
    if needs.keyboard && s.platform.system.secure_input_active() {
        return Some(
            "macOS Secure Input is on (a password field has focus somewhere, or Secure Keyboard \
             Entry is turned on in Terminal), so Hop cannot see the keyboard and the emergency \
             chord would not work during this step. Close the password field and run the \
             self-test again"
                .to_string(),
        );
    }
    None
}

/// A platform error as a step outcome: a missing permission or a feature a
/// backend does not have yet is a skip, anything else a failure.
fn platform_outcome(doing: &str, error: &PlatformError) -> Outcome {
    match error {
        PlatformError::PermissionDenied(permission) => Outcome::Skipped(format!(
            "{doing} needs the {permission} permission. {}",
            grant_hint(*permission)
        )),
        PlatformError::Unsupported => {
            Outcome::Skipped(format!("{doing} is not supported on this platform yet"))
        }
        PlatformError::Os(message) => Outcome::Fail(format!("{doing} failed: {message}")),
    }
}

fn grant_hint(permission: Permission) -> &'static str {
    match permission {
        Permission::Accessibility => {
            "Open System Settings > Privacy & Security > Accessibility, turn on the app you run \
             Hop from (Hop itself, or your terminal), then run the self-test again"
        }
        Permission::LocalNetwork => {
            "Open System Settings > Privacy & Security > Local Network, turn on the app you run \
             Hop from, then run the self-test again"
        }
    }
}

fn short_status(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Pass => "PASS".to_string(),
        Outcome::Fail(reason) => format!("FAIL: {reason}"),
        Outcome::Skipped(reason) => format!("SKIPPED: {reason}"),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "no message".to_string()
    }
}
