//! The seven steps of the self-test.
//!
//! Each step shows its instructions, counts down, does its part and returns
//! an [`Outcome`], adding what it measured to the step's details. Details
//! hold counts, positions and states only: never which keys were pressed,
//! never the contents of the test window's text box (the user may have typed
//! into it), and never anything from the clipboard.

use std::time::{Duration, Instant};

use hop_core::{Action, Core, CoreConfig, DeviceLayout, Event, Layout};
use hop_platform::{CaptureMode, PermissionState};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, InputEvent, MouseButton, Os, Point, Rect,
};

use crate::keys::{KEY_A, command_key, shortcut_events, shortcut_name, typing_events};
use crate::observe::{Observed, Tally};
use crate::report::{Outcome, StepId, StepReport};
use crate::session::{Doing, Session, StepError};

/// What the typing step types first.
pub(crate) const FIRST_TEXT: &str = "Hop test 123";
/// What it types over the first text after select-all.
pub(crate) const SECOND_TEXT: &str = "ok";
/// How far, in logical units, an injected move or a restored cursor may
/// land from where it should.
const POSITION_TOLERANCE: f64 = 2.0;
/// How far the cursor may move while it should stand still.
const STILL_TOLERANCE: f64 = 3.0;

/// Runs step `report.id`.
pub(crate) fn run(s: &mut Session, report: &mut StepReport) -> Result<Outcome, StepError> {
    match report.id {
        StepId::Environment => environment(s, report),
        StepId::Capture => capture(s, report),
        StepId::Typing => typing(s, report),
        StepId::Clicking => clicking(s, report),
        StepId::Suppression => suppression(s, report),
        StepId::Park => park(s, report),
        StepId::EdgePush => edge_push(s, report),
    }
}

// ----- 1. Environment ---------------------------------------------------------

fn environment(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    s.instruct(
        StepId::Environment,
        "Checking this computer's displays and permissions.",
    );
    r.details.push(format!("Operating system: {}", s.os));
    let accessibility = s.read_accessibility();
    r.details.push(format!(
        "Accessibility permission: {}",
        permission_words(accessibility)
    ));
    let secure = s.platform.system.secure_input_active();
    r.details.push(format!(
        "Secure Input: {}",
        if secure {
            "on (a password field has focus somewhere, so capture cannot see the keyboard)"
        } else {
            "off"
        }
    ));
    let displays = s
        .platform
        .displays
        .displays()
        .doing("reading the list of displays")?;
    r.details.extend(displays.iter().map(describe_display));
    Ok(match check_displays(&displays) {
        Ok(()) => Outcome::Pass,
        Err(reason) => Outcome::Fail(reason),
    })
}

/// A permission state in words.
pub(crate) fn permission_words(state: PermissionState) -> &'static str {
    match state {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "not granted",
        PermissionState::NotDetermined => "not granted yet",
        PermissionState::NotNeeded => "not needed on this OS",
    }
}

fn describe_display(display: &DisplayInfo) -> String {
    format!(
        "Display {}: {} x {} at {}, scale {}{}",
        display.id,
        num(display.frame.size.width),
        num(display.frame.size.height),
        point(display.frame.origin),
        num(display.scale),
        if display.primary { ", primary" } else { "" }
    )
}

/// Whether the displays follow the platform contract's coordinates and are
/// usable by Hop's core.
fn check_displays(displays: &[DisplayInfo]) -> Result<(), String> {
    if displays.is_empty() {
        return Err("the platform reported no displays".to_string());
    }
    let primaries: Vec<&DisplayInfo> = displays.iter().filter(|d| d.primary).collect();
    let [primary] = primaries.as_slice() else {
        return Err(format!(
            "{} displays are marked as the primary one; exactly one should be",
            primaries.len()
        ));
    };
    let origin = primary.frame.origin;
    if origin.x.abs() > 0.5 || origin.y.abs() > 0.5 {
        return Err(format!(
            "the primary display's top-left corner is at {}, but Hop's coordinates put it at \
             (0, 0)",
            point(origin)
        ));
    }
    let me = DeviceLayout::new(
        DeviceId::new("this-computer"),
        Point::default(),
        displays.to_vec(),
    );
    Layout::new([me])
        .map(|_| ())
        .map_err(|error| format!("Hop's core cannot use these displays: {error}"))
}

// ----- 2. Capture -----------------------------------------------------------------

fn capture(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    let copy = shortcut_name(s.os, 'C');
    let text = format!(
        "Move the mouse, click the button below once, scroll, then press {copy}.\n\
         You have {} for this.",
        secs(s.timing.capture)
    );
    s.instruct(StepId::Capture, &text);
    s.countdown(StepId::Capture, &text)?;
    let clicks_before = s.window.button_clicks().ok();
    let secure_before = s.platform.system.secure_input_active();
    s.discard();
    s.sleep(s.timing.capture)?;
    let mut tally = Tally::default();
    tally.add_all(&s.drain());
    let secure = secure_before || s.platform.system.secure_input_active();
    let clicks = clicks_before
        .zip(s.window.button_clicks().ok())
        .map(|(before, after)| after.saturating_sub(before));
    describe_capture(&tally, &copy, clicks, secure, &mut r.details);
    Ok(judge_capture(&tally, &copy, secure))
}

