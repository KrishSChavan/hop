//! Tests of the agent with fake platforms: the action executor, the core
//! loop's fail-open duties, the updates an app hears, and two agents talking
//! over loopback hop-net. Nothing here captures or injects real input, and
//! the network tests bind 127.0.0.1 only, with discovery off (no multicast).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hop_core::{Action, CaptureMode as CoreMode, Chord, CoreConfig, State};
use hop_keymap::ModifierPolicy;
use hop_net::{DownReason, Identity, NetConfig, NetEvent, PeerUp, PinnedPeer};
use hop_platform::{CaptureMode, DropZone};
use hop_proto::{Capability, Negotiated, PROTO_MAJOR, caps};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, EntryPoint, InputEvent, KeyCode, MotionMark,
    MouseButton, Os, Point, Rect, ScrollUnit,
};

use super::exec::Executor;
use super::fake::{Call, Fake, FakeLink};
use super::link::{Link, NetLink};
use super::start::Host;
use super::{Agent, INPUT_QUEUE, PeerPlacement, Settings, Signals};
use crate::agent::start::chord;
use crate::logging;
use crate::service::{AgentHandle, AgentUpdate, PANICKED, Session};

const C: KeyCode = KeyCode::keyboard(0x06);
const Q: KeyCode = KeyCode::keyboard(0x14);

/// The Mac's id is the lower one, so it wins a simultaneous crossing.
fn mac_id() -> DeviceId {
    DeviceId::new("1".repeat(32))
}

fn pc_id() -> DeviceId {
    DeviceId::new("9".repeat(32))
}

fn display(width: f64, height: f64) -> DisplayInfo {
    DisplayInfo {
        id: 1,
        frame: Rect::new(0.0, 0.0, width, height),
        scale: 1.0,
        primary: true,
    }
}

/// Settings for a test agent. The Cmd/Ctrl swap is on (it is off by
/// default), so the tests show keys being mapped on their way.
fn settings(me: DeviceId, os: Os, peer: DeviceId, peer_name: &str, side: Edge) -> Settings {
    Settings {
        me,
        os,
        core: CoreConfig::default(),
        policy: ModifierPolicy {
            swap_cmd_ctrl: true,
        },
        peer: PeerPlacement {
            id: peer,
            name: peer_name.to_owned(),
            side,
        },
    }
}

fn peer_up(peer: DeviceId, name: &str, os: Os, screen: DisplayInfo) -> NetEvent {
    NetEvent::PeerUp(PeerUp {
        peer,
        name: name.to_owned(),
        app_version: "0.1.0".to_owned(),
        os,
        displays: vec![screen],
        negotiated: Negotiated {
            proto_major: PROTO_MAJOR,
            proto_minor: 0,
            capabilities: vec![Capability::new(caps::INPUT, 1)],
        },
        remote: SocketAddr::from(([127, 0, 0, 1], 47800)),
        dialled_by_us: true,
    })
}

/// A Mac with the PC on its right, already connected to it.
fn mac_agent() -> (Agent<FakeLink>, Fake, FakeLink, Arc<Signals>) {
    let (platform, fake) = Fake::new(1512.0, 982.0);
    let link = FakeLink::default();
    let signals = Arc::new(Signals::default());
    let mut agent = Agent::start(
        platform,
        link.clone(),
        settings(mac_id(), Os::MacOs, pc_id(), "PC", Edge::Right),
        signals.clone(),
    )
    .expect("agent");
    link.deliver(peer_up(pc_id(), "PC", Os::Windows, display(1536.0, 864.0)));
    let _ = agent.poll();
    (agent, fake, link, signals)
}

/// A Windows PC with the Mac on its left, already connected to it.
fn pc_agent() -> (Agent<FakeLink>, Fake, FakeLink) {
    let (platform, fake) = Fake::new(1536.0, 864.0);
    let link = FakeLink::default();
    let mut agent = Agent::start(
        platform,
        link.clone(),
        settings(pc_id(), Os::Windows, mac_id(), "Mac", Edge::Left),
        Arc::new(Signals::default()),
    )
    .expect("agent");
    link.deliver(peer_up(mac_id(), "Mac", Os::MacOs, display(1512.0, 982.0)));
    let _ = agent.poll();
    (agent, fake, link)
}

/// The `seq` of the last `Enter` sent.
fn enter_seq(link: &FakeLink) -> u64 {
    link.sent()
        .iter()
        .rev()
        .find_map(|msg| match msg {
            CoreMessage::Enter { seq, .. } => Some(*seq),
            _ => None,
        })
        .expect("an Enter was sent")
}

/// Pushes the Mac's cursor past its right edge and lets the PC accept.
fn cross_to_the_pc(agent: &mut Agent<FakeLink>, fake: &Fake, link: &FakeLink) -> u64 {
    fake.state().cursor = Point::new(1505.0, 400.0);
    fake.motion(20.0, 0.0);
    let _ = agent.poll();
    let seq = enter_seq(link);
    link.deliver(NetEvent::Message {
        from: pc_id(),
        msg: CoreMessage::EnterAck { seq },
    });
    let _ = agent.poll();
    assert_eq!(
        agent.state(),
        State::Controlling {
            target: pc_id(),
            seq
        }
    );
    seq
}

/// The Mac enters the PC; returns the session's `seq`.
fn mac_enters(agent: &mut Agent<FakeLink>, link: &FakeLink) -> u64 {
    let seq = 1;
    link.deliver(NetEvent::Message {
        from: mac_id(),
        msg: CoreMessage::Enter {
            seq,
            entry: EntryPoint {
                display_id: 1,
                edge: Edge::Left,
                position: Point::new(1.0, 400.0),
            },
            held_keys: Vec::new(),
            source_os: Os::MacOs,
        },
    });
    let _ = agent.poll();
    assert_eq!(agent.state(), State::Controlled { by: mac_id(), seq });
    seq
}

