//! Whole self-test runs against the pretend machine in `fake`.

use std::fs;
use std::time::Duration;

use hop_platform::PermissionState;
use hop_types::{DisplayInfo, Rect};

use crate::fake::{Fake, Script, lock};
use crate::{Outcome, Report, SelfTestOptions, StepId, Stop, Timing, Verdict};

/// Durations short enough for tests, long enough for the pretend user's
/// thread to act in every step even on a busy machine. The pretend lease is
/// 300 ms, so the lapse check waits 600 ms.
fn fast() -> SelfTestOptions {
    SelfTestOptions {
        report_path: None,
        time_limit: Duration::from_secs(60),
        print_to_stdout: false,
        timing: Timing {
            countdown: Duration::from_millis(20),
            capture: Duration::from_millis(250),
            key_interval: Duration::ZERO,
            settle_timeout: Duration::from_millis(500),
            click_pause: Duration::from_millis(5),
            focus_pause: Duration::from_millis(5),
            suppress: Duration::from_millis(250),
            renew_interval: Duration::from_millis(20),
            lapse_wait: Duration::from_millis(600),
            park: Duration::from_millis(150),
            edge_push: Duration::from_secs(5),
            poll_interval: Duration::from_millis(2),
        },
    }
}

/// [`fast`] with a short edge push, for runs where no push comes.
fn fast_no_push() -> SelfTestOptions {
    let mut options = fast();
    options.timing.edge_push = Duration::from_millis(300);
    options
}

fn outcome(report: &Report, id: StepId) -> &Outcome {
    &report
        .step(id)
        .unwrap_or_else(|| panic!("no {id:?} in\n{report}"))
        .outcome
}

fn failure(report: &Report, id: StepId) -> String {
    match outcome(report, id) {
        Outcome::Fail(reason) => reason.clone(),
        other => panic!("{id:?} should have failed, but: {other:?}\n{report}"),
    }
}

fn skip(report: &Report, id: StepId) -> String {
    match outcome(report, id) {
        Outcome::Skipped(reason) => reason.clone(),
        other => panic!("{id:?} should have been skipped, but: {other:?}\n{report}"),
    }
}

fn assert_pass(report: &Report, id: StepId) {
    assert_eq!(outcome(report, id), &Outcome::Pass, "{id:?}\n{report}");
}

fn details(report: &Report, id: StepId) -> Vec<String> {
    report
        .step(id)
        .map(|step| step.details.clone())
        .unwrap_or_default()
}

/// The run gave everything back and left nothing behind.
fn assert_handed_back(fake: &Fake) {
    let w = fake.world();
    assert!(!w.suppressing(), "still suppressing");
    assert!(w.parked.is_none(), "the cursor is still parked");
    assert!(
        w.held_keys.is_empty() && w.held_buttons.is_empty(),
        "something is still held down"
    );
    assert!(w.sink.is_none(), "capture is still running");
    assert!(!w.window_open, "the test window is still open");
    assert!(
        !w.logged("clipboard"),
        "the self-test touched the clipboard"
    );
    assert!(
        !w.logged("permission request"),
        "the self-test asked for a permission"
    );
}

#[test]
fn a_well_behaved_machine_passes_every_step() {
    let mut fake = Fake::new(Script::cooperative(), |_| {});
    let report = fake.run(&fast(), &StepId::ALL);
    assert_eq!(report.verdict(), Verdict::Pass, "{report}");
    assert_eq!(report.steps.len(), 7);
    assert_eq!(report.stop, None);
    assert_eq!(
        report.step(StepId::Park).unwrap().note.as_deref(),
        Some("confirm you saw it disappear")
    );
    assert!(
        details(&report, StepId::Capture).contains(&"Cmd+C: seen".to_string())
            || details(&report, StepId::Capture).contains(&"Ctrl+C: seen".to_string())
    );
    assert_handed_back(&fake);
    let w = fake.world();
    assert_eq!(w.text, "ok");
    assert!(w.logged("release all") && w.logged("capture start") && w.logged("capture stop"));
}

