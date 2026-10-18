use std::path::PathBuf;
use std::time::{Duration, Instant};

use hop_agent::pairing::{PairEvent, PairRole, Replaced, Saved};
use hop_agent::service::{AgentUpdate, Session};
use hop_agent::settings::{Overview, PeerView};
use hop_agent::{DeviceId, Os, Side, Zeroizing};

use super::pairing::{Outcome, Stage};
use super::sharing::{Next, Running, Tone};
use super::*;

const KEYS: &str = "If the keyboard and mouse ever seem stuck on the other computer, press \
                    Control + Option + Shift + Escape (the left-hand keys) on this one to bring \
                    them back at once.";

fn id(hex: char) -> DeviceId {
    DeviceId::new(std::iter::repeat_n(hex, 32).collect::<String>())
}

fn peer(name: &str, hex: char, side: Side, paired: bool) -> PeerView {
    PeerView {
        id: id(hex),
        name: name.to_owned(),
        os: Os::Windows,
        side,
        address: None,
        paired,
    }
}

fn overview(peers: Vec<PeerView>) -> Overview {
    Overview {
        name: "Mac".to_owned(),
        os: Os::MacOs,
        config_path: PathBuf::from("config.toml"),
        peers,
        notes: Vec::new(),
    }
}

fn model(peers: Vec<PeerView>) -> Model {
    Model::new(Os::MacOs, Settings::Loaded(overview(peers)), None)
}

fn paired_model() -> Model {
    model(vec![peer("Windows PC", 'a', Side::Right, true)])
}

fn started() -> AgentUpdate {
    AgentUpdate::Started {
        peer: "Windows PC".to_owned(),
        side: Side::Right,
        emergency_keys: KEYS.to_owned(),
    }
}

fn saved(side: Side, replaced: Replaced) -> Saved {
    Saved {
        name: "Windows PC".to_owned(),
        os: Os::Windows,
        side,
        replaced,
        path: PathBuf::from("config.toml"),
    }
}

fn lines(activity: &Activity) -> Vec<String> {
    activity.lines().map(str::to_owned).collect()
}

// ---- Sharing -------------------------------------------------------------

#[test]
fn sharing_goes_from_starting_to_looking_to_connected_to_a_session() {
    let mut sharing = Sharing::default();
    assert_eq!(sharing.status(), (Tone::Idle, "Not sharing.".to_owned()));
    assert!(sharing.start());
    assert!(!sharing.start(), "one agent at a time");
    assert_eq!(sharing.status(), (Tone::Busy, "Starting…".to_owned()));

    assert_eq!(sharing.apply(started()), Next::Nothing);
    assert_eq!(
        sharing.status(),
        (Tone::Busy, "Looking for Windows PC…".to_owned())
    );
    sharing.apply(AgentUpdate::Connected {
        name: "Windows PC".to_owned(),
        os: Os::Windows,
    });
    assert_eq!(
        sharing.status(),
        (Tone::Ready, "Connected to Windows PC.".to_owned())
    );
    sharing.apply(AgentUpdate::Session(Session::Controlling));
    assert_eq!(
        sharing.status(),
        (
            Tone::Active,
            "Using Windows PC: this keyboard and mouse control it now.".to_owned()
        )
    );
    sharing.apply(AgentUpdate::Session(Session::Controlled));
    assert_eq!(
        sharing.status(),
        (
            Tone::Active,
            "Windows PC is using this computer.".to_owned()
        )
    );
    sharing.apply(AgentUpdate::Disconnected);
    assert_eq!(
        sharing.running(),
        Some(&Running {
            peer: "Windows PC".to_owned(),
            side: Side::Right,
            emergency_keys: KEYS.to_owned(),
            connected: false,
            session: Session::Idle,
        }),
        "a lost connection ends the session"
    );
}

