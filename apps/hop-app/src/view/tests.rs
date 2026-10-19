//! The window drawn headless, with no OS window, no renderer and no
//! network: egui runs the view against made-up state for each major state
//! the window can be in, and the tests read back the text it drew. Clicks
//! are made-up pointer events too; nothing reaches the real mouse.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Shape, epaint::ClippedShape, vec2,
};
use hop_agent::pairing::{PairEvent, Replaced, Saved};
use hop_agent::service::{AgentUpdate, Session};
use hop_agent::settings::{Overview, PeerView};
use hop_agent::{CmdCtrl, DeviceId, Os, PermissionState, Side, Zeroizing};

use super::{Action, show};
use crate::model::{Model, Settings, words};

const KEYS: &str = "If the keyboard and mouse ever seem stuck on the other computer, press \
                    Control + Option + Shift + Escape (the left-hand keys) on this one to bring \
                    them back at once.";

fn pc() -> PeerView {
    PeerView {
        id: DeviceId::new("a".repeat(32)),
        name: "Windows PC".to_owned(),
        os: Os::Windows,
        side: Side::Right,
        address: Some("192.168.1.20".to_owned()),
        paired: true,
    }
}

fn model_with(os: Os, peers: Vec<PeerView>) -> Model {
    let overview = Overview {
        name: "Mac".to_owned(),
        os,
        config_path: PathBuf::from("config.toml"),
        peers,
        cmd_ctrl: CmdCtrl::Keep,
        notes: Vec::new(),
    };
    Model::new(
        os,
        Settings::Loaded(overview),
        Some(PermissionState::Granted),
    )
}

fn paired() -> Model {
    model_with(Os::MacOs, vec![pc()])
}

/// The PC's window, paired with the Mac on its left.
fn paired_pc() -> Model {
    let mac = PeerView {
        id: DeviceId::new("c".repeat(32)),
        name: "Mac".to_owned(),
        os: Os::MacOs,
        side: Side::Left,
        address: None,
        paired: true,
    };
    let mut model = model_with(Os::Windows, vec![mac]);
    model.permission = None;
    model
}

/// `model` with the Shortcuts setting at `cmd_ctrl`.
fn with_shortcuts(mut model: Model, cmd_ctrl: CmdCtrl) -> Model {
    if let Settings::Loaded(overview) = &mut model.settings {
        overview.cmd_ctrl = cmd_ctrl;
    }
    model
}

/// A running window that just heard of files from the PC.
fn received_files() -> Model {
    let mut model = running(true, Session::Idle);
    model.agent_update(AgentUpdate::FilesReceived {
        from: "Windows PC".to_owned(),
        names: vec!["report.pdf".to_owned(), "photo (2).jpg".to_owned()],
    });
    model
}

fn running(connected: bool, session: Session) -> Model {
    let mut model = paired();
    model.sharing.start();
    model.sharing.apply(AgentUpdate::Started {
        peer: "Windows PC".to_owned(),
        side: Side::Right,
        emergency_keys: KEYS.to_owned(),
    });
    if connected {
        model.sharing.apply(AgentUpdate::Connected {
            name: "Windows PC".to_owned(),
            os: Os::Windows,
        });
    }
    model.sharing.apply(AgentUpdate::Session(session));
    model
}

fn showing(now: Instant) -> Model {
    let mut model = paired();
    model.pairing.begin_show();
    model.pairing.apply(
        PairEvent::Code(Zeroizing::new("482 913".to_owned())),
        now,
        &mut model.activity,
    );
    model.pairing.apply(
        PairEvent::WrongCode { failures: 1 },
        now,
        &mut model.activity,
    );
    model
}

/// The window's size at the start.
const WINDOW: [f32; 2] = [480.0, 680.0];

/// A window tall enough to show every section at once, so the text of all
/// of them can be read back (the real window scrolls).
const TALL: [f32; 2] = [480.0, 2400.0];

/// Draws `model` for a few frames and returns the text on screen, plus
/// anything the view asked for.
fn draw(model: &mut Model, now: Instant) -> (String, Vec<Action>) {
    draw_in(model, now, TALL)
}