#[test]
fn the_report_is_written_where_the_options_say() {
    let dir = std::env::temp_dir().join(format!("hop-selftest-run-{}", std::process::id()));
    let path = dir.join("Hop").join("selftest.txt");
    let mut options = fast();
    options.report_path = Some(path.clone());
    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&options, &[StepId::Environment]);
    assert_eq!(report.saved_to.as_deref(), Some(path.as_path()));
    assert_eq!(report.save_error, None);
    assert_eq!(fs::read_to_string(&path).unwrap(), report.to_string());
    fs::remove_dir_all(&dir).unwrap();

    // A path that cannot be written is reported, not fatal.
    let blocker = std::env::temp_dir().join(format!("hop-selftest-file-{}", std::process::id()));
    fs::write(&blocker, "a file, not a folder").unwrap();
    options.report_path = Some(blocker.join("selftest.txt"));
    let report = fake.run(&options, &[StepId::Environment]);
    assert_eq!(report.saved_to, None);
    assert!(report.save_error.unwrap().starts_with("could not write"));
    fs::remove_file(&blocker).unwrap();
}

#[test]
fn the_environment_lists_the_machine() {
    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&fast(), &[]);
    assert_eq!(report.steps.len(), 1, "only the environment step runs");
    assert_pass(&report, StepId::Environment);
    let details = details(&report, StepId::Environment);
    assert!(details.contains(&"Accessibility permission: granted".to_string()));
    assert!(details.contains(&"Secure Input: off".to_string()));
    assert!(details.contains(&"Display 1: 1512 x 982 at (0, 0), scale 2, primary".to_string()));
    // Nothing interactive happened.
    let w = fake.world();
    assert!(!w.logged("capture start") && !w.logged("window show"));
}

#[test]
fn displays_that_break_the_coordinate_contract_fail_the_environment() {
    let mut fake = Fake::new(Script::default(), |w| {
        w.displays = vec![DisplayInfo {
            id: 1,
            frame: Rect::new(-1512.0, 0.0, 1512.0, 982.0),
            scale: 2.0,
            primary: true,
        }];
    });
    let report = fake.run(&fast(), &[]);
    assert!(failure(&report, StepId::Environment).contains("(-1512, 0)"));
}

// ----- suppression and its lease ---------------------------------------------------------

#[test]
fn suppression_that_holds_the_cursor_still_passes() {
    let mut fake = Fake::new(Script::cooperative(), |_| {});
    let report = fake.run(&fast(), &[StepId::Suppression]);
    assert_pass(&report, StepId::Suppression);
    let details = details(&report, StepId::Suppression);
    assert!(
        details
            .iter()
            .any(|d| d.contains("passive again (the lease ran out by itself)"))
    );
    // Held the way the core holds it: suppressed and parked together, and
    // the lapse freed the cursor by itself.
    let w = lock(&fake.world);
    assert!(w.logged("park") && w.logged("lapse frees the cursor"));
    drop(w);
    assert_handed_back(&fake);
}

#[test]
fn a_parked_cursor_that_follows_the_mouse_fails() {
    let mut fake = Fake::new(Script::cooperative(), |w| w.park_moves_cursor = true);
    let report = fake.run(&fast(), &[StepId::Suppression]);
    assert!(failure(&report, StepId::Suppression).contains("the cursor moved 4 units"));
    assert_handed_back(&fake);
}

#[test]
fn a_lapse_that_leaves_the_cursor_pinned_fails() {
    let mut fake = Fake::new(Script::cooperative(), |w| {
        w.lapse_keeps_cursor_parked = true
    });
    let report = fake.run(&fast(), &[StepId::Suppression]);
    let reason = failure(&report, StepId::Suppression);
    assert!(
        reason.contains("the cursor stayed pinned after the lease ran out"),
        "{reason}"
    );
    // Capture itself did go passive.
    assert!(!reason.contains("lease did not run out"), "{reason}");
    assert_handed_back(&fake);
}

#[test]
fn suppression_needs_the_user_to_move_the_mouse() {
    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&fast(), &[StepId::Suppression]);
    assert!(failure(&report, StepId::Suppression).starts_with("no mouse movement was seen"));
}