#[test]
fn stopping_says_so_until_the_agent_has_stopped() {
    let mut sharing = Sharing::default();
    sharing.start();
    sharing.apply(started());
    assert!(sharing.stop(), "the agent must be asked");
    assert!(!sharing.stop(), "only once");
    assert_eq!(sharing.status(), (Tone::Busy, "Stopping…".to_owned()));
    assert_eq!(sharing.button(true), ("Stop sharing", false));
    // Updates still on their way change nothing.
    sharing.apply(AgentUpdate::Session(Session::Controlling));
    sharing.apply(started());
    assert_eq!(sharing, Sharing::Stopping { restart: false });
    assert_eq!(
        sharing.apply(AgentUpdate::Stopped { error: None }),
        Next::Nothing
    );
    assert_eq!(sharing.status(), (Tone::Idle, "Not sharing.".to_owned()));
    assert!(!sharing.is_active());
}

#[test]
fn an_error_stops_sharing_with_the_reason_in_full() {
    let mut sharing = Sharing::default();
    sharing.start();
    let reason = "UDP port 47800 is already in use, most likely by another copy of Hop on this \
                  computer: quit it first";
    sharing.apply(AgentUpdate::Stopped {
        error: Some(reason.to_owned()),
    });
    assert_eq!(
        sharing.status(),
        (Tone::Problem, format!("Stopped: {reason}."))
    );
    assert_eq!(sharing.button(true), ("Start sharing", true));
    assert!(sharing.start(), "it can start again");
}

#[test]
fn a_stop_during_startup_ends_without_started() {
    let mut sharing = Sharing::default();
    sharing.start();
    assert!(sharing.stop());
    sharing.apply(AgentUpdate::Stopped { error: None });
    assert_eq!(sharing, Sharing::Off { stopped: None });
}

#[test]
fn a_restart_stops_the_agent_and_starts_it_again() {
    let mut sharing = Sharing::default();
    assert!(!sharing.restart(), "nothing runs");
    sharing.start();
    sharing.apply(started());
    assert!(sharing.restart());
    assert!(!sharing.restart(), "asked once");
    assert_eq!(
        sharing.status(),
        (Tone::Busy, "Starting…".to_owned()),
        "a restart reads as a start"
    );
    assert_eq!(sharing.button(true), ("Stop sharing", true));
    assert_eq!(
        sharing.apply(AgentUpdate::Stopped { error: None }),
        Next::StartAgain
    );
    assert_eq!(sharing, Sharing::Starting);
}

#[test]
fn stop_sharing_calls_off_a_restart_but_a_restart_never_overrides_a_stop() {
    let mut sharing = Sharing::default();
    sharing.start();
    sharing.restart();
    assert!(!sharing.stop(), "the agent was asked to stop already");
    assert_eq!(
        sharing.apply(AgentUpdate::Stopped { error: None }),
        Next::Nothing
    );
    assert!(!sharing.is_active());

    sharing.start();
    sharing.stop();
    assert!(!sharing.restart());
    assert_eq!(
        sharing.apply(AgentUpdate::Stopped { error: None }),
        Next::Nothing
    );
}

#[test]
fn the_button_waits_for_a_pairing() {
    let mut sharing = Sharing::default();
    assert_eq!(sharing.button(false), ("Start sharing", false));
    assert_eq!(sharing.button(true), ("Start sharing", true));
    sharing.failed_to_start("could not start sharing (no threads left)".to_owned());
    assert_eq!(
        sharing.status().1,
        "Stopped: could not start sharing (no threads left)."
    );
}

// ---- The window's other rules ---------------------------------------------