fn key_input(seq: u64, n: u64, code: KeyCode, pressed: bool) -> NetEvent {
    NetEvent::Message {
        from: mac_id(),
        msg: CoreMessage::Input {
            seq,
            mark: MotionMark {
                n,
                total_dx: 0.0,
                total_dy: 0.0,
            },
            events: vec![InputEvent::Key {
                code,
                pressed,
                repeat: false,
            }],
        },
    }
}

fn key(code: KeyCode, pressed: bool) -> InputEvent {
    InputEvent::Key {
        code,
        pressed,
        repeat: false,
    }
}

fn inject_from_mac(code: KeyCode, pressed: bool) -> Action {
    Action::Inject {
        event: key(code, pressed),
        source_os: Os::MacOs,
    }
}

fn executor(os: Os, swap_cmd_ctrl: bool) -> (Executor<FakeLink>, Fake, FakeLink) {
    let (platform, fake) = Fake::new(1536.0, 864.0);
    let link = FakeLink::default();
    let exec = Executor::new(platform, link.clone(), os, ModifierPolicy { swap_cmd_ctrl });
    (exec, fake, link)
}

// ----- the action executor -------------------------------------------------

#[test]
fn a_session_start_logs_how_long_the_other_computer_took_to_answer() {
    let _ = logging::tests::capture();
    let (mut exec, _fake, _link) = executor(Os::MacOs, false);
    // A peer id of its own, so the line found is this test's.
    let peer = DeviceId::new("a".repeat(32));
    let enter = CoreMessage::Enter {
        seq: 1,
        entry: EntryPoint {
            display_id: 1,
            edge: Edge::Left,
            position: Point::new(1.0, 400.0),
        },
        held_keys: Vec::new(),
        source_os: Os::MacOs,
    };
    let _ = exec.execute(vec![
        Action::Send {
            to: peer.clone(),
            msg: enter,
        },
        Action::Notify(hop_core::Notice::ControlStarted { peer: peer.clone() }),
    ]);
    let log = logging::tests::captured_text();
    assert!(
        log.lines()
            .any(|line| line.contains("session started: controlling")
                && line.contains(peer.as_str())
                && line.contains("answered_in_ms=")),
        "{log}"
    );
}

#[test]
fn cmd_c_from_a_mac_keyboard_is_injected_on_windows_as_ctrl_c() {
    let (mut exec, fake, _) = executor(Os::Windows, true);
    let outcome = exec.execute(vec![
        inject_from_mac(KeyCode::LEFT_META, true),
        inject_from_mac(C, true),
        inject_from_mac(C, false),
        inject_from_mac(KeyCode::LEFT_META, false),
    ]);
    assert!(!outcome.stalled);
    assert_eq!(
        fake.state().injected_keys(),
        [
            (KeyCode::LEFT_CONTROL, true),
            (C, true),
            (C, false),
            (KeyCode::LEFT_CONTROL, false)
        ]
    );
    assert!(fake.state().keys_down.is_empty());
}

#[test]
fn with_the_swap_off_keys_pass_through() {
    let (mut exec, fake, _) = executor(Os::Windows, false);
    let _ = exec.execute(vec![
        inject_from_mac(KeyCode::LEFT_META, true),
        inject_from_mac(KeyCode::LEFT_META, false),
    ]);
    assert_eq!(
        fake.state().injected_keys(),
        [(KeyCode::LEFT_META, true), (KeyCode::LEFT_META, false)]
    );
}

#[test]
fn ctrl_c_from_a_pc_keyboard_is_injected_on_a_mac_as_cmd_c() {
    let (mut exec, fake, _) = executor(Os::MacOs, true);
    let _ = exec.execute(vec![
        Action::Inject {
            event: key(KeyCode::LEFT_CONTROL, true),
            source_os: Os::Windows,
        },
        Action::Inject {
            event: key(C, true),
            source_os: Os::Windows,
        },
    ]);
    assert_eq!(
        fake.state().injected_keys(),
        [(KeyCode::LEFT_META, true), (C, true)]
    );
}

#[test]
fn keys_pressed_again_for_local_apps_are_never_swapped() {
    // The core presses held modifiers again when the cursor comes home,
    // with this computer's own OS as the source.
    let (mut exec, fake, _) = executor(Os::MacOs, true);
    let _ = exec.execute(vec![Action::Inject {
        event: key(KeyCode::LEFT_META, true),
        source_os: Os::MacOs,
    }]);
    assert_eq!(fake.state().injected_keys(), [(KeyCode::LEFT_META, true)]);
}

#[test]
fn a_key_up_releases_what_its_key_down_injected_even_after_a_repeat() {
    let (mut exec, fake, _) = executor(Os::Windows, true);
    let _ = exec.execute(vec![
        inject_from_mac(KeyCode::LEFT_META, true),
        Action::Inject {
            event: InputEvent::Key {
                code: KeyCode::LEFT_META,
                pressed: true,
                repeat: true,
            },
            source_os: Os::MacOs,
        },
        inject_from_mac(KeyCode::LEFT_META, false),
    ]);
    assert_eq!(
        fake.state().injected_keys(),
        [
            (KeyCode::LEFT_CONTROL, true),
            (KeyCode::LEFT_CONTROL, true),
            (KeyCode::LEFT_CONTROL, false)
        ]
    );
}

fn button(button: MouseButton, pressed: bool) -> Action {
    Action::Inject {
        event: InputEvent::Button { button, pressed },
        source_os: Os::MacOs,
    }
}