#[test]
fn a_lease_that_never_lapses_fails_and_is_ended_anyway() {
    let mut fake = Fake::new(Script::cooperative(), |w| w.lease_never_lapses = true);
    let report = fake.run(&fast(), &[StepId::Suppression]);
    let reason = failure(&report, StepId::Suppression);
    assert!(
        reason.contains("the fail-open lease did not run out"),
        "{reason}"
    );
    assert!(!reason.contains("cursor moved"), "{reason}");
    // The self-test ended the lease itself.
    assert_handed_back(&fake);
}

// ----- typing --------------------------------------------------------------------------------

#[test]
fn typing_produces_the_right_text() {
    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&fast(), &[StepId::Typing]);
    assert_pass(&report, StepId::Typing);
    let details = details(&report, StepId::Typing);
    assert!(details.contains(
        &"Capture saw 34 of the 34 key events Hop typed, marked as Hop's own (physical = false)"
            .to_string()
    ));
    assert!(details.contains(&"Typed \"Hop test 123\" (26 key events): it arrived".to_string()));
    assert_eq!(fake.world().text, "ok");
    // The second half went in while suppressing.
    assert!(fake.world().logged("suppress"));
    assert_handed_back(&fake);
}

#[test]
fn typing_stops_after_one_key_when_the_keys_go_elsewhere() {
    let mut fake = Fake::new(Script::default(), |w| w.keys_go_elsewhere = true);
    let report = fake.run(&fast(), &[StepId::Typing]);
    let reason = failure(&report, StepId::Typing);
    assert!(
        reason.starts_with("the first key Hop typed did not reach"),
        "{reason}"
    );
    // Shift down, H down, H up, Shift up, and nothing else: in particular
    // no select-all, which would act on the other app.
    assert_eq!(fake.world().injected_key_events, 4);
    assert!(!fake.world().logged("suppress"));
    assert_handed_back(&fake);
}

#[test]
fn typing_failures_never_quote_the_text_box() {
    let mut fake = Fake::new(Script::default(), |w| w.shift_broken = true);
    let report = fake.run(&fast(), &[StepId::Typing]);
    let reason = failure(&report, StepId::Typing);
    assert!(reason.contains("wrong capitals"), "{reason}");
    assert_eq!(fake.world().text, "ok");
    // What the box held ("hop test 123") is not in the report.
    assert!(!report.to_string().contains("hop test"), "{report}");
}

#[test]
fn suppression_must_let_hops_own_keys_through() {
    let mut fake = Fake::new(Script::default(), |w| w.suppress_swallows_injected = true);
    let report = fake.run(&fast(), &[StepId::Typing]);
    let reason = failure(&report, StepId::Typing);
    assert!(
        reason.contains("suppression swallowed Hop's own keys"),
        "{reason}"
    );
    assert_handed_back(&fake);
}

#[test]
fn injected_keys_that_look_physical_fail() {
    let mut fake = Fake::new(Script::default(), |w| w.injected_look_physical = true);
    let report = fake.run(&fast(), &[StepId::Typing]);
    assert!(failure(&report, StepId::Typing).contains("INJECTED_TAG"));
}

#[test]
fn capture_must_see_hops_typing() {
    let mut fake = Fake::new(Script::default(), |w| w.capture_sees_injected = false);
    let report = fake.run(&fast(), &[StepId::Typing]);
    assert!(failure(&report, StepId::Typing).contains("capture saw only 0 of the 34"));
}

// ----- clicking --------------------------------------------------------------------------------

#[test]
fn clicking_counts_exactly_one_click() {
    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&fast(), &[StepId::Clicking]);
    assert_pass(&report, StepId::Clicking);
    assert!(
        details(&report, StepId::Clicking).contains(&"Clicks the button counted: 1".to_string())
    );
    assert_handed_back(&fake);

    let mut fake = Fake::new(Script::default(), |w| w.clicks_per_click = 2);
    let report = fake.run(&fast(), &[StepId::Clicking]);
    assert_eq!(
        failure(&report, StepId::Clicking),
        "the button counted 2 clicks instead of 1"
    );

    let mut fake = Fake::new(Script::default(), |w| w.clicks_per_click = 0);
    let report = fake.run(&fast(), &[StepId::Clicking]);
    assert_eq!(
        failure(&report, StepId::Clicking),
        "the button counted no click"
    );
}