fn describe_capture(
    t: &Tally,
    copy: &str,
    clicks: Option<u32>,
    secure: bool,
    details: &mut Vec<String>,
) {
    details.push(format!("Mouse movements: {}", t.physical.motion));
    details.push(format!(
        "Mouse button presses and releases: {} (left button: {} down, {} up)",
        t.physical.button, t.left_presses, t.left_releases
    ));
    details.push(format!("Scroll events: {}", t.physical.scroll));
    details.push(format!(
        "Key presses and releases: {} (which keys is never recorded)",
        t.physical.key
    ));
    let modifier = copy.split('+').next().unwrap_or(copy);
    details.push(format!(
        "{copy}: {}",
        match (t.copy_shortcut, t.copy_modifier, t.copy_letter) {
            (true, _, _) => "seen".to_string(),
            (false, true, true) => format!("{modifier} and C were seen, but not together"),
            (false, true, false) => format!("only {modifier} was seen"),
            (false, false, true) => "only C was seen".to_string(),
            (false, false, false) => "not seen".to_string(),
        }
    ));
    if let Some(clicks) = clicks {
        details.push(format!("Clicks the test window's button counted: {clicks}"));
    }
    let injected = t.injected.motion + t.injected.button + t.injected.scroll + t.injected.key;
    if injected > 0 {
        details.push(format!(
            "Events marked as Hop's own (physical = false): {injected}, though Hop injected \
             nothing in this step"
        ));
    }
    if secure {
        details.push(
            "Secure Input was on during this step, so capture could not see the keyboard"
                .to_string(),
        );
    }
}

fn judge_capture(t: &Tally, copy: &str, secure: bool) -> Outcome {
    // What arrived marked as Hop's own instead of physical points at the
    // tag check rather than at a missing event.
    fn missing(what: &str, injected: u32) -> String {
        if injected > 0 {
            format!("{what} (it arrived marked as Hop's own input)")
        } else {
            what.to_string()
        }
    }
    let mut absent = Vec::new();
    if t.physical.motion == 0 {
        absent.push(missing("mouse movement", t.injected.motion));
    }
    if t.left_presses == 0 || t.left_releases == 0 {
        absent.push(missing("a left click", t.injected.button));
    }
    if t.physical.scroll == 0 {
        absent.push(missing("scrolling", t.injected.scroll));
    }
    if !t.copy_shortcut && !secure {
        absent.push(missing(copy, t.injected.key));
    }
    if !absent.is_empty() {
        return Outcome::Fail(format!(
            "Hop did not see {}. If you did that, capture is missing those events",
            join_words(&absent)
        ));
    }
    if !t.copy_shortcut {
        return Outcome::Skipped(format!(
            "the mouse checks passed, but Secure Input was on, so Hop could not see {copy}. \
             Close any password field (or turn off Secure Keyboard Entry in Terminal) and run \
             the self-test again"
        ));
    }
    Outcome::Pass
}

// ----- 3. Typing ------------------------------------------------------------------------