fn draw_in(model: &mut Model, now: Instant, size: [f32; 2]) -> (String, Vec<Action>) {
    let ctx = egui::Context::default();
    let mut actions = Vec::new();
    let mut text = String::new();
    // egui sizes some things on the first frame and draws them on the
    // next; a third checks that the state holds still.
    for frame in 0..3 {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(size[0], size[1]))),
            time: Some(f64::from(frame) / 60.0),
            ..RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| show(ui, model, now, &mut actions));
        text.clear();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        output.drop_without_applying_deltas();
    }
    // egui marks two widgets that share an id on screen (in debug builds),
    // which would make one of them react to the other's clicks.
    for clash in ["Double use of", "First use of", "Second use of"] {
        assert!(!text.contains(clash), "an id is used twice:\n{text}");
    }
    (text, actions)
}

fn collect_text(shape: &Shape, out: &mut String) {
    match shape {
        Shape::Text(text) => {
            out.push_str(text.galley.text());
            out.push('\n');
        }
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, out);
            }
        }
        _ => {}
    }
}

/// Draws `model` and checks that each of `expected` is on screen and that
/// nothing was clicked.
fn shows(model: &mut Model, expected: &[&str]) -> String {
    let (text, actions) = draw(model, Instant::now());
    for wanted in expected {
        assert!(
            text.contains(wanted),
            "{wanted:?} is not on screen:\n{text}"
        );
    }
    assert_eq!(actions, [], "nothing was clicked");
    text
}

#[test]
fn not_paired() {
    let mut model = model_with(Os::MacOs, Vec::new());
    shows(
        &mut model,
        &[
            "Hop",
            "This computer: Mac (macOS)",
            "Not sharing.",
            "Start sharing",
            "Pair with your other computer first.",
            "Not paired yet. Use Pair below.",
            "Show a pairing code",
            "Enter a code",
            "Accessibility: Allowed",
            "Open log folder",
        ],
    );
}

#[test]
fn paired_and_stopped() {
    let text = shows(
        &mut paired(),
        &[
            "Not sharing.",
            "Windows PC (Windows)",
            "Where is it?",
            "Left",
            "Right",
            "Above",
            "Below",
            "On Windows PC, choose Left for this computer.",
            "Address",
            "only if it is not found automatically",
            "Save address",
            "192.168.1.20",
            "Forget…",
        ],
    );
    assert!(
        !text.contains("Pair with your other computer first."),
        "{text}"
    );
}

#[test]
fn two_paired_computers_each_get_their_own_controls() {
    let laptop = PeerView {
        id: DeviceId::new("b".repeat(32)),
        name: "Laptop".to_owned(),
        os: Os::Windows,
        side: Side::Left,
        address: None,
        paired: true,
    };
    let mut model = model_with(Os::MacOs, vec![pc(), laptop]);
    let text = shows(
        &mut model,
        &[
            "Windows PC (Windows)",
            "Laptop (Windows)",
            "On Laptop, choose Right for this computer.",
        ],
    );
    assert_eq!(text.matches("Save address").count(), 2, "{text}");
}

#[test]
fn stopped_with_an_error_and_stopping() {
    let mut model = paired();
    model.sharing.start();
    model.sharing.apply(AgentUpdate::Stopped {
        error: Some("UDP port 47800 is already in use".to_owned()),
    });
    shows(&mut model, &["Stopped: UDP port 47800 is already in use."]);

    let mut model = running(true, Session::Idle);
    model.sharing.stop();
    shows(&mut model, &["Stopping…", "Stop sharing"]);
}

#[test]
fn running_and_connected() {
    shows(
        &mut running(false, Session::Idle),
        &["Looking for Windows PC…", "Stop sharing"],
    );
    shows(
        &mut running(true, Session::Idle),
        &[
            "Connected to Windows PC.",
            "Push the cursor past the right edge of this screen to move to Windows PC.",
            "Control + Option + Shift + Escape",
        ],
    );
}

#[test]
fn controlling_and_controlled() {
    shows(
        &mut running(true, Session::Controlling),
        &["Using Windows PC: this keyboard and mouse control it now."],
    );
    shows(
        &mut running(true, Session::Controlled),
        &["Windows PC is using this computer."],
    );
}