#[test]
fn hints_say_why_sharing_cannot_start_and_how_to_use_it() {
    let unpaired = model(vec![peer("Old PC", 'b', Side::Left, false)]);
    assert!(!unpaired.has_paired_peer());
    assert_eq!(
        unpaired.sharing_hints(),
        ["Pair with your other computer first."]
    );
    assert_eq!(unpaired.sharing_button(), ("Start sharing", false));

    let broken = Model::new(
        Os::MacOs,
        Settings::Failed("could not load config.toml".to_owned()),
        None,
    );
    assert_eq!(
        broken.sharing_hints(),
        ["Hop cannot share until it can read its settings (see above)."]
    );
    assert!(!broken.can_pair());

    let mut running = paired_model();
    assert!(running.sharing_hints().is_empty());
    running.sharing.start();
    assert!(running.sharing_hints().is_empty(), "nothing to say yet");
    running.sharing.apply(started());
    assert_eq!(
        running.sharing_hints(),
        [
            "Push the cursor past the right edge of this screen to move to Windows PC.",
            KEYS
        ]
    );

    let mut pc = Model::new(
        Os::Windows,
        Settings::Loaded(overview(vec![peer("Mac", 'c', Side::Left, true)])),
        None,
    );
    assert_eq!(
        pc.sharing_hints(),
        [
            "If Windows asks whether to let Hop use networks, choose Private networks and click \
             Allow."
        ],
        "always, on Windows"
    );
    pc.quitting = true;
    assert_eq!(pc.sharing_button(), ("Start sharing", false));
}

#[test]
fn new_settings_say_whether_the_computers_changed() {
    let mut model = paired_model();
    assert!(!model.set_settings(Settings::Loaded(overview(vec![peer(
        "Windows PC",
        'a',
        Side::Right,
        true
    )]))));
    assert!(model.set_settings(Settings::Loaded(overview(vec![peer(
        "Windows PC",
        'a',
        Side::Left,
        true
    )]))));
    assert!(!model.set_settings(Settings::Failed("gone".to_owned())));
    assert!(!model.settings_loaded());
    assert!(!model.has_paired_peer());
}

#[test]
fn address_fields_follow_the_config_unless_the_user_typed_something() {
    let mut pc = peer("Windows PC", 'a', Side::Right, true);
    let laptop = peer("Laptop", 'b', Side::Left, true);
    let mut model = model(vec![pc.clone(), laptop.clone()]);
    assert_eq!(model.peers.addresses[&id('a')], "");

    // The user types an address for the laptop but has not saved it; the
    // PC's address is saved.
    model
        .peers
        .addresses
        .insert(id('b'), "192.168.1.21".to_owned());
    pc.address = Some("192.168.1.20".to_owned());
    model.set_settings(Settings::Loaded(overview(vec![pc.clone(), laptop.clone()])));
    assert_eq!(model.peers.addresses[&id('a')], "192.168.1.20");
    assert_eq!(model.peers.addresses[&id('b')], "192.168.1.21", "kept");

    // A saved address shows as the config has it.
    model
        .peers
        .addresses
        .insert(id('a'), "  pc.local  ".to_owned());
    pc.address = Some("pc.local".to_owned());
    model.set_settings(Settings::Loaded(overview(vec![pc.clone(), laptop])));
    model.show_saved_address(&id('a'));
    assert_eq!(model.peers.addresses[&id('a')], "pc.local");

    // A forgotten computer's field, dialog and problem go with it.
    model.peers.forgetting = Some(id('b'));
    model.peers.problem = Some((id('b'), "Not saved: no.".to_owned()));
    assert_eq!(
        model.forgetting().map(|peer| peer.name.as_str()),
        Some("Laptop")
    );
    model.set_settings(Settings::Loaded(overview(vec![pc])));
    assert!(!model.peers.addresses.contains_key(&id('b')));
    assert_eq!(model.peers.forgetting, None);
    assert_eq!(model.peers.problem, None);
}

#[test]
fn only_paired_computers_are_listed() {
    let model = model(vec![
        peer("Old PC", 'b', Side::Left, false),
        peer("Windows PC", 'a', Side::Right, true),
    ]);
    let names: Vec<&str> = model.paired_peers().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Windows PC"]);
}