#[test]
fn hop_never_clicks_anywhere_but_on_the_button() {
    // The cursor misses the button: no click at all.
    let mut fake = Fake::new(Script::default(), |w| w.move_offset = 3.0);
    let report = fake.run(&fast(), &[StepId::Clicking]);
    assert_eq!(
        failure(&report, StepId::Clicking),
        "the cursor went to (703, 403) instead of the button at (700, 400), so Hop did not click"
    );
    assert!(fake.world().held_buttons.is_empty());
    assert_eq!(fake.world().clicks, 0);
    assert!(
        details(&report, StepId::Clicking)
            .contains(&"Cursor after the move: (703, 403), 4.24 units from the center".to_string())
    );

    // The button is off screen: no move, no click.
    let mut fake = Fake::new(Script::default(), |w| {
        w.button_center = hop_types::Point::new(2000.0, 400.0);
    });
    let start = fake.world().cursor;
    let report = fake.run(&fast(), &[StepId::Clicking]);
    assert_eq!(
        failure(&report, StepId::Clicking),
        "the test window's button is off screen at (2000, 400), so Hop did not click"
    );
    assert_eq!(fake.world().cursor, start);
}

#[test]
fn a_click_reported_as_physical_fails() {
    let mut fake = Fake::new(Script::default(), |w| w.injected_look_physical = true);
    let report = fake.run(&fast(), &[StepId::Clicking]);
    assert!(failure(&report, StepId::Clicking).contains("INJECTED_TAG"));
}

// ----- capture ---------------------------------------------------------------------------------

#[test]
fn capture_sees_what_the_user_does() {
    let mut fake = Fake::new(Script::cooperative(), |_| {});
    let report = fake.run(&fast(), &[StepId::Capture]);
    assert_pass(&report, StepId::Capture);
    let details = details(&report, StepId::Capture);
    assert!(
        details
            .iter()
            .any(|d| d.ends_with("(which keys is never recorded)"))
    );

    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&fast(), &[StepId::Capture]);
    let reason = failure(&report, StepId::Capture);
    assert!(reason.starts_with("Hop did not see mouse movement, a left click, scrolling and"));
}

// ----- the edge push ------------------------------------------------------------------------

#[test]
fn an_edge_push_reaches_the_real_core() {
    let mut fake = Fake::new(Script::cooperative(), |_| {});
    let report = fake.run(&fast(), &[StepId::EdgePush]);
    assert_pass(&report, StepId::EdgePush);
    assert!(
        details(&report, StepId::EdgePush)
            .iter()
            .any(|d| d.starts_with("Hop's core started a crossing"))
    );
    // Nothing the pretend core asked for was carried out.
    let w = fake.world();
    assert!(!w.logged("suppress") && !w.logged("park"));
    drop(w);
    assert_handed_back(&fake);
}

#[test]
fn an_edge_push_through_a_second_screen_reaches_the_core() {
    // A fast push, so the long way across both screens takes few events
    // even where threads sleep in coarse steps (Windows).
    let script = Script {
        push: Some(100.0),
        ..Script::cooperative()
    };
    let mut fake = Fake::new(script, |w| {
        w.displays.push(DisplayInfo {
            id: 2,
            frame: Rect::new(1512.0, -100.0, 1920.0, 1080.0),
            scale: 1.0,
            primary: false,
        });
    });
    let report = fake.run(&fast(), &[StepId::EdgePush]);
    assert_pass(&report, StepId::EdgePush);
    assert!(
        fake.world().cursor.x > 3400.0,
        "the push went across both screens"
    );
}