#[test]
fn showing_a_code() {
    let now = Instant::now();
    let mut model = showing(now);
    let (text, actions) = draw(&mut model, now);
    for wanted in [
        "482 913",
        "On the other computer, open Hop, click Enter a code and type this code.",
        "Time left: 2:00",
        "Someone entered a wrong code (1 of 3 tries used).",
        "Cancel",
    ] {
        assert!(
            text.contains(wanted),
            "{wanted:?} is not on screen:\n{text}"
        );
    }
    assert_eq!(actions, []);
    // The code is on screen once, large, and never in Activity.
    assert_eq!(text.matches("482 913").count(), 1, "{text}");
}

#[test]
fn the_pairing_question() {
    let now = Instant::now();
    let mut model = showing(now);
    model.pairing.apply(
        PairEvent::Confirm {
            name: "Mac".to_owned(),
            os: Os::MacOs,
            replaces: Some(
                "This replaces the pairing with Old Mac, whose old key will stop working."
                    .to_owned(),
            ),
            within: Duration::from_secs(90),
        },
        now,
        &mut model.activity,
    );
    let (text, actions) = draw(&mut model, now);
    for wanted in [
        "Mac (macOS) entered the code. Pair with it?",
        "This replaces the pairing with Old Mac",
        "Answer Yes only if Mac says it is waiting for you.",
        "1:30 left",
        "Yes, pair",
        "No",
    ] {
        assert!(
            text.contains(wanted),
            "{wanted:?} is not on screen:\n{text}"
        );
    }
    assert_eq!(actions, []);
}

#[test]
fn the_pairing_question_once_its_time_is_up() {
    let now = Instant::now();
    let mut model = showing(now);
    model.pairing.apply(
        PairEvent::Confirm {
            name: "Mac".to_owned(),
            os: Os::MacOs,
            replaces: None,
            within: Duration::from_secs(90),
        },
        now,
        &mut model.activity,
    );
    let (text, actions) = draw(&mut model, now + Duration::from_secs(90));
    for wanted in [
        "Mac (macOS) entered the code. Pair with it?",
        "No answer in time.",
    ] {
        assert!(
            text.contains(wanted),
            "{wanted:?} is not on screen:\n{text}"
        );
    }
    assert!(
        !text.contains("Yes, pair") && !text.contains("left)"),
        "no buttons and no countdown once the time is up:\n{text}"
    );
    assert_eq!(actions, []);
}

#[test]
fn joining_in_progress() {
    let now = Instant::now();
    let mut model = paired();
    model.pairing.open_join();
    shows(
        &mut model,
        &[
            "Code",
            "If the other computer is not found",
            "Address",
            "Pair",
            "Cancel",
        ],
    );

    model.pairing.code.push_str("482 913");
    model.pairing.begin_join();
    model
        .pairing
        .apply(PairEvent::Searching, now, &mut model.activity);
    shows(&mut model, &["Looking for the computer showing the code…"]);
    model.pairing.apply(
        PairEvent::WaitingForConfirmation {
            host: "Windows PC".to_owned(),
        },
        now,
        &mut model.activity,
    );
    shows(&mut model, &["Now click Yes, pair on Windows PC."]);

    let mut model = paired();
    model.pairing.open_join();
    model.pairing.code.push_str("482913");
    model.pairing.begin_join();
    model.pairing.apply(
        PairEvent::SeveralHosts(vec![
            ("PC".to_owned(), vec!["192.168.1.20".parse().expect("ip")]),
            (
                "Laptop".to_owned(),
                vec!["192.168.1.21".parse().expect("ip")],
            ),
        ]),
        now,
        &mut model.activity,
    );
    shows(
        &mut model,
        &["More than one computer is showing a pairing code"],
    );
}