#[test]
fn releasing_everything_lets_go_of_buttons_then_letters_then_modifiers() {
    // A Ctrl-drag (a copy in Explorer) cut short by the end of a session:
    // the button comes up while Ctrl is still down, so the drop stays a
    // copy, as both platform injectors order it themselves.
    let (mut exec, fake, _) = executor(Os::Windows, true);
    let _ = exec.execute(vec![
        inject_from_mac(KeyCode::LEFT_META, true),
        inject_from_mac(C, true),
        button(MouseButton::Right, true),
        button(MouseButton::Right, false),
        button(MouseButton::Left, true),
    ]);
    fake.state().calls.clear();
    let _ = exec.execute(vec![Action::ReleaseAllInjected]);
    let state = fake.state();
    assert_eq!(
        state.calls,
        [
            Call::Inject(InputEvent::Button {
                button: MouseButton::Left,
                pressed: false
            }),
            Call::Inject(key(C, false)),
            Call::Inject(key(KeyCode::LEFT_CONTROL, false)),
            Call::ReleaseAll,
        ],
        "only the button still held is released, before the keys"
    );
    assert!(state.keys_down.is_empty() && state.buttons_down.is_empty());
}

#[test]
fn a_button_press_that_failed_is_not_released_later() {
    let (mut exec, fake, _) = executor(Os::Windows, true);
    fake.state().fail_inject = true;
    let _ = exec.execute(vec![button(MouseButton::Left, true)]);
    fake.state().fail_inject = false;
    fake.state().calls.clear();
    let _ = exec.execute(vec![Action::ReleaseAllInjected]);
    assert_eq!(fake.state().calls, [Call::ReleaseAll]);
}

#[test]
fn each_session_starts_with_a_fresh_translator() {
    // A key whose key-up never came is released at the end of its session,
    // and the next session's keys use the policy as it is then.
    let (mut exec, fake, _) = executor(Os::Windows, true);
    let _ = exec.execute(vec![
        inject_from_mac(KeyCode::LEFT_META, true),
        Action::ReleaseAllInjected,
        inject_from_mac(KeyCode::LEFT_META, false),
    ]);
    assert_eq!(
        fake.state().injected_keys(),
        [
            (KeyCode::LEFT_CONTROL, true),
            (KeyCode::LEFT_CONTROL, false),
            (KeyCode::LEFT_CONTROL, false)
        ]
    );
}

#[test]
fn failed_session_calls_report_a_stall_and_fail_open_calls_do_not() {
    let (mut exec, fake, _) = executor(Os::Windows, true);
    // Hidden before the failures start, so showing it again fails too.
    let _ = exec.execute(vec![Action::ConcealCursor]);
    {
        let mut state = fake.state();
        state.fail_inject = true;
        state.fail_park = true;
        state.fail_suppress = true;
        state.fail_conceal = true;
    }
    for (action, stalls) in [
        (inject_from_mac(C, true), true),
        (
            Action::MoveCursorTo {
                to: Point::new(5.0, 5.0),
            },
            true,
        ),
        // Cosmetic: the cursor merely stays visible, or hidden until this
        // computer's mouse is used. Before parking and restoring, which end
        // a conceal, so the failing reveal does reach the platform.
        (Action::ConcealCursor, false),
        (Action::RevealCursor, false),
        (Action::ParkCursor, true),
        (Action::SetCaptureMode(CoreMode::Suppress), true),
        (Action::SetCaptureMode(CoreMode::Passive), false),
        (
            Action::RestoreCursor {
                at: Point::new(5.0, 5.0),
            },
            false,
        ),
        (Action::ReleaseAllInjected, false),
        (Action::Nudge { to: pc_id() }, false),
    ] {
        let label = format!("{action:?}");
        assert_eq!(exec.execute(vec![action]).stalled, stalls, "{label}");
    }
}

#[test]
fn a_failed_reveal_is_only_warned_about_and_not_tried_again() {
    let (mut exec, fake, _) = executor(Os::Windows, false);
    let _ = exec.execute(vec![Action::ConcealCursor]);
    fake.state().fail_conceal = true;
    assert!(!exec.execute(vec![Action::RevealCursor]).stalled);
    assert!(fake.state().concealed, "the fake refused to show it");
    // Hop counts the cursor as shown all the same (the platform shows it by
    // itself when this computer's mouse is used), so it asks only once.
    fake.state().fail_conceal = false;
    let _ = exec.execute(vec![Action::RevealCursor]);
    exec.fail_open();
    let state = fake.state();
    assert!(!state.calls.contains(&Call::Reveal), "{:?}", state.calls);
    assert_eq!(state.calls.first(), Some(&Call::Conceal));
}

#[test]
fn the_cursor_a_session_left_behind_is_hidden_until_shown_again() {
    let (mut exec, fake, _) = executor(Os::Windows, false);
    assert!(!exec.execute(vec![Action::ConcealCursor]).stalled);
    assert!(fake.state().concealed);
    assert!(!exec.execute(vec![Action::RevealCursor]).stalled);
    assert!(!fake.state().concealed);
    // Showing a cursor that is not hidden asks the platform for nothing.
    let _ = exec.execute(vec![Action::RevealCursor]);
    assert_eq!(fake.state().calls, [Call::Conceal, Call::Reveal]);

    // Failing open shows a hidden cursor again too.
    let _ = exec.execute(vec![Action::ConcealCursor]);
    exec.fail_open();
    let state = fake.state();
    assert!(!state.concealed);
    assert_eq!(state.calls.last(), Some(&Call::Reveal));
}

#[test]
fn nudges_go_to_the_network() {
    let (mut exec, _, link) = executor(Os::MacOs, false);
    let _ = exec.execute(vec![Action::Nudge { to: pc_id() }]);
    assert_eq!(link.nudges(), [pc_id()]);
    assert!(link.sent().is_empty(), "a nudge is not a message");
}