#[test]
fn edge_push_failures_say_why() {
    let left = Script {
        push: Some(-25.0),
        ..Script::default()
    };
    let mut fake = Fake::new(left, |_| {});
    let report = fake.run(&fast_no_push(), &[StepId::EdgePush]);
    assert!(failure(&report, StepId::EdgePush).contains("never reached the right edge"));

    // Start next to the edge, so the short step reaches it however slowly
    // the pretend user's thread runs.
    let mut fake = Fake::new(Script::cooperative(), |w| {
        w.clamp_attempted_motion = true;
        w.cursor = hop_types::Point::new(1490.0, 491.0);
    });
    let report = fake.run(&fast_no_push(), &[StepId::EdgePush]);
    assert!(failure(&report, StepId::EdgePush).contains("the motion the user attempted"));

    let mut fake = Fake::new(Script::default(), |_| {});
    let report = fake.run(&fast_no_push(), &[StepId::EdgePush]);
    assert!(failure(&report, StepId::EdgePush).starts_with("no mouse movement was seen"));
}

// ----- stopping, crashing and skipping ----------------------------------------------------

#[test]
fn the_emergency_chord_stops_the_run_and_gives_everything_back() {
    let script = Script {
        chord_in: Some(StepId::Suppression),
        ..Script::cooperative()
    };
    let mut fake = Fake::new(script, |_| {});
    let report = fake.run(&fast(), &StepId::ALL);
    assert_eq!(report.verdict(), Verdict::Stopped, "{report}");
    assert_eq!(report.stop, Some(Stop::EmergencyChord(StepId::Suppression)));
    assert!(skip(&report, StepId::Suppression).contains("emergency chord"));
    for id in [StepId::Park, StepId::EdgePush] {
        assert!(skip(&report, id).starts_with("not run: you stopped the self-test"));
    }
    for id in [
        StepId::Environment,
        StepId::Capture,
        StepId::Typing,
        StepId::Clicking,
    ] {
        assert_pass(&report, id);
    }
    assert_handed_back(&fake);
}

#[test]
fn the_chord_while_the_cursor_is_hidden_brings_it_back() {
    let script = Script {
        chord_while_parked: true,
        ..Script::default()
    };
    let mut fake = Fake::new(script, |_| {});
    let start = fake.world().cursor;
    let report = fake.run(&fast(), &[StepId::Park, StepId::EdgePush]);
    assert_eq!(
        report.stop,
        Some(Stop::EmergencyChord(StepId::Park)),
        "{report}"
    );
    assert_handed_back(&fake);
    let w = fake.world();
    assert!(w.logged(&format!("restore {} {}", start.x, start.y)));
    assert_eq!(w.cursor, start);
}

#[test]
fn a_panic_while_parked_is_contained_and_the_cursor_comes_back() {
    let mut fake = Fake::new(Script::default(), |w| w.panic_when_parked = true);
    let start = fake.world().cursor;
    let report = fake.run(&fast(), &[StepId::Park, StepId::EdgePush]);
    assert_eq!(report.stop, Some(Stop::Crashed(StepId::Park)));
    assert_eq!(report.verdict(), Verdict::Fail);
    let reason = failure(&report, StepId::Park);
    assert!(reason.contains("crashed during this step"), "{reason}");
    assert!(
        reason.contains("the pretend cursor broke while parked"),
        "{reason}"
    );
    assert!(skip(&report, StepId::EdgePush).contains("crashed in an earlier step"));
    assert_handed_back(&fake);
    let w = fake.world();
    assert!(w.logged(&format!("restore {} {}", start.x, start.y)));
    assert!(w.logged("release all") && w.logged("passive"));
}

#[test]
fn a_backend_that_also_panics_during_cleanup_is_still_cleaned_up() {
    let mut fake = Fake::new(Script::default(), |w| {
        w.panic_when_parked = true;
        w.panic_on_release_all = true;
    });
    let report = fake.run(&fast(), &[StepId::Park]);
    assert_eq!(report.stop, Some(Stop::Crashed(StepId::Park)));
    let w = fake.world();
    assert!(
        w.parked.is_none(),
        "the cursor came back despite the second panic"
    );
    assert!(!w.suppressing() && w.sink.is_none() && !w.window_open);
}