#[test]
fn finished_ok_and_failed() {
    let now = Instant::now();
    let mut model = paired();
    model.pairing.begin_show();
    model.pairing.apply(
        PairEvent::Finished(Ok(Saved {
            name: "Windows PC".to_owned(),
            os: Os::Windows,
            side: Side::Right,
            replaced: Replaced::Nothing,
            path: PathBuf::from("config.toml"),
        })),
        now,
        &mut model.activity,
    );
    shows(
        &mut model,
        &[
            "Paired with Windows PC. It sits to the right of this screen; change that under Your \
           other computer.",
        ],
    );

    let mut model = paired();
    model.pairing.open_join();
    model.pairing.code.push_str("482 913");
    model.pairing.begin_join();
    model
        .pairing
        .apply(PairEvent::NoHostFound, now, &mut model.activity);
    model.pairing.apply(
        PairEvent::Finished(Err(
            "could not find a computer showing a pairing code".to_owned()
        )),
        now,
        &mut model.activity,
    );
    shows(
        &mut model,
        &[
            "Not paired: could not find a computer showing a pairing code.",
            "No computer showing a pairing code was found.",
        ],
    );
}

#[test]
fn settings_that_cannot_be_read() {
    let mut model = Model::new(
        Os::MacOs,
        Settings::Failed("could not load config.toml: expected `=`".to_owned()),
        Some(PermissionState::Granted),
    );
    let text = shows(
        &mut model,
        &[
            "Hop could not read its settings: could not load config.toml: expected `=`.",
            "Try again",
            "Hop cannot share until it can read its settings (see above).",
            "Shortcuts",
            "Hop cannot show this choice until it can read its settings (see above).",
        ],
    );
    assert!(!text.contains(words::MAC_KEEP_KEYS), "{text}");
}

#[test]
fn the_shortcuts_are_worded_for_the_other_computers_keyboard() {
    // The Mac applies the choice to keys from the PC's keyboard.
    let text = shows(
        &mut paired(),
        &[
            "Shortcuts",
            "With Windows PC's keyboard on this computer:",
            "Keep keys as they are: the Windows key is Cmd",
            "Swap Ctrl and Cmd: Ctrl+C copies, as on a PC",
            "The choice for this computer's keyboard is in Hop on Windows PC.",
        ],
    );
    assert!(!text.contains(words::PC_KEEP_KEYS), "{text}");

    // The PC applies it to keys from the Mac's keyboard.
    let text = shows(
        &mut paired_pc(),
        &[
            "With Mac's keyboard on this computer:",
            "Keep keys as they are: Ctrl+C copies, Cmd is the Windows key",
            "Swap Cmd and Ctrl: Cmd+C copies, as on a Mac",
            "The choice for this computer's keyboard is in Hop on Mac.",
        ],
    );
    assert!(!text.contains(words::MAC_KEEP_KEYS), "{text}");

    // Before pairing, the other computer has no name yet.
    shows(
        &mut model_with(Os::MacOs, Vec::new()),
        &[
            "With the other computer's keyboard on this computer:",
            "The choice for this computer's keyboard is in Hop on the other computer.",
        ],
    );
}

#[test]
fn a_shortcuts_change_that_was_not_saved_shows_under_the_choice() {
    let mut model = paired();
    model.shortcuts_problem = Some("Not saved: could not save config.toml.".to_owned());
    shows(&mut model, &["Not saved: could not save config.toml."]);
}

#[test]
fn files_that_arrived_are_named_in_activity() {
    shows(
        &mut received_files(),
        &["Files from Windows PC: report.pdf, photo (2).jpg"],
    );
}

#[test]
fn accessibility_missing_and_a_windows_pc() {
    let mut model = paired();
    model.permission = Some(PermissionState::Denied);
    shows(&mut model, &["Accessibility: Not allowed yet", "Allow…"]);

    let mut pc = model_with(Os::Windows, vec![pc()]);
    pc.permission = None;
    let text = shows(
        &mut pc,
        &[
            "If Windows asks whether to let Hop use networks, choose Private networks and click \
           Allow.",
        ],
    );
    assert!(!text.contains("Accessibility"), "Mac only:\n{text}");
}

#[test]
fn the_forget_dialog_and_a_change_that_was_not_saved() {
    let mut model = paired();
    let id = DeviceId::new("a".repeat(32));
    model.peers.forgetting = Some(id.clone());
    model.peers.problem = Some((
        id,
        "Not saved: Laptop already sits above this computer.".to_owned(),
    ));
    shows(
        &mut model,
        &[
            "Forget Windows PC? You will need to pair again.",
            "Forget",
            "Not saved: Laptop already sits above this computer.",
        ],
    );
}