#[test]
fn activity_keeps_the_last_hundred_lines() {
    let mut activity = Activity::default();
    assert_eq!(activity.lines().next(), None);
    for n in 0..150 {
        activity.push(format!("line {n}"));
    }
    let lines = lines(&activity);
    assert_eq!(lines.len(), ACTIVITY_LINES);
    assert_eq!(lines.first().map(String::as_str), Some("line 50"));
    assert_eq!(lines.last().map(String::as_str), Some("line 149"));
}

// ---- Pairing -------------------------------------------------------------

#[test]
fn showing_a_code_asks_the_user_and_pairs_on_yes() {
    let now = Instant::now();
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    assert!(pairing.begin_show());
    assert!(!pairing.begin_show(), "one session at a time");
    assert!(pairing.busy);

    let code = Zeroizing::new("482 913".to_owned());
    assert_eq!(
        pairing.apply(PairEvent::Code(code), now, &mut activity),
        None
    );
    assert!(pairing.is_counting_down());
    let Stage::Show(showing) = &pairing.stage else {
        panic!("not showing");
    };
    assert_eq!(showing.code.as_deref().map(String::as_str), Some("482 913"));
    assert_eq!(showing.expires, Some(now + Duration::from_secs(120)));

    pairing.apply(PairEvent::WrongCode { failures: 1 }, now, &mut activity);
    pairing.apply(
        PairEvent::Confirm {
            name: "Mac".to_owned(),
            os: Os::MacOs,
            replaces: None,
            within: Duration::from_secs(90),
        },
        now,
        &mut activity,
    );
    let Stage::Show(showing) = &pairing.stage else {
        panic!("not showing");
    };
    assert_eq!(showing.wrong, 1);
    let question = showing.question.clone().expect("the question");
    assert_eq!(question.name, "Mac");
    assert_eq!(question.deadline, now + Duration::from_secs(90));

    assert!(pairing.answer(true, now));
    assert!(!pairing.answer(true, now), "one answer per question");
    let Stage::Show(showing) = &pairing.stage else {
        panic!("not showing");
    };
    assert_eq!(showing.accepted.as_deref(), Some("Mac"));

    let done = saved(Side::Left, Replaced::Nothing);
    assert_eq!(
        pairing.apply(PairEvent::Finished(Ok(done.clone())), now, &mut activity),
        Some(done)
    );
    assert!(!pairing.busy);
    assert!(matches!(pairing.stage, Stage::Choose), "the code is gone");
    assert_eq!(
        pairing.outcome,
        Some(Outcome {
            paired: true,
            lines: vec![
                "Paired with Windows PC. It sits to the left of this screen; change that under \
                 Your other computer."
                    .to_owned()
            ],
        })
    );
    let said = lines(&activity);
    assert_eq!(
        said,
        [
            "Showing a pairing code. It works once, for 2 minutes.",
            "Someone entered a wrong code (1 of 3 tries used).",
            "Mac (macOS) entered the code.",
            "Paired with Windows PC. It sits to the left of this screen; change that under Your \
             other computer.",
        ]
    );
    for line in said {
        assert!(
            !line.contains("482"),
            "the code is never in Activity: {line}"
        );
    }
}

#[test]
fn a_cancelled_code_says_cancelled_not_a_reason() {
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.begin_show();
    assert!(pairing.cancel());
    assert!(!pairing.cancel(), "once");
    pairing.apply(
        PairEvent::Finished(Err("cancelled".to_owned())),
        Instant::now(),
        &mut activity,
    );
    assert!(matches!(pairing.stage, Stage::Choose));
    assert!(!pairing.busy && !pairing.cancelling);
    assert_eq!(
        pairing.outcome.map(|outcome| outcome.lines),
        Some(vec!["Pairing cancelled.".to_owned()])
    );
}