fn typing(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    let select_all = shortcut_name(s.os, 'A');
    let text = "Hop will type into the text box below, then select it all and type over it. \
                Please keep your hands off the keyboard and mouse until this step ends, in a \
                few seconds. For part of that time Hop pauses them on purpose.";
    // The first character goes alone, so that if the keys go to some other
    // app instead of the test window, only that one character does.
    let (first_char, rest) = FIRST_TEXT.split_at_checked(1).unwrap_or((FIRST_TEXT, ""));
    let (Some(head), Some(tail), Some(second_text)) = (
        typing_events(first_char),
        typing_events(rest),
        typing_events(SECOND_TEXT),
    ) else {
        return Ok(Outcome::Fail(
            "internal error: the test text has a character Hop cannot type".to_string(),
        ));
    };
    let mut second = shortcut_events(command_key(s.os), KEY_A);
    second.extend(second_text);

    s.instruct(StepId::Typing, text);
    s.countdown(StepId::Typing, text)?;
    s.window
        .focus_text()
        .doing("putting the keyboard focus in the test window's text box")?;
    s.window
        .clear_text()
        .doing("clearing the test window's text box")?;
    s.sleep(s.timing.focus_pause)?;
    let cleared = wait_for_text(s, "")?;
    if !cleared.is_empty() {
        r.details
            .push("The text box was not empty before typing, though Hop cleared it".to_string());
    }
    s.discard();

    let typed_head = s.inject_all(&head)?;
    let timeout = s.timing.settle_timeout;
    let reached = s.poll(timeout, |s| {
        let text = s
            .window
            .text()
            .doing("reading the test window's text box")?;
        Ok((text != cleared, text != cleared))
    })?;
    if !reached {
        r.details.push(format!(
            "Typed \"{first_char}\" ({typed_head} key events): nothing arrived in the text box"
        ));
        // Select-all and more typing would act on whatever app has the
        // keyboard instead, and could replace a document's contents.
        return Ok(Outcome::Fail(
            "the first key Hop typed did not reach the test window's text box, so Hop stopped \
             typing (the key may have gone to another app). Make sure the test window is in \
             front and run the self-test again"
                .to_string(),
        ));
    }
    let typed_first = typed_head + s.inject_all(&tail)?;
    let got_first = wait_for_text(s, FIRST_TEXT)?;

    // The second part goes in while physical input is suppressed, which
    // checks that suppression lets Hop's own, tagged events through. Hop
    // relies on that when it releases held keys right after suppressing.
    s.suppress()?;
    let typed_second = s.inject_all(&second)?;
    let got_second = wait_for_text(s, SECOND_TEXT)?;
    s.passive()?;

    let typed = typed_first + typed_second;
    let mut tally = Tally::default();
    s.collect_until(&mut tally, timeout, |t| t.injected.key >= typed)?;

    let mut problems = Vec::new();
    let first_result = if got_first == FIRST_TEXT {
        "it arrived".to_string()
    } else {
        let why = describe_text(FIRST_TEXT, &got_first);
        problems.push(format!("typing \"{FIRST_TEXT}\": {why}"));
        why
    };
    r.details.push(format!(
        "Typed \"{FIRST_TEXT}\" ({typed_first} key events): {first_result}"
    ));
    let second_result = if got_second == SECOND_TEXT {
        "it replaced the text".to_string()
    } else {
        let why = describe_second(&got_first, &got_second, &select_all);
        problems.push(format!(
            "{select_all} and \"{SECOND_TEXT}\" with your input suppressed: {why}"
        ));
        why
    };
    r.details.push(format!(
        "{select_all} and \"{SECOND_TEXT}\" with your input suppressed ({typed_second} key \
         events): {second_result}"
    ));
    r.details.push(format!(
        "Capture saw {} of the {typed} key events Hop typed, marked as Hop's own (physical = \
         false)",
        tally.injected.key
    ));
    r.details.push(format!(
        "Physical key events during this step: {} (which keys is never recorded)",
        tally.physical.key
    ));
    if tally.injected.key == 0 && tally.physical.key >= typed {
        problems.push(
            "capture reported Hop's typing as physical keys, so the INJECTED_TAG mark is lost \
             or not checked"
                .to_string(),
        );
    } else if tally.injected.key < typed {
        problems.push(format!(
            "capture saw only {} of the {typed} key events Hop typed",
            tally.injected.key
        ));
    }
    Ok(verdict(problems))
}

/// Polls the text box until it holds `expected` or the settle timeout
/// passes, and returns what it holds then.
fn wait_for_text(s: &mut Session, expected: &str) -> Result<String, StepError> {
    let timeout = s.timing.settle_timeout;
    s.poll(timeout, |s| {
        let text = s
            .window
            .text()
            .doing("reading the test window's text box")?;
        Ok((text == expected, text))
    })
}

/// Why the text box does not hold `expected`, without quoting what it does
/// hold: if the user typed during the step, that is their keystrokes.
pub(crate) fn describe_text(expected: &str, actual: &str) -> String {
    let want = expected.chars().count();
    let have = actual.chars().count();
    if actual.is_empty() {
        "nothing arrived in the text box (was the test window in front?)".to_string()
    } else if actual.eq_ignore_ascii_case(expected) {
        "the letters arrived with the wrong capitals, so Shift is not applied correctly".to_string()
    } else if expected.starts_with(actual) {
        format!("only the first {have} of {want} characters arrived")
    } else {
        let same = expected
            .chars()
            .zip(actual.chars())
            .take_while(|(a, b)| a == b)
            .count();
        format!(
            "the text box holds {have} characters instead of the {want} typed, and only the \
             first {same} match"
        )
    }
}

/// Why select-all and the second text did not leave just the second text.
fn describe_second(before: &str, actual: &str, select_all: &str) -> String {
    let modifier = select_all.split('+').next().unwrap_or(select_all);
    if actual == format!("{before}{SECOND_TEXT}") {
        format!(
            "\"{SECOND_TEXT}\" was added after the old text instead of replacing it, so \
             {select_all} did not select it"
        )
    } else if actual == format!("{before}a{SECOND_TEXT}") {
        format!("{select_all} typed an \"a\" instead of selecting, so {modifier} was not applied")
    } else if actual == before {
        "nothing changed, so suppression swallowed Hop's own keys (or they never arrived)"
            .to_string()
    } else {
        describe_text(SECOND_TEXT, actual)
    }
}

// ----- 4. Clicking ---------------------------------------------------------------------