#[test]
fn activity_shows_the_newest_line_at_the_bottom() {
    let mut model = paired();
    for n in 0..120 {
        model.activity.push(format!("Line number {n}."));
    }
    let text = shows(&mut model, &["Line number 119."]);
    assert!(
        !text.contains("Line number 19."),
        "only the last 100 are kept"
    );
}

#[test]
fn a_closing_window_greys_everything_out_without_panicking() {
    let mut model = running(true, Session::Idle);
    model.quitting = true;
    model.sharing.stop();
    shows(&mut model, &["Stopping…"]);
}

#[test]
fn every_state_draws_at_the_real_size_and_at_the_smallest() {
    let now = Instant::now();
    let mut models = vec![
        model_with(Os::MacOs, Vec::new()),
        paired(),
        paired_pc(),
        with_shortcuts(paired_pc(), CmdCtrl::Swap),
        running(true, Session::Controlling),
        received_files(),
        showing(now),
    ];
    let mut joining = paired();
    joining.pairing.open_join();
    models.push(joining);
    let mut not_saved = paired();
    not_saved.shortcuts_problem = Some("Not saved: could not save config.toml.".to_owned());
    models.push(not_saved);
    for model in &mut models {
        for size in [WINDOW, crate::WINDOW_MIN] {
            let (text, actions) = draw_in(model, now, size);
            assert!(text.contains("Hop"), "{text}");
            assert_eq!(actions, []);
        }
    }
}

// ---- Clicks ----------------------------------------------------------------