#[test]
fn a_declined_or_expired_code_says_not_paired_with_the_reason() {
    let now = Instant::now();
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.begin_show();
    pairing.apply(
        PairEvent::Confirm {
            name: "Mac".to_owned(),
            os: Os::MacOs,
            replaces: Some(
                "This replaces the pairing with Old Mac, whose old key will stop \
                            working."
                    .to_owned(),
            ),
            within: Duration::from_secs(5),
        },
        now,
        &mut activity,
    );
    assert!(pairing.answer(false, now));
    pairing.apply(
        PairEvent::Finished(Err("nothing was saved on either computer".to_owned())),
        Instant::now(),
        &mut activity,
    );
    assert_eq!(
        pairing.outcome,
        Some(Outcome {
            paired: false,
            lines: vec!["Not paired: nothing was saved on either computer.".to_owned()],
        })
    );
}

#[test]
fn entering_a_code_needs_one_and_keeps_it_for_another_try() {
    let now = Instant::now();
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.open_join();
    assert!(matches!(pairing.stage, Stage::Join(_)));
    assert!(!pairing.can_join(), "no code yet");
    assert!(pairing.begin_join().is_none());
    pairing.code.push_str("   ");
    assert!(!pairing.can_join(), "spaces are not a code");
    pairing.code.clear();
    pairing.code.push_str("482 913");
    pairing.address.push_str("192.168.1.20");

    let Some(PairRole::Join { code, address }) = pairing.begin_join() else {
        panic!("no role");
    };
    assert_eq!(code.as_str(), "482 913");
    assert_eq!(address.as_deref(), Some("192.168.1.20"));
    assert!(!pairing.can_join(), "while it runs");

    pairing.apply(PairEvent::Searching, now, &mut activity);
    pairing.apply(PairEvent::Connecting, now, &mut activity);
    pairing.apply(
        PairEvent::WaitingForConfirmation {
            host: "Windows PC".to_owned(),
        },
        now,
        &mut activity,
    );
    let Stage::Join(joining) = &pairing.stage else {
        panic!("not joining");
    };
    assert_eq!(
        joining.progress.as_deref(),
        Some("Now click Yes, pair on Windows PC.")
    );
    pairing.apply(
        PairEvent::Finished(Err(
            "Windows PC did not answer: the code did not match; check it and try again".to_owned(),
        )),
        now,
        &mut activity,
    );
    let Stage::Join(joining) = &pairing.stage else {
        panic!("the form stays open");
    };
    assert_eq!(joining.progress, None);
    assert_eq!(pairing.code.as_str(), "482 913", "kept for another try");
    assert_eq!(
        lines(&activity),
        [
            "Looking for the computer showing the code…",
            "Connecting…",
            "Now click Yes, pair on Windows PC.",
            "Not paired: Windows PC did not answer: the code did not match; check it and try \
             again.",
        ]
    );

    pairing.close_join();
    assert!(matches!(pairing.stage, Stage::Choose));
    assert_eq!(pairing.outcome, None);
    assert!(pairing.code.is_empty(), "Cancel wipes the typed code");
}

#[test]
fn showing_a_code_wipes_a_typed_one() {
    let mut pairing = Pairing::default();
    pairing.open_join();
    pairing.code.push_str("482 913");
    assert!(pairing.begin_show());
    assert!(pairing.code.is_empty());
}

#[test]
fn an_answer_after_the_time_is_up_does_not_count() {
    let now = Instant::now();
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.begin_show();
    pairing.apply(
        PairEvent::Confirm {
            name: "Mac".to_owned(),
            os: Os::MacOs,
            replaces: None,
            within: Duration::from_secs(90),
        },
        now,
        &mut activity,
    );
    let late = now + Duration::from_secs(90);
    assert!(!pairing.answer(true, late), "the flow took it as No");
    let Stage::Show(showing) = &pairing.stage else {
        panic!("not showing");
    };
    assert!(
        showing.question.is_some(),
        "the question stays until the end"
    );
    assert_eq!(showing.accepted, None, "not \"Pairing with Mac…\"");
    assert!(pairing.answer(true, late - Duration::from_millis(1)));
}