#[test]
fn failing_open_gives_everything_back() {
    let (mut exec, fake, _) = executor(Os::Windows, true);
    fake.state().cursor = Point::new(1500.0, 400.0);
    let _ = exec.execute(vec![
        Action::SetCaptureMode(CoreMode::Suppress),
        Action::ParkCursor,
        inject_from_mac(KeyCode::LEFT_SHIFT, true),
    ]);
    fake.state().cursor = Point::new(768.0, 432.0);
    exec.fail_open();
    let state = fake.state();
    assert_eq!(state.mode, CaptureMode::Passive);
    assert!(state.keys_down.is_empty());
    assert!(!state.parked);
    assert_eq!(
        state.cursor,
        Point::new(1500.0, 400.0),
        "back where it left"
    );
}

#[test]
fn sends_go_to_the_network() {
    let (mut exec, _, link) = executor(Os::MacOs, true);
    let _ = exec.execute(vec![Action::Send {
        to: pc_id(),
        msg: CoreMessage::Heartbeat { seq: 3 },
    }]);
    assert_eq!(
        link.net().sent,
        [(pc_id(), CoreMessage::Heartbeat { seq: 3 })]
    );
}

#[test]
fn the_emergency_chord_comes_from_its_key_names() {
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        chord(&names(&["ControlLeft", "AltLeft", "ShiftLeft", "Escape"])),
        Chord::default()
    );
    assert_eq!(
        chord(&names(&["MetaLeft", "KeyQ"])),
        Chord::new([KeyCode::LEFT_META], Q)
    );
    // A name Hop does not know keeps the default, so there is always a way
    // out of a session.
    assert_eq!(chord(&names(&["ControlLeft", "Hyper"])), Chord::default());
}

/// The fakes keep the contracts that tests of the clipboard, drop zones and
/// file transfers rely on.
#[test]
fn the_fakes_keep_the_clipboard_drop_and_transfer_contracts() {
    let (mut platform, fake) = Fake::new(1512.0, 982.0);
    // The clipboard counts every write and never cuts text short.
    let count = platform.clipboard.write_text("one\ntwo").expect("write");
    assert_eq!(platform.clipboard.change_count().ok(), Some(count));
    assert_eq!(
        platform.clipboard.read_text(7).ok().flatten().as_deref(),
        Some("one\ntwo")
    );
    assert_eq!(platform.clipboard.read_text(6).ok(), Some(None));
    assert!(platform.clipboard.write_text("three").expect("write") > count);

    // The drop target remembers its zones and hands out drops oldest first.
    let zone = DropZone {
        display: Rect::new(0.0, 0.0, 1512.0, 982.0),
        edge: Edge::Right,
        start: 59.0,
        end: 923.0,
    };
    platform
        .drops
        .set_zones(vec![zone.clone()], "Drop to send to PC")
        .expect("zones");
    assert_eq!(fake.state().zones, [zone]);
    assert_eq!(fake.state().label, "Drop to send to PC");
    let (first, second) = (vec![PathBuf::from("a.txt")], vec![PathBuf::from("b.txt")]);
    fake.state().drops.extend([first.clone(), second.clone()]);
    assert_eq!(platform.drops.take_drops(), [first.clone(), second.clone()]);
    assert!(platform.drops.take_drops().is_empty());

    // The network numbers transfers from 1.
    let link = FakeLink::default();
    assert_eq!(link.send_files(&pc_id(), first.clone()).ok(), Some(1));
    assert_eq!(link.send_files(&pc_id(), second.clone()).ok(), Some(2));
    assert_eq!(link.sent_files(), [(pc_id(), first), (pc_id(), second)]);
}

// ----- the core loop ---------------------------------------------------------

#[test]
fn pushing_past_the_edge_suppresses_input_and_renews_the_lease_on_every_tick() {
    let (mut agent, fake, link, _) = mac_agent();
    cross_to_the_pc(&mut agent, &fake, &link);
    {
        let state = fake.state();
        assert_eq!(state.mode, CaptureMode::Suppress);
        assert!(state.parked);
    }
    fake.state().calls.clear();
    agent.tick();
    agent.tick();
    let renewals = fake
        .state()
        .calls
        .iter()
        .filter(|call| **call == Call::SetMode(CaptureMode::Suppress))
        .count();
    assert_eq!(renewals, 2);
}

#[test]
fn a_lapsed_lease_ends_the_session_failing_open() {
    let (mut agent, fake, link, _) = mac_agent();
    let seq = cross_to_the_pc(&mut agent, &fake, &link);
    // The backend gave input back by itself: the core thread stalled.
    fake.state().mode = CaptureMode::Passive;
    agent.tick();
    assert_eq!(agent.state(), State::Idle);
    assert!(link.sent().contains(&CoreMessage::Release { seq }));
    let state = fake.state();
    assert!(!state.parked);
    assert_eq!(state.mode, CaptureMode::Passive);
    assert!(
        !state
            .calls
            .iter()
            .rev()
            .take_while(|call| !matches!(call, Call::Restore(_)))
            .any(|call| *call == Call::SetMode(CaptureMode::Suppress)),
        "the lapsed lease was not renewed"
    );
}

#[test]
fn input_that_arrives_after_the_lease_lapsed_is_never_forwarded() {
    let (mut agent, fake, link, _) = mac_agent();
    let seq = cross_to_the_pc(&mut agent, &fake, &link);
    // The core thread stalled for over a second, so the backend gave the
    // keyboard and mouse back by itself; what the user types from then on
    // reaches the Mac's own apps. It waits in the queue for the core thread.
    fake.state().mode = CaptureMode::Passive;
    fake.key(Q, true);
    fake.key(Q, false);
    fake.motion(5.0, 0.0);
    let _ = agent.poll();
    assert_eq!(agent.state(), State::Idle);
    let sent = link.sent();
    assert!(sent.contains(&CoreMessage::Release { seq }));
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, CoreMessage::Input { .. } | CoreMessage::Motion { .. })),
        "keys typed into this computer's apps must not also reach the PC: {sent:?}"
    );
}