fn clicking(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    let text = "Hop will move the cursor onto the button below and click it once. Please \
                don't touch the mouse.";
    s.instruct(StepId::Clicking, text);
    s.countdown(StepId::Clicking, text)?;
    // A click lands on whatever is under the cursor, so make sure that is
    // the test window's button before clicking: bring the window to the
    // front, check the button is on screen, and check the cursor got there.
    s.window
        .focus_text()
        .doing("bringing the test window to the front")?;
    s.sleep(s.timing.focus_pause)?;
    let target = s
        .window
        .button_center()
        .doing("finding the test window's button")?;
    r.details.push(format!("Button center: {}", point(target)));
    let displays = s
        .platform
        .displays
        .displays()
        .doing("reading the list of displays")?;
    if !displays.iter().any(|d| d.frame.contains(target)) {
        return Ok(Outcome::Fail(format!(
            "the test window's button is off screen at {}, so Hop did not click",
            point(target)
        )));
    }
    let before = s
        .window
        .button_clicks()
        .doing("reading the button's click count")?;
    s.discard();
    s.check()?;
    s.platform
        .injector
        .move_cursor_to(target)
        .doing("moving the cursor")?;
    let timeout = s.timing.settle_timeout;
    let arrived = s.poll(timeout, |s| {
        let here = s
            .platform
            .cursor
            .position()
            .doing("reading the cursor position")?;
        Ok((distance(here, target) <= POSITION_TOLERANCE, here))
    })?;
    let off = distance(arrived, target);
    r.details.push(format!(
        "Cursor after the move: {}, {} units from the center",
        point(arrived),
        num(off)
    ));
    if off > POSITION_TOLERANCE {
        return Ok(Outcome::Fail(format!(
            "the cursor went to {} instead of the button at {}, so Hop did not click",
            point(arrived),
            point(target)
        )));
    }
    // Let the window see the cursor arrive before the click.
    s.sleep(s.timing.click_pause)?;
    let button = |pressed| InputEvent::Button {
        button: MouseButton::Left,
        pressed,
    };
    s.inject_all(&[button(true)])?;
    s.sleep(s.timing.click_pause)?;
    s.inject_all(&[button(false)])?;

    let mut after = s.poll(timeout, |s| {
        let clicks = s
            .window
            .button_clicks()
            .doing("reading the button's click count")?;
        Ok((clicks > before, clicks))
    })?;
    if after > before {
        // Look again a little later, in case one click is counted twice.
        s.sleep(s.timing.click_pause * 3)?;
        after = s
            .window
            .button_clicks()
            .doing("reading the button's click count")?;
    }
    let position = s
        .platform
        .cursor
        .position()
        .doing("reading the cursor position")?;
    let mut tally = Tally::default();
    s.collect_until(&mut tally, timeout, |t| t.injected.button >= 2)?;

    let clicks = after.saturating_sub(before);
    let off = distance(position, target);
    r.details.push(format!(
        "Cursor after the click: {}, {} units from the center",
        point(position),
        num(off)
    ));
    r.details
        .push(format!("Clicks the button counted: {clicks}"));
    r.details.push(format!(
        "Capture saw {} of Hop's 2 button events and {} of its moves, marked as Hop's own \
         (physical = false)",
        tally.injected.button, tally.injected.motion
    ));
    r.details.push(format!(
        "Physical mouse events during this step: {}",
        tally.physical.motion + tally.physical.button + tally.physical.scroll
    ));

    let mut problems = Vec::new();
    match clicks {
        1 => {}
        0 => problems.push("the button counted no click".to_string()),
        n => problems.push(format!("the button counted {n} clicks instead of 1")),
    }
    if off > POSITION_TOLERANCE {
        problems.push(format!(
            "the cursor ended up {} units from the button's center",
            num(off)
        ));
    }
    if tally.injected.button == 0 && tally.physical.button >= 2 {
        problems.push(
            "capture reported Hop's click as a physical click, so the INJECTED_TAG mark is lost \
             or not checked"
                .to_string(),
        );
    }
    Ok(verdict(problems))
}

// ----- 5. Holding the mouse still (suppression) -----------------------------------------