#[test]
fn nothing_found_or_several_found_points_to_the_address_field() {
    let now = Instant::now();
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.open_join();
    pairing.code.push_str("482913");
    pairing.begin_join();
    pairing.apply(PairEvent::Searching, now, &mut activity);
    pairing.apply(PairEvent::NoHostFound, now, &mut activity);
    let Stage::Join(joining) = &pairing.stage else {
        panic!("not joining");
    };
    assert_eq!(joining.progress, None);
    let problem = joining.problem.clone().expect("a problem");
    assert!(problem.contains("type its address in Address"), "{problem}");
    pairing.apply(
        PairEvent::Finished(Err(
            "could not find a computer showing a pairing code".to_owned()
        )),
        now,
        &mut activity,
    );
    let Stage::Join(joining) = &pairing.stage else {
        panic!("not joining");
    };
    assert_eq!(
        joining.problem.as_deref(),
        Some(problem.as_str()),
        "it stays"
    );

    pairing.begin_join();
    let Stage::Join(joining) = &pairing.stage else {
        panic!("not joining");
    };
    assert_eq!(joining.problem, None, "a new try starts afresh");
    pairing.apply(
        PairEvent::SeveralHosts(vec![
            ("PC".to_owned(), vec!["192.168.1.20".parse().expect("ip")]),
            (
                "Laptop".to_owned(),
                vec!["192.168.1.21".parse().expect("ip")],
            ),
        ]),
        now,
        &mut activity,
    );
    let Stage::Join(joining) = &pairing.stage else {
        panic!("not joining");
    };
    let problem = joining.problem.clone().expect("a problem");
    assert!(
        problem.contains("PC (192.168.1.20); Laptop (192.168.1.21)"),
        "{problem}"
    );
}

#[test]
fn a_pairing_saved_only_here_says_however_and_an_old_key_is_named() {
    let now = Instant::now();
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.begin_show();
    pairing.apply(
        PairEvent::Unconfirmed {
            reason: "the connection closed".to_owned(),
        },
        now,
        &mut activity,
    );
    let done = saved(Side::Right, Replaced::OldKey("Old PC".to_owned()));
    pairing.apply(PairEvent::Finished(Ok(done)), now, &mut activity);
    assert_eq!(
        pairing.outcome.map(|outcome| outcome.lines),
        Some(vec![
            "Paired with Windows PC. It sits to the right of this screen; change that under Your \
             other computer."
                .to_owned(),
            "It replaces the pairing with Old PC, whose old key can no longer connect to this \
             computer."
                .to_owned(),
            "However: the connection closed.".to_owned(),
        ])
    );
}

#[test]
fn a_successful_join_wipes_the_typed_code() {
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.open_join();
    pairing.code.push_str("482 913");
    pairing.begin_join();
    pairing.apply(
        PairEvent::Finished(Ok(saved(Side::Right, Replaced::Entry))),
        Instant::now(),
        &mut activity,
    );
    assert!(pairing.code.is_empty());
    assert!(matches!(pairing.stage, Stage::Choose));
}

#[test]
fn a_session_that_cannot_start_says_why() {
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.begin_show();
    pairing.failed_to_start("could not start pairing (no threads left)", &mut activity);
    assert!(!pairing.busy);
    assert!(matches!(pairing.stage, Stage::Choose));
    assert_eq!(
        lines(&activity),
        ["Not paired: could not start pairing (no threads left)."]
    );
}

#[test]
fn an_unannounced_computer_tells_the_user_to_give_its_address() {
    let mut activity = Activity::default();
    let mut pairing = Pairing::default();
    pairing.begin_show();
    pairing.apply(
        PairEvent::NotAnnounced {
            reason: "no network interface".to_owned(),
        },
        Instant::now(),
        &mut activity,
    );
    let Stage::Show(showing) = &pairing.stage else {
        panic!("not showing");
    };
    let line = showing.not_announced.clone().expect("a line");
    assert!(line.contains("(no network interface)"), "{line}");
    assert!(line.contains("IP address in Address"), "{line}");
    assert!(!pairing.is_counting_down(), "no code yet");
}