#[test]
fn a_full_input_queue_ends_the_session_failing_open() {
    let (mut agent, fake, link, _) = mac_agent();
    let seq = cross_to_the_pc(&mut agent, &fake, &link);
    // The core thread falls behind: capture fills the queue and drops the
    // rest.
    for _ in 0..INPUT_QUEUE + 10 {
        fake.physical(InputEvent::Scroll {
            dx: 0.0,
            dy: 1.0,
            unit: ScrollUnit::Line,
        });
    }
    let _ = agent.poll();
    assert!(link.sent().contains(&CoreMessage::Release { seq }));
    let state = fake.state();
    assert!(!state.parked);
    assert_eq!(state.mode, CaptureMode::Passive);
}

#[test]
fn a_failed_injection_ends_a_controlled_session_failing_open() {
    let (mut agent, fake, link) = pc_agent();
    let seq = mac_enters(&mut agent, &link);
    link.deliver(key_input(seq, 0, KeyCode::LEFT_SHIFT, true));
    let _ = agent.poll();
    assert_eq!(fake.state().keys_down.len(), 1);
    fake.state().fail_inject = true;
    link.deliver(key_input(seq, 0, C, true));
    let _ = agent.poll();
    assert_eq!(agent.state(), State::Idle);
    assert!(link.sent().contains(&CoreMessage::TakeBack { seq }));
    assert!(
        fake.state().keys_down.is_empty(),
        "the injector let go of everything"
    );
}

#[test]
fn a_failed_suppression_ends_the_crossing() {
    let (mut agent, fake, link, _) = mac_agent();
    fake.state().fail_suppress = true;
    fake.state().cursor = Point::new(1505.0, 400.0);
    fake.motion(20.0, 0.0);
    let _ = agent.poll();
    assert_eq!(agent.state(), State::Idle);
    let seq = enter_seq(&link);
    assert!(link.sent().contains(&CoreMessage::Release { seq }));
    assert!(!fake.state().parked);
}

#[test]
fn quitting_while_controlling_sends_release_and_gives_the_cursor_back() {
    let (mut agent, fake, link, _) = mac_agent();
    let seq = cross_to_the_pc(&mut agent, &fake, &link);
    agent.quit();
    assert_eq!(agent.state(), State::Idle);
    assert!(link.sent().contains(&CoreMessage::Release { seq }));
    let state = fake.state();
    assert!(!state.parked);
    assert_eq!(state.mode, CaptureMode::Passive);
    assert!(
        state
            .calls
            .contains(&Call::Restore(Point::new(1511.0, 400.0)))
    );
    assert_eq!(state.calls.last(), Some(&Call::StopCapture));
}

#[test]
fn quitting_while_controlled_releases_the_injected_keys() {
    let (mut agent, fake, link) = pc_agent();
    let seq = mac_enters(&mut agent, &link);
    link.deliver(key_input(seq, 0, KeyCode::LEFT_META, true));
    let _ = agent.poll();
    assert_eq!(
        fake.state().keys_down.iter().copied().collect::<Vec<_>>(),
        [KeyCode::LEFT_CONTROL],
        "the Mac's Cmd is held here as Ctrl"
    );
    agent.quit();
    assert!(link.sent().contains(&CoreMessage::TakeBack { seq }));
    let state = fake.state();
    assert!(state.keys_down.is_empty());
    assert!(
        state
            .injected_keys()
            .ends_with(&[(KeyCode::LEFT_CONTROL, false)])
    );
}

#[test]
fn a_panic_on_the_core_thread_gives_input_back() {
    // The controlling side: the cursor is parked and input suppressed.
    let (platform, mac) = Fake::new(1512.0, 982.0);
    let fake = mac.clone();
    let controlling = thread::spawn(move || {
        let link = FakeLink::default();
        let mut agent = Agent::start(
            platform,
            link.clone(),
            settings(mac_id(), Os::MacOs, pc_id(), "PC", Edge::Right),
            Arc::new(Signals::default()),
        )
        .expect("agent");
        link.deliver(peer_up(pc_id(), "PC", Os::Windows, display(1536.0, 864.0)));
        let _ = agent.poll();
        cross_to_the_pc(&mut agent, &fake, &link);
        fake.state().cursor = Point::new(756.0, 491.0);
        panic!("a test panic while controlling");
    });
    assert!(controlling.join().is_err());
    {
        let state = mac.state();
        assert_eq!(state.mode, CaptureMode::Passive);
        assert!(!state.parked, "the cursor was given back");
        assert_eq!(state.cursor, Point::new(1511.0, 400.0), "where it left");
    }

    // The controlled side: a key is held down for the peer.
    let (platform, pc) = Fake::new(1536.0, 864.0);
    let controlled = thread::spawn(move || {
        let link = FakeLink::default();
        let mut agent = Agent::start(
            platform,
            link.clone(),
            settings(pc_id(), Os::Windows, mac_id(), "Mac", Edge::Left),
            Arc::new(Signals::default()),
        )
        .expect("agent");
        link.deliver(peer_up(mac_id(), "Mac", Os::MacOs, display(1512.0, 982.0)));
        let _ = agent.poll();
        let seq = mac_enters(&mut agent, &link);
        link.deliver(key_input(seq, 0, KeyCode::LEFT_META, true));
        let _ = agent.poll();
        panic!("a test panic while controlled");
    });
    assert!(controlled.join().is_err());
    let state = pc.state();
    assert!(state.keys_down.is_empty(), "the injected Ctrl was released");
    assert_eq!(
        state.injected_keys().last(),
        Some(&(KeyCode::LEFT_CONTROL, false))
    );
}