/// Holds the mouse the way Hop does while controlling the other computer:
/// suppressed and parked together, as the core always does. Then lets the
/// lease run out and checks that the backend gives both the input and the
/// cursor back by itself, which is the fail-open guarantee.
fn suppression(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    let text = format!(
        "For the next {}, Hop holds your mouse the way it does when you control the other \
         computer: the cursor disappears and stays put. Move the mouse anyway. Then stop: Hop \
         lets go by itself and asks you to move the mouse again.",
        secs(s.timing.suppress)
    );
    s.instruct(StepId::Suppression, &text);
    s.countdown(StepId::Suppression, &text)?;
    s.suppress()?;
    let original = s
        .platform
        .cursor
        .position()
        .doing("reading the cursor position")?;
    s.park(original)?;
    // Let motion that was already on its way land before taking the
    // starting point.
    s.sleep(s.timing.renew_interval.min(s.timing.suppress / 4))?;
    let start = s
        .platform
        .cursor
        .position()
        .doing("reading the cursor position")?;
    s.discard();
    let drift = largest_drift(s, start, s.timing.suppress)?;
    let mut tally = Tally::default();
    tally.add_all(&s.drain());

    // Now stop renewing and see whether the backend gives input back by
    // itself, as the lease requires.
    s.stop_renewing();
    let lapse_check = s.last_renew + s.timing.lapse_wait;
    s.update(
        StepId::Suppression,
        "Stop moving the mouse. Hop is checking that the mouse comes back by itself.",
    );
    s.sleep_until(lapse_check)?;
    let mode = s.platform.capture.mode();

    // The lapse must also have freed the cursor: it moves again.
    s.update(
        StepId::Suppression,
        "Now move the mouse again: the cursor should be back and moving.",
    );
    let here = s
        .platform
        .cursor
        .position()
        .doing("reading the cursor position")?;
    let freed = largest_drift(s, here, s.timing.park)?;
    s.passive()?;
    s.unpark()?;

    r.details.push(format!(
        "Mouse movements seen while suppressing: {}",
        tally.physical.motion
    ));
    r.details.push(format!(
        "Largest cursor movement: {} units (it must stay under {})",
        num(drift),
        num(STILL_TOLERANCE)
    ));
    let lapse = secs(s.timing.lapse_wait);
    r.details.push(format!(
        "{lapse} after the last renewal, capture was {}",
        match mode {
            CaptureMode::Passive => "passive again (the lease ran out by itself)",
            CaptureMode::Suppress => "still suppressing",
        }
    ));
    r.details.push(format!(
        "After the lease ran out, the cursor moved {} units when you moved the mouse (it must \
         move at least {})",
        num(freed),
        num(STILL_TOLERANCE)
    ));

    let mut problems = Vec::new();
    if tally.physical.motion == 0 {
        let marked = if tally.injected.motion > 0 {
            format!(
                " ({} movements arrived marked as Hop's own input)",
                tally.injected.motion
            )
        } else {
            String::new()
        };
        problems.push(format!(
            "no mouse movement was seen while input was suppressed{marked}. Move the mouse \
             during this step; if you did, capture stops reporting motion while it suppresses, \
             which would break controlling the other computer"
        ));
    }
    if drift >= STILL_TOLERANCE {
        problems.push(format!(
            "the cursor moved {} units while it should have stayed still, so suppression does \
             not hold the cursor",
            num(drift)
        ));
    }
    if mode != CaptureMode::Passive {
        problems.push(format!(
            "capture was still suppressing {lapse} after Hop stopped renewing, so the \
             fail-open lease did not run out"
        ));
    }
    if freed < STILL_TOLERANCE {
        problems.push(
            "the cursor stayed pinned after the lease ran out, so the backend did not give the \
             cursor back by itself (or the mouse was not moved when asked)"
                .to_string(),
        );
    }
    Ok(verdict(problems))
}

/// Samples the cursor for `duration` and returns how far it got from `from`.
fn largest_drift(s: &mut Session, from: Point, duration: Duration) -> Result<f64, StepError> {
    let end = Instant::now() + duration;
    let mut drift = 0.0_f64;
    while Instant::now() < end {
        s.sleep_until((Instant::now() + s.timing.poll_interval).min(end))?;
        let here = s
            .platform
            .cursor
            .position()
            .doing("reading the cursor position")?;
        drift = drift.max(distance(here, from));
    }
    Ok(drift)
}

// ----- 6. Hiding the cursor (park) -------------------------------------------------------

fn park(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    let text = format!(
        "The cursor will disappear for {} and then come back where it was. Move the mouse a \
         little while it is hidden.",
        secs(s.timing.park)
    );
    s.instruct(StepId::Park, &text);
    s.countdown(StepId::Park, &text)?;
    let original = s
        .platform
        .cursor
        .position()
        .doing("reading the cursor position")?;
    s.park(original)?;
    let pinned = s
        .platform
        .cursor
        .position()
        .doing("reading the hidden cursor's position")?;
    s.discard();
    let drift = largest_drift(s, pinned, s.timing.park)?;
    let mut tally = Tally::default();
    tally.add_all(&s.drain());
    s.unpark()?;
    let timeout = s.timing.settle_timeout;
    let back = s.poll(timeout, |s| {
        let here = s
            .platform
            .cursor
            .position()
            .doing("reading the cursor position")?;
        Ok((distance(here, original) <= POSITION_TOLERANCE, here))
    })?;
    let off = distance(back, original);

    r.note = Some("confirm you saw it disappear".to_string());
    r.details.push(format!(
        "Cursor before: {}; after: {}, {} units away",
        point(original),
        point(back),
        num(off)
    ));
    r.details.push(format!(
        "While hidden, the cursor moved {} units (parking pins it, so it should not move)",
        num(drift)
    ));
    r.details.push(format!(
        "Mouse movements seen while hidden: {}{}",
        tally.physical.motion,
        if tally.physical.motion == 0 {
            " (move the mouse during this step to check that motion keeps arriving while the \
             cursor is parked)"
        } else {
            ""
        }
    ));
    r.details.push(
        "Visibility: Hop cannot see the cursor itself. Confirm you saw it disappear and come \
         back"
            .to_string(),
    );

    let mut problems = Vec::new();
    if off > POSITION_TOLERANCE {
        problems.push(format!(
            "the cursor came back {} units from where it was",
            num(off)
        ));
    }
    if drift >= STILL_TOLERANCE {
        problems.push(format!(
            "the hidden cursor moved {} units, so parking does not pin it",
            num(drift)
        ));
    }
    Ok(verdict(problems))
}