#[test]
fn a_panic_in_the_window_is_contained() {
    let mut fake = Fake::new(Script::default(), |w| w.panic_on_button_center = true);
    let report = fake.run(&fast(), &[StepId::Clicking, StepId::Suppression]);
    assert_eq!(report.stop, Some(Stop::Crashed(StepId::Clicking)));
    assert!(failure(&report, StepId::Clicking).contains("the pretend window broke"));
    assert!(skip(&report, StepId::Suppression).starts_with("not run"));
    assert_handed_back(&fake);
}

#[test]
fn without_accessibility_every_input_step_is_skipped_without_asking() {
    let mut fake = Fake::new(Script::cooperative(), |w| {
        w.accessibility = PermissionState::Denied;
    });
    let report = fake.run(&fast(), &StepId::ALL);
    assert_eq!(report.verdict(), Verdict::Incomplete, "{report}");
    assert_pass(&report, StepId::Environment);
    for id in &StepId::ALL[1..] {
        let reason = skip(&report, *id);
        assert!(
            reason.contains("needs the Accessibility permission, which is not granted"),
            "{reason}"
        );
        assert!(reason.contains("System Settings > Privacy & Security > Accessibility"));
    }
    let w = fake.world();
    assert!(!w.logged("capture start"), "capture must not even be tried");
    assert!(!w.logged("window show"));
    assert!(!w.logged("permission request"));
}

#[test]
fn secure_input_skips_the_steps_that_act() {
    let mut fake = Fake::new(Script::cooperative(), |w| w.secure_input = true);
    let report = fake.run(&fast(), &StepId::ALL);
    for id in [
        StepId::Typing,
        StepId::Clicking,
        StepId::Suppression,
        StepId::Park,
    ] {
        assert!(skip(&report, id).starts_with("macOS Secure Input is on"));
    }
    // Watching needs no chord; this pretend keyboard is visible anyway.
    assert_pass(&report, StepId::Capture);
    assert_pass(&report, StepId::EdgePush);
    assert_eq!(report.verdict(), Verdict::Incomplete);
    assert_handed_back(&fake);
}

#[test]
fn the_time_limit_stops_a_run() {
    let mut options = fast();
    options.time_limit = Duration::from_millis(100);
    let mut fake = Fake::new(Script::cooperative(), |_| {});
    let report = fake.run(&options, &StepId::ALL);
    let stopped_in = match report.stop {
        Some(Stop::TimeLimit(id)) => id,
        other => panic!("expected the time limit, got {other:?}\n{report}"),
    };
    assert!(failure(&report, stopped_in).contains("time limit of 0.1 seconds ran out"));
    for step in report.steps.iter().filter(|step| step.id > stopped_in) {
        assert_eq!(
            step.outcome,
            Outcome::Skipped("not run: the self-test's time limit ran out".to_string())
        );
    }
    assert_eq!(report.verdict(), Verdict::Fail);
    assert_handed_back(&fake);
}

#[test]
fn capture_that_cannot_start_skips_the_input_steps() {
    let mut fake = Fake::new(Script::cooperative(), |w| w.capture_fails = true);
    let report = fake.run(&fast(), &StepId::ALL);
    for id in &StepId::ALL[1..] {
        let reason = skip(&report, *id);
        assert!(
            reason.starts_with("input capture could not start"),
            "{reason}"
        );
        assert!(
            reason.contains("the hook could not be installed"),
            "{reason}"
        );
    }
    assert!(!fake.world().logged("window show"));
}

#[test]
fn without_a_window_only_the_steps_that_need_it_are_skipped() {
    let mut fake = Fake::new(Script::cooperative(), |w| w.window_fails = true);
    let report = fake.run(&fast_no_push(), &StepId::ALL);
    for id in [StepId::Capture, StepId::Typing, StepId::Clicking] {
        assert!(skip(&report, id).starts_with("the test window could not open"));
    }
    // These still run, with their instructions on stdout only. (The
    // pretend user reads only the window, so it sits them out.)
    for id in [StepId::Suppression, StepId::Park, StepId::EdgePush] {
        assert!(
            !matches!(outcome(&report, id), Outcome::Skipped(_)),
            "{id:?}\n{report}"
        );
    }
    assert_pass(&report, StepId::Park);
    assert_handed_back(&fake);
}