#[test]
fn a_display_change_reaches_the_core_and_the_network() {
    let (mut agent, fake, link, _) = mac_agent();
    let bigger = display(1728.0, 1117.0);
    fake.state().displays = vec![bigger.clone()];
    agent.next_display_check = Instant::now();
    agent.tick();
    assert_eq!(link.net().announced, Some(vec![bigger.clone()]));
    let layout = agent.core.layout();
    let me = layout.device(&mac_id()).expect("this computer");
    assert_eq!(me.displays(), [bigger]);
}

// ----- updates for an app -----------------------------------------------------

/// A Mac with the PC on its right that reports to an app; not connected
/// yet.
fn reporting_mac_agent() -> (Agent<FakeLink>, Fake, FakeLink, mpsc::Receiver<AgentUpdate>) {
    let (platform, fake) = Fake::new(1512.0, 982.0);
    let link = FakeLink::default();
    let mut agent = Agent::start(
        platform,
        link.clone(),
        settings(mac_id(), Os::MacOs, pc_id(), "PC", Edge::Right),
        Arc::new(Signals::default()),
    )
    .expect("agent");
    let (tx, updates) = mpsc::channel();
    agent.report_to(tx);
    (agent, fake, link, updates)
}

fn connected(name: &str, os: Os) -> AgentUpdate {
    AgentUpdate::Connected {
        name: name.to_owned(),
        os,
    }
}

#[test]
fn a_connection_reaches_the_app_as_connected_then_disconnected() {
    let (mut agent, _, link, updates) = reporting_mac_agent();
    link.deliver(peer_up(pc_id(), "PC", Os::Windows, display(1536.0, 864.0)));
    let _ = agent.poll();
    link.deliver(NetEvent::PeerDown {
        peer: pc_id(),
        reason: DownReason::TimedOut,
    });
    let _ = agent.poll();
    assert_eq!(
        updates.try_iter().collect::<Vec<_>>(),
        [connected("PC", Os::Windows), AgentUpdate::Disconnected]
    );
}

#[test]
fn a_crossing_reports_controlling_once_then_idle() {
    let (mut agent, fake, link, updates) = reporting_mac_agent();
    link.deliver(peer_up(pc_id(), "PC", Os::Windows, display(1536.0, 864.0)));
    let _ = agent.poll();
    // Entering, then controlling once the PC accepts: one kind, one update.
    cross_to_the_pc(&mut agent, &fake, &link);
    agent.tick();
    agent.tick();
    // The lease lapsed: the session ends, failing open.
    fake.state().mode = CaptureMode::Passive;
    agent.tick();
    assert_eq!(agent.state(), State::Idle);
    assert_eq!(
        updates.try_iter().collect::<Vec<_>>(),
        [
            connected("PC", Os::Windows),
            AgentUpdate::Session(Session::Controlling),
            AgentUpdate::Session(Session::Idle),
        ]
    );
}

#[test]
fn being_controlled_is_reported_and_quitting_ends_it() {
    let (platform, _) = Fake::new(1536.0, 864.0);
    let link = FakeLink::default();
    let mut agent = Agent::start(
        platform,
        link.clone(),
        settings(pc_id(), Os::Windows, mac_id(), "Mac", Edge::Left),
        Arc::new(Signals::default()),
    )
    .expect("agent");
    let (tx, updates) = mpsc::channel();
    agent.report_to(tx);
    link.deliver(peer_up(mac_id(), "Mac", Os::MacOs, display(1512.0, 982.0)));
    let _ = agent.poll();
    mac_enters(&mut agent, &link);
    agent.quit();
    assert_eq!(
        updates.try_iter().collect::<Vec<_>>(),
        [
            connected("Mac", Os::MacOs),
            AgentUpdate::Session(Session::Controlled),
            AgentUpdate::Session(Session::Idle),
        ]
    );
}

#[test]
fn without_an_app_nothing_is_reported() {
    // `hop run`: no channel, and the core loop works as before.
    let (mut agent, fake, link, _) = mac_agent();
    cross_to_the_pc(&mut agent, &fake, &link);
    agent.quit();
    assert_eq!(agent.state(), State::Idle);
}

#[test]
fn the_agent_thread_reports_a_session_and_stopped_comes_last() {
    let (platform, fake) = Fake::new(1512.0, 982.0);
    let link = FakeLink::default();
    let thread_link = link.clone();
    let (tx, updates) = mpsc::channel();
    let handle = AgentHandle::spawn(tx, move |signals, updates| {
        let mut agent = Agent::start(
            platform,
            thread_link,
            settings(mac_id(), Os::MacOs, pc_id(), "PC", Edge::Right),
            signals.clone(),
        )?;
        agent.report_to(updates);
        agent.run();
        agent.quit();
        Ok(())
    })
    .expect("thread");
    let wait = Duration::from_secs(10);
    link.deliver(peer_up(pc_id(), "PC", Os::Windows, display(1536.0, 864.0)));
    assert_eq!(updates.recv_timeout(wait), Ok(connected("PC", Os::Windows)));

    // Push past the Mac's right edge until the core sends Enter, then the
    // PC accepts.
    fake.state().cursor = Point::new(1505.0, 400.0);
    wait_until(wait, "the Enter", || {
        fake.motion(20.0, 0.0);
        thread::sleep(Duration::from_millis(20));
        link.sent()
            .iter()
            .any(|msg| matches!(msg, CoreMessage::Enter { .. }))
    });
    assert_eq!(
        updates.recv_timeout(wait),
        Ok(AgentUpdate::Session(Session::Controlling))
    );
    let seq = enter_seq(&link);
    link.deliver(NetEvent::Message {
        from: pc_id(),
        msg: CoreMessage::EnterAck { seq },
    });
    wait_until(wait, "suppression", || {
        let state = fake.state();
        state.mode == CaptureMode::Suppress && state.parked
    });

    // Stop sharing: input back, then the thread ends with Stopped.
    handle.stop();
    assert!(handle.wait(wait), "the agent thread ended");
    assert_eq!(
        updates.try_iter().collect::<Vec<_>>(),
        [
            AgentUpdate::Session(Session::Idle),
            AgentUpdate::Stopped { error: None }
        ]
    );
    assert!(link.sent().contains(&CoreMessage::Release { seq }));
    let state = fake.state();
    assert_eq!(state.mode, CaptureMode::Passive);
    assert!(!state.parked, "the cursor was given back");
    assert_eq!(state.calls.last(), Some(&Call::StopCapture));
}