// ----- 7. Edge push -----------------------------------------------------------------------

fn edge_push(s: &mut Session, r: &mut StepReport) -> Result<Outcome, StepError> {
    let displays = s
        .platform
        .displays
        .displays()
        .doing("reading the list of displays")?;
    let Some(bounds) = displays.iter().map(|d| d.frame).reduce(|a, b| a.union(&b)) else {
        return Ok(Outcome::Fail(
            "the platform reported no displays".to_string(),
        ));
    };
    let me = DeviceId::new("this-computer");
    let peer = DeviceId::new("pretend-peer");
    // A pretend screen as tall as all of this computer's screens together,
    // so it faces the whole right edge.
    let peer_display = DisplayInfo {
        id: 1,
        frame: Rect::new(0.0, 0.0, 1920.0, bounds.size.height.max(1080.0)),
        scale: 1.0,
        primary: true,
    };
    let layout = match Layout::two_devices(
        me.clone(),
        displays,
        peer.clone(),
        Edge::Right,
        vec![peer_display.clone()],
    ) {
        Ok(layout) => layout,
        Err(error) => {
            return Ok(Outcome::Fail(format!(
                "Hop's core cannot use this computer's displays: {error}"
            )));
        }
    };
    let mut core = Core::new(me, s.os, CoreConfig::default(), layout);
    // Only the core's answers to local input matter below; nothing it says
    // is ever carried out.
    let _ = core.handle(
        0,
        Event::PeerConnected {
            peer,
            os: other_os(s.os),
            displays: vec![peer_display],
        },
    );

    let text = format!(
        "Push the cursor against the right edge of your right-most screen and keep pushing \
         for a moment, away from the corners and without holding a mouse button. Nothing goes \
         to another computer: this only checks that Hop notices the push. You have up to {}.",
        secs(s.timing.edge_push)
    );
    s.instruct(StepId::EdgePush, &text);
    s.countdown(StepId::EdgePush, &text)?;
    s.discard();
    let started = Instant::now();
    let end = started + s.timing.edge_push;
    let right = bounds.max_x();
    let mut watch = EdgeWatch::default();
    loop {
        for observation in s.drain() {
            if !observation.physical {
                continue;
            }
            let event = match observation.kind {
                Observed::Motion { dx, dy } => {
                    watch.motion(observation.cursor, dx, right);
                    InputEvent::PointerMotion { dx, dy }
                }
                // Buttons matter: the core never crosses while one is held.
                Observed::Button { button, pressed } => InputEvent::Button { button, pressed },
                // Keys and scrolling play no part in crossing an edge, and
                // the self-test never hands keys to anyone.
                Observed::Scroll | Observed::Key { .. } => continue,
            };
            let at = observation.at.saturating_duration_since(started);
            let actions = core.handle(
                millis(at),
                Event::LocalInput {
                    event,
                    cursor: observation.cursor,
                    physical: true,
                },
            );
            // The actions are only inspected. Carrying them out would
            // suppress input and hide the cursor for a peer that does not
            // exist.
            let crossed = actions.iter().any(|action| {
                matches!(
                    action,
                    Action::Send {
                        msg: CoreMessage::Enter { .. },
                        ..
                    }
                )
            });
            if crossed {
                r.details.push(format!(
                    "Hop's core started a crossing {} into the step, with the cursor at {}",
                    secs(at),
                    point(observation.cursor)
                ));
                r.details.extend(watch.details(right));
                return Ok(Outcome::Pass);
            }
        }
        if Instant::now() >= end {
            break;
        }
        s.sleep(s.timing.poll_interval)?;
    }
    r.details.extend(watch.details(right));
    Ok(Outcome::Fail(watch.diagnose(right)))
}

/// What the edge-push step saw of the cursor's way to the right edge.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct EdgeWatch {
    motions: u32,
    /// The largest x the cursor was at.
    max_x: Option<f64>,
    /// Motions that tried to go past the right edge.
    pushes: u32,
}

impl EdgeWatch {
    fn motion(&mut self, cursor: Point, dx: f64, right: f64) {
        self.motions = self.motions.saturating_add(1);
        self.max_x = Some(self.max_x.map_or(cursor.x, |x| x.max(cursor.x)));
        if dx > 0.0 && cursor.x + dx >= right {
            self.pushes = self.pushes.saturating_add(1);
        }
    }

    /// How far the cursor stayed from the last column of the right edge.
    fn gap(&self, right: f64) -> Option<f64> {
        self.max_x.map(|x| (right - 1.0 - x).max(0.0))
    }