/// Everywhere the text `label` is drawn (the middle of it), in the order it
/// was drawn.
fn find(shapes: &[ClippedShape], label: &str) -> Vec<Pos2> {
    fn search(shape: &Shape, label: &str, found: &mut Vec<Pos2>) {
        match shape {
            Shape::Text(text) if text.galley.text() == label => {
                found.push(text.pos + text.galley.rect.center().to_vec2());
            }
            Shape::Vec(shapes) => {
                for shape in shapes {
                    search(shape, label, found);
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in shapes {
        search(&clipped.shape, label, &mut found);
    }
    found
}

/// Draws `model`, clicks the text `label` (which must be on screen once)
/// with made-up pointer events, and returns what the view asked for.
fn click(model: &mut Model, label: &str) -> Vec<Action> {
    click_nth(model, label, 0, 1)
}

/// [`click`] for a label on screen `count` times: clicks the one drawn
/// `nth` (from 0).
fn click_nth(model: &mut Model, label: &str, nth: usize, count: usize) -> Vec<Action> {
    let ctx = egui::Context::default();
    let now = Instant::now();
    let mut actions = Vec::new();
    let mut frame = 0;
    let mut run = |model: &mut Model, events: Vec<Event>, actions: &mut Vec<Action>| {
        frame += 1;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(TALL[0], TALL[1]))),
            time: Some(f64::from(frame) / 60.0),
            events,
            ..RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| show(ui, model, now, actions));
        let shapes = output.shapes.clone();
        output.drop_without_applying_deltas();
        shapes
    };
    run(model, Vec::new(), &mut actions);
    let shapes = run(model, Vec::new(), &mut actions);
    let found = find(&shapes, label);
    assert_eq!(
        found.len(),
        count,
        "{label:?} is on screen {} times",
        found.len()
    );
    let at = found[nth];
    let button = |pressed| Event::PointerButton {
        pos: at,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    for events in [
        vec![Event::PointerMoved(at)],
        vec![button(true)],
        vec![button(false)],
        Vec::new(),
    ] {
        run(model, events, &mut actions);
    }
    actions
}

fn pc_id() -> DeviceId {
    DeviceId::new("a".repeat(32))
}

#[test]
fn clicks_become_actions() {
    assert_eq!(
        click(&mut paired(), "Start sharing"),
        [Action::StartSharing]
    );
    assert_eq!(
        click(&mut running(true, Session::Idle), "Stop sharing"),
        [Action::StopSharing]
    );
    assert_eq!(
        click(&mut paired(), "Left"),
        [Action::SetSide {
            id: pc_id(),
            side: Side::Left
        }]
    );
    assert_eq!(click(&mut paired(), "Right"), [], "it is there already");
    assert_eq!(
        click(&mut paired(), "Save address"),
        [Action::SaveAddress(pc_id())]
    );
    assert_eq!(
        click(&mut paired(), "Forget…"),
        [Action::AskForget(pc_id())]
    );
    assert_eq!(
        click(&mut paired(), "Show a pairing code"),
        [Action::ShowCode]
    );
    assert_eq!(click(&mut paired(), "Enter a code"), [Action::OpenJoin]);
    assert_eq!(
        click(&mut paired(), "Open log folder"),
        [Action::OpenLogFolder]
    );

    let mut missing = paired();
    missing.permission = Some(PermissionState::Denied);
    assert_eq!(click(&mut missing, "Allow…"), [Action::AllowAccessibility]);
}

#[test]
fn a_shortcuts_choice_becomes_an_action_only_when_it_changes_something() {
    assert_eq!(
        click(&mut paired(), words::MAC_SWAP_KEYS),
        [Action::SetShortcuts(CmdCtrl::Swap)]
    );
    assert_eq!(
        click(&mut paired(), words::MAC_KEEP_KEYS),
        [],
        "it is chosen already"
    );
    let mut swapped = with_shortcuts(paired_pc(), CmdCtrl::Swap);
    assert_eq!(
        click(&mut swapped, words::PC_KEEP_KEYS),
        [Action::SetShortcuts(CmdCtrl::Keep)]
    );
    assert_eq!(click(&mut swapped, words::PC_SWAP_KEYS), []);

    let mut closing = paired();
    closing.quitting = true;
    assert_eq!(
        click(&mut closing, words::MAC_SWAP_KEYS),
        [],
        "greyed out while the window closes"
    );
}

#[test]
fn a_greyed_out_button_does_nothing() {
    let mut unpaired = model_with(Os::MacOs, Vec::new());
    assert_eq!(click(&mut unpaired, "Start sharing"), []);
    let mut stopping = running(true, Session::Idle);
    stopping.sharing.stop();
    assert_eq!(click(&mut stopping, "Stop sharing"), []);
}

#[test]
fn the_join_form_pairs_or_closes() {
    let mut model = paired();
    model.pairing.open_join();
    // "Pair" is the section's title first, then the form's button.
    assert_eq!(click_nth(&mut model, "Pair", 1, 2), [], "no code yet");
    model.pairing.code.push_str("482 913");
    assert_eq!(click_nth(&mut model, "Pair", 1, 2), [Action::Join]);
    assert_eq!(
        click_nth(&mut model, "Pair", 0, 2),
        [],
        "the title is not a button"
    );
    assert_eq!(click(&mut model, "Cancel"), [Action::CloseJoin]);
    model.pairing.begin_join();
    assert_eq!(
        click(&mut model, "Cancel"),
        [Action::CancelPairing],
        "while it runs, Cancel stops it"
    );
}

#[test]
fn the_dialogs_answer_with_their_buttons() {
    let now = Instant::now();
    let question = |model: &mut Model| {
        model.pairing.apply(
            PairEvent::Confirm {
                name: "Mac".to_owned(),
                os: Os::MacOs,
                replaces: None,
                within: Duration::from_secs(90),
            },
            now,
            &mut model.activity,
        );
    };
    let mut model = showing(now);
    question(&mut model);
    assert_eq!(click(&mut model, "Yes, pair"), [Action::Answer(true)]);
    let mut model = showing(now);
    question(&mut model);
    assert_eq!(click(&mut model, "No"), [Action::Answer(false)]);

    let mut model = paired();
    model.peers.forgetting = Some(pc_id());
    assert_eq!(click(&mut model, "Forget"), [Action::Forget(pc_id())]);
    model.peers.forgetting = Some(pc_id());
    assert_eq!(click(&mut model, "Cancel"), [Action::CancelForget]);
}