#[test]
fn stopped_is_the_last_update_even_when_starting_fails() {
    let (tx, updates) = mpsc::channel();
    let handle = AgentHandle::spawn(tx, |_, _| {
        Err(anyhow::anyhow!("UDP port 47800 is already in use").context("could not start"))
    })
    .expect("thread");
    assert!(handle.wait(Duration::from_secs(10)));
    assert_eq!(
        updates.try_iter().collect::<Vec<_>>(),
        [AgentUpdate::Stopped {
            error: Some("could not start: UDP port 47800 is already in use".to_owned())
        }],
        "the whole chain of causes"
    );
}

#[test]
fn a_panic_on_the_agent_thread_still_ends_with_stopped() {
    let (tx, updates) = mpsc::channel();
    let handle = AgentHandle::spawn(tx, |_, updates| {
        let _ = updates.send(AgentUpdate::Disconnected);
        panic!("a test panic on the agent thread");
    })
    .expect("thread");
    assert!(handle.wait(Duration::from_secs(10)));
    assert_eq!(
        updates.try_iter().collect::<Vec<_>>(),
        [
            AgentUpdate::Disconnected,
            AgentUpdate::Stopped {
                error: Some(PANICKED.to_owned())
            }
        ]
    );
}

#[test]
fn an_unpaired_computer_stops_with_the_reason_before_touching_input() {
    // The real start-up sequence, with files of the test's own. It stops at
    // the pairing check, before the platform, the network or any permission.
    let dir = std::env::temp_dir().join(format!("hop-agent-unpaired-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (config, key) = (dir.join("config.toml"), dir.join("identity.pem"));
    let (tx, updates) = mpsc::channel();
    let handle = AgentHandle::spawn(tx, move |signals, updates| {
        super::start::run(signals, Host::App(updates), || {
            crate::startup::load_from(config, &key)
        })
        .map(drop)
    })
    .expect("thread");
    assert!(handle.wait(Duration::from_secs(10)));
    let all: Vec<AgentUpdate> = updates.try_iter().collect();
    let [AgentUpdate::Stopped { error: Some(error) }] = all.as_slice() else {
        panic!("only Stopped, with the reason: {all:?}");
    };
    assert!(error.contains("not paired"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- two agents over loopback ---------------------------------------------

/// Waits until `done`, failing the test after `limit`.
fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A link on 127.0.0.1 without discovery, announcing `name`, `os` and
/// `displays`, pinned to `peer`.
fn loopback_config(
    name: &str,
    os: Os,
    displays: Vec<DisplayInfo>,
    peer: &Identity,
    peer_name: &str,
    signals: &Arc<Signals>,
) -> NetConfig {
    let mut config = NetConfig::new(0, name, env!("CARGO_PKG_VERSION"));
    config.bind = SocketAddr::from(([127, 0, 0, 1], 0));
    config.discovery = false;
    config.os = os;
    config.displays = displays;
    config.peers = vec![PinnedPeer {
        id: peer.device_id(),
        fingerprint: peer.fingerprint(),
        name: peer_name.to_owned(),
        address: None,
    }];
    let ring = signals.clone();
    config.on_event = Some(Arc::new(move || ring.ring()));
    config
}

#[test]
fn two_agents_cross_both_ways_with_the_cmd_ctrl_swap() {
    // Every log line from here on, from every crate and at every level.
    let _ = logging::tests::capture();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let mac_key = Arc::new(Identity::generate().expect("key"));
    let pc_key = Arc::new(Identity::generate().expect("key"));
    let (mac_signals, pc_signals) = (Arc::new(Signals::default()), Arc::new(Signals::default()));
    let (mac_platform, mac) = Fake::new(1512.0, 982.0);
    let (pc_platform, pc) = Fake::new(1536.0, 864.0);
    // Each link announces its fake's displays in its Hello, as `hop run`
    // announces the platform's.
    let (mac_net, mac_events) = runtime
        .block_on(hop_net::start(
            mac_key.clone(),
            loopback_config(
                "Mac",
                Os::MacOs,
                mac.state().displays.clone(),
                &pc_key,
                "PC",
                &mac_signals,
            ),
        ))
        .expect("Mac link");
    let (pc_net, pc_events) = runtime
        .block_on(hop_net::start(
            pc_key.clone(),
            loopback_config(
                "PC",
                Os::Windows,
                pc.state().displays.clone(),
                &mac_key,
                "Mac",
                &pc_signals,
            ),
        ))
        .expect("PC link");
    // Only the Mac knows where the PC is: one dial is enough.
    mac_net.set_peer_address(&pc_key.device_id(), Some(pc_net.local_addr().to_string()));

    let spawn = |platform, link, settings, signals: Arc<Signals>| {
        thread::spawn(move || {
            let mut agent = Agent::start(platform, link, settings, signals).expect("agent");
            agent.run();
            agent.quit();
        })
    };
    let mac_thread = spawn(
        mac_platform,
        NetLink::new(mac_net.clone(), mac_events),
        settings(
            mac_key.device_id(),
            Os::MacOs,
            pc_key.device_id(),
            "PC",
            Edge::Right,
        ),
        mac_signals.clone(),
    );
    let pc_thread = spawn(
        pc_platform,
        NetLink::new(pc_net.clone(), pc_events),
        settings(
            pc_key.device_id(),
            Os::Windows,
            mac_key.device_id(),
            "Mac",
            Edge::Left,
        ),
        pc_signals.clone(),
    );

    wait_until(Duration::from_secs(15), "the two links to connect", || {
        mac_net.connected_peers() == [pc_key.device_id()]
            && pc_net.connected_peers() == [mac_key.device_id()]
    });

    // 1. Push past the Mac's right edge until the PC takes the cursor. A
    // push that comes before both cores heard of the connection crosses
    // nothing; backing off re-arms the edge.
    mac.state().cursor = Point::new(1500.0, 400.0);
    wait_until(Duration::from_secs(10), "the cursor to cross", || {
        mac.motion(30.0, 0.0);
        thread::sleep(Duration::from_millis(50));
        let crossed =
            mac.state().parked && pc.state().calls.iter().any(|c| matches!(c, Call::Move(_)));
        if !crossed && mac.state().mode == CaptureMode::Passive {
            mac.motion(-40.0, 0.0);
        }
        crossed
    });
    assert_eq!(mac.state().mode, CaptureMode::Suppress);

    // 2. Cmd+C on the Mac's keyboard arrives on the PC as Ctrl+C.
    mac.key(KeyCode::LEFT_META, true);
    mac.key(C, true);
    mac.key(C, false);
    mac.key(KeyCode::LEFT_META, false);
    wait_until(Duration::from_secs(5), "Ctrl+C on the PC", || {
        pc.state().injected_keys().len() >= 4
    });
    assert_eq!(
        pc.state().injected_keys(),
        [
            (KeyCode::LEFT_CONTROL, true),
            (C, true),
            (C, false),
            (KeyCode::LEFT_CONTROL, false)
        ]
    );
    assert!(
        mac.state().injected_keys().is_empty(),
        "nothing was typed into the Mac"
    );

    // 3. Back to the left: the cursor leaves the PC's left edge and
    // reappears at the Mac's right edge.
    wait_until(Duration::from_secs(10), "the cursor to come back", || {
        mac.motion(-100.0, 0.0);
        thread::sleep(Duration::from_millis(20));
        let state = mac.state();
        state.mode == CaptureMode::Passive && !state.parked
    });
    {
        let state = mac.state();
        let restored = state.calls.iter().rev().find_map(|call| match call {
            Call::Restore(at) => Some(*at),
            _ => None,
        });
        let at = restored.expect("the cursor was shown again");
        assert_eq!(at.x, 1511.0, "it came back through the right edge");
    }
    wait_until(Duration::from_secs(5), "the PC to let go", || {
        // A release after the last key, not the one that started the
        // session.
        let state = pc.state();
        let last_key = state
            .calls
            .iter()
            .rposition(|call| matches!(call, Call::Inject(InputEvent::Key { .. })));
        let last_release = state
            .calls
            .iter()
            .rposition(|call| *call == Call::ReleaseAll);
        matches!((last_key, last_release), (Some(key), Some(release)) if release > key)
    });
    assert!(pc.state().keys_down.is_empty());

    // 4. The other way round: the PC's mouse pushes past its left edge, and
    // Ctrl+C on the PC's keyboard arrives on the Mac as Cmd+C.
    wait_until(Duration::from_secs(10), "the PC's cursor to cross", || {
        pc.motion(-30.0, 0.0);
        thread::sleep(Duration::from_millis(50));
        let crossed =
            pc.state().parked && mac.state().calls.iter().any(|c| matches!(c, Call::Move(_)));
        if !crossed && pc.state().mode == CaptureMode::Passive {
            pc.motion(40.0, 0.0);
        }
        crossed
    });
    pc.key(KeyCode::LEFT_CONTROL, true);
    pc.key(C, true);
    pc.key(C, false);
    pc.key(KeyCode::LEFT_CONTROL, false);
    wait_until(Duration::from_secs(5), "Cmd+C on the Mac", || {
        mac.state().injected_keys().len() >= 4
    });
    assert_eq!(
        mac.state().injected_keys(),
        [
            (KeyCode::LEFT_META, true),
            (C, true),
            (C, false),
            (KeyCode::LEFT_META, false)
        ]
    );
    wait_until(
        Duration::from_secs(10),
        "the cursor to go back to the PC",
        || {
            pc.motion(100.0, 0.0);
            thread::sleep(Duration::from_millis(20));
            let state = pc.state();
            state.mode == CaptureMode::Passive && !state.parked
        },
    );

    // 5. Quit both, failing open.
    mac_signals.request_quit();
    pc_signals.request_quit();
    mac_thread.join().expect("Mac agent");
    pc_thread.join().expect("PC agent");
    runtime.block_on(async {
        mac_net.shutdown().await;
        pc_net.shutdown().await;
    });
    for fake in [&mac, &pc] {
        let state = fake.state();
        assert_eq!(state.mode, CaptureMode::Passive);
        assert!(!state.parked);
        assert!(state.keys_down.is_empty());
    }

    // 6. The log of the run shows the sessions, and no key names or codes.
    let log = logging::tests::captured_text();
    assert!(log.contains("Controlling PC."), "status lines are logged");
    assert!(log.contains("Mac is controlling this computer."));
    assert!(log.contains("Controlling Mac."));
    assert!(log.contains("PC is controlling this computer."));
    assert!(
        log.contains("Back on this computer: the cursor crossed back."),
        "the session ended through the edge, not by a timeout"
    );
    for forbidden in [
        "KeyCode",
        "MetaLeft",
        "ControlLeft",
        "LEFT_META",
        "LEFT_CONTROL",
        "Key {",
        "Input {",
        "held_keys",
    ] {
        assert!(!log.contains(forbidden), "the log mentions {forbidden:?}");
    }
    let named_c = log.match_indices("KeyC").any(|(at, _)| {
        !log[at + 4..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
    });
    assert!(!named_c, "the log names the C key");
}