    fn details(&self, right: f64) -> Vec<String> {
        let mut details = vec![format!("Mouse movements seen: {}", self.motions)];
        if let Some(gap) = self.gap(right) {
            details.push(format!(
                "Closest the cursor came to the right edge: {} units",
                num(gap)
            ));
        }
        details.push(format!(
            "Movements that pushed past the right edge: {}",
            self.pushes
        ));
        details
    }

    fn diagnose(&self, right: f64) -> String {
        match self.gap(right) {
            None => "no mouse movement was seen. Push the cursor against the right edge while \
                     this step runs"
                .to_string(),
            Some(gap) if gap > POSITION_TOLERANCE => format!(
                "the cursor never reached the right edge of the right-most screen (it came \
                 within {} units)",
                num(gap)
            ),
            Some(_) if self.pushes == 0 => "the cursor reached the right edge, but capture \
                 reported no motion past it. At a screen edge, capture must report the motion \
                 the user attempted (macOS: the event's delta fields; Windows: Raw Input \
                 deltas), or Hop cannot tell a push from standing still"
                .to_string(),
            Some(_) => format!(
                "Hop saw {} pushes past the right edge, but none started a crossing. Were they \
                 within {} units of a corner, or was a mouse button held down?",
                self.pushes,
                num(CoreConfig::DEFAULT_CORNER_DEAD_ZONE)
            ),
        }
    }
}

fn other_os(os: Os) -> Os {
    match os {
        Os::MacOs => Os::Windows,
        Os::Windows | Os::Linux => Os::MacOs,
    }
}

// ----- helpers ---------------------------------------------------------------------------

/// Pass, or a failure listing every problem.
fn verdict(problems: Vec<String>) -> Outcome {
    if problems.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail(problems.join("; "))
    }
}

/// "a", "a and b", "a, b and c".
fn join_words(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A number with at most two decimals and no trailing zeros: "2", "1.25".
pub(crate) fn num(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".to_string()
    } else {
        text.to_string()
    }
}

fn point(p: Point) -> String {
    format!("({}, {})", num(p.x), num(p.y))
}

/// A duration in words: "1 second", "8 seconds", "0.3 seconds".
pub(crate) fn secs(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        match duration.as_secs() {
            1 => "1 second".to_string(),
            n => format!("{n} seconds"),
        }
    } else {
        format!("{} seconds", num(duration.as_secs_f64()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::Counts;

    #[test]
    fn numbers_and_durations_read_naturally() {
        assert_eq!(num(2.0), "2");
        assert_eq!(num(1.25), "1.25");
        assert_eq!(num(700.5), "700.5");
        assert_eq!(num(-0.001), "0");
        assert_eq!(num(1511.0), "1511");
        assert_eq!(secs(Duration::from_secs(1)), "1 second");
        assert_eq!(secs(Duration::from_secs(8)), "8 seconds");
        assert_eq!(secs(Duration::from_millis(1500)), "1.5 seconds");
        assert_eq!(secs(Duration::from_millis(250)), "0.25 seconds");
        assert_eq!(join_words(&["a".into()]), "a");
        assert_eq!(
            join_words(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
    }

    #[test]
    fn text_mismatches_are_described_without_quoting_the_text_box() {
        assert_eq!(
            describe_text(FIRST_TEXT, ""),
            "nothing arrived in the text box (was the test window in front?)"
        );
        assert_eq!(
            describe_text(FIRST_TEXT, "hop test 123"),
            "the letters arrived with the wrong capitals, so Shift is not applied correctly"
        );
        assert_eq!(
            describe_text(FIRST_TEXT, "Hop t"),
            "only the first 5 of 12 characters arrived"
        );
        let private = "Hop tesSECRET";
        let described = describe_text(FIRST_TEXT, private);
        assert_eq!(
            described,
            "the text box holds 13 characters instead of the 12 typed, and only the first 7 \
             match"
        );
        assert!(!described.contains("SECRET"));

        assert!(describe_second(FIRST_TEXT, "Hop test 123ok", "Cmd+A").contains("did not select"));
        assert!(
            describe_second(FIRST_TEXT, "Hop test 123aok", "Cmd+A").contains("Cmd was not applied")
        );
        assert!(describe_second(FIRST_TEXT, FIRST_TEXT, "Ctrl+A").contains("swallowed"));
        assert_eq!(
            describe_second(FIRST_TEXT, "o", "Cmd+A"),
            "only the first 1 of 2 characters arrived"
        );
    }

    fn tally(motion: u32, button: u32, scroll: u32, copy: bool) -> Tally {
        Tally {
            physical: Counts {
                motion,
                button: button * 2,
                scroll,
                key: if copy { 4 } else { 0 },
            },
            left_presses: button,
            left_releases: button,
            copy_modifier: copy,
            copy_letter: copy,
            copy_shortcut: copy,
            ..Tally::default()
        }
    }

    #[test]
    fn capture_passes_only_with_everything_it_asked_for() {
        assert_eq!(
            judge_capture(&tally(10, 1, 3, true), "Cmd+C", false),
            Outcome::Pass
        );
        assert_eq!(
            judge_capture(&tally(10, 0, 0, true), "Cmd+C", false),
            Outcome::Fail(
                "Hop did not see a left click and scrolling. If you did that, capture is \
                 missing those events"
                    .to_string()
            )
        );
        let Outcome::Fail(reason) = judge_capture(&tally(10, 1, 3, false), "Ctrl+C", false) else {
            panic!("a missing shortcut must fail");
        };
        assert!(reason.contains("Ctrl+C"));
    }

    #[test]
    fn capture_points_at_the_tag_when_physical_input_looks_injected() {
        let mut t = tally(0, 1, 3, true);
        t.injected.motion = 25;
        let Outcome::Fail(reason) = judge_capture(&t, "Cmd+C", false) else {
            panic!("missing motion must fail");
        };
        assert!(reason.contains("mouse movement (it arrived marked as Hop's own input)"));
    }

    #[test]
    fn secure_input_turns_a_missing_shortcut_into_a_skip() {
        assert!(matches!(
            judge_capture(&tally(10, 1, 3, false), "Cmd+C", true),
            Outcome::Skipped(_)
        ));
        // The mouse must still have worked.
        assert!(matches!(
            judge_capture(&tally(0, 1, 3, false), "Cmd+C", true),
            Outcome::Fail(_)
        ));
        // And a shortcut that got through anyway counts.
        assert_eq!(
            judge_capture(&tally(10, 1, 3, true), "Cmd+C", true),
            Outcome::Pass
        );
    }

    #[test]
    fn capture_details_count_keys_without_naming_them() {
        let mut details = Vec::new();
        describe_capture(
            &tally(10, 1, 3, true),
            "Cmd+C",
            Some(1),
            false,
            &mut details,
        );
        assert_eq!(
            details,
            [
                "Mouse movements: 10",
                "Mouse button presses and releases: 2 (left button: 1 down, 1 up)",
                "Scroll events: 3",
                "Key presses and releases: 4 (which keys is never recorded)",
                "Cmd+C: seen",
                "Clicks the test window's button counted: 1",
            ]
        );
        let mut details = Vec::new();
        let mut t = tally(10, 1, 3, false);
        t.copy_modifier = true;
        describe_capture(&t, "Cmd+C", None, true, &mut details);
        assert!(details.contains(&"Cmd+C: only Cmd was seen".to_string()));
        assert!(details.iter().any(|d| d.starts_with("Secure Input was on")));
    }

    #[test]
    fn display_checks_follow_the_coordinate_contract() {
        let display = |id, x, y, primary| DisplayInfo {
            id,
            frame: Rect::new(x, y, 1512.0, 982.0),
            scale: 2.0,
            primary,
        };
        assert_eq!(check_displays(&[display(1, 0.0, 0.0, true)]), Ok(()));
        assert_eq!(
            check_displays(&[display(1, 0.0, 0.0, true), display(2, 1512.0, 0.0, false)]),
            Ok(())
        );
        assert!(check_displays(&[]).is_err());
        assert!(check_displays(&[display(1, 0.0, 0.0, false)]).is_err());
        assert!(check_displays(&[display(1, 0.0, 0.0, true), display(2, 9.0, 0.0, true)]).is_err());
        assert!(
            check_displays(&[display(1, 100.0, 0.0, true)])
                .unwrap_err()
                .contains("(100, 0)")
        );
        let mut broken = display(1, 0.0, 0.0, true);
        broken.scale = 0.0;
        assert!(
            check_displays(&[broken])
                .unwrap_err()
                .contains("Hop's core cannot use these displays")
        );
        assert_eq!(
            describe_display(&display(1, 0.0, 0.0, true)),
            "Display 1: 1512 x 982 at (0, 0), scale 2, primary"
        );
    }

    #[test]
    fn the_edge_diagnosis_says_what_went_wrong() {
        let right = 1512.0;
        let watch = EdgeWatch::default();
        assert!(watch.diagnose(right).starts_with("no mouse movement"));

        let mut watch = EdgeWatch::default();
        watch.motion(Point::new(900.0, 400.0), 5.0, right);
        assert!(
            watch
                .diagnose(right)
                .contains("never reached the right edge")
        );
        assert!(watch.diagnose(right).contains("within 611 units"));

        let mut watch = EdgeWatch::default();
        watch.motion(Point::new(1511.0, 400.0), 0.0, right);
        assert!(watch.diagnose(right).contains("attempted"));

        let mut watch = EdgeWatch::default();
        watch.motion(Point::new(1511.0, 400.0), 4.0, right);
        assert!(watch.diagnose(right).contains("corner"));
        assert_eq!(
            watch.details(right),
            [
                "Mouse movements seen: 1",
                "Closest the cursor came to the right edge: 0 units",
                "Movements that pushed past the right edge: 1",
            ]
        );
    }
}
