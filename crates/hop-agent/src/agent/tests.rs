//! Tests of the agent with fake platforms: the action executor, the core
//! loop's fail-open duties, the updates an app hears, the clipboard and
//! files, and two agents talking over loopback hop-net. Nothing here
//! captures or injects real input, touches the real clipboard or opens a
//! window, and the network tests bind 127.0.0.1 only, with discovery off
//! (no multicast).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hop_core::{Action, CaptureMode as CoreMode, Chord, CoreConfig, State};
use hop_keymap::ModifierPolicy;
use hop_net::{
    DownReason, Identity, NetConfig, NetEvent, PeerUp, PinnedPeer, Refusal, SendError,
    TransferEvent, TransferFailure,
};
use hop_platform::{CaptureMode, DropZone, Platform};
use hop_proto::{Capability, Negotiated, PROTO_MAJOR, ProtoError, caps};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, EntryPoint, InputEvent, KeyCode, MotionMark,
    MouseButton, Os, Point, Rect, ScrollUnit,
};

use super::clipboard::ClipboardSettings;
use super::exec::{Executor, Features};
use super::fake::{Call, Fake, FakeLink};
use super::link::{Link, NetLink};
use super::start::Host;
use super::{Agent, INPUT_QUEUE, PeerPlacement, Settings, ShowFiles, Signals};
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
/// default), so the tests show keys being mapped on their way. Clipboard
/// sync and files are on, as by default; they work only with a peer that
/// negotiated them.
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
        clipboard: ClipboardSettings {
            enabled: true,
            max_text_bytes: 1_048_576,
        },
        files: true,
        // A test never opens a window: files that arrive are not shown.
        show_files: ShowFiles::with(|_| {}),
    }
}

/// A connection that negotiated input only, as with a Hop from before M4.
fn peer_up(peer: DeviceId, name: &str, os: Os, screen: DisplayInfo) -> NetEvent {
    peer_up_with(peer, name, os, screen, &[caps::INPUT])
}

/// A connection that negotiated `capabilities`.
fn peer_up_with(
    peer: DeviceId,
    name: &str,
    os: Os,
    screen: DisplayInfo,
    capabilities: &[&str],
) -> NetEvent {
    NetEvent::PeerUp(PeerUp {
        peer,
        name: name.to_owned(),
        app_version: "0.1.0".to_owned(),
        os,
        displays: vec![screen],
        negotiated: Negotiated {
            proto_major: PROTO_MAJOR,
            proto_minor: 0,
            capabilities: capabilities
                .iter()
                .map(|name| Capability::new(*name, 1))
                .collect(),
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

/// Pushes the Mac's cursor past its right edge: the `Enter` goes out, and
/// the crossing waits for the PC's answer. Returns the session's `seq`.
fn enter_the_pc(agent: &mut Agent<FakeLink>, fake: &Fake, link: &FakeLink) -> u64 {
    fake.state().cursor = Point::new(1505.0, 400.0);
    fake.motion(20.0, 0.0);
    let _ = agent.poll();
    let seq = enter_seq(link);
    assert_eq!(
        agent.state(),
        State::Entering {
            target: pc_id(),
            seq
        }
    );
    seq
}

/// The PC's answer to the `Enter` of session `seq`.
fn enter_ack(seq: u64) -> NetEvent {
    NetEvent::Message {
        from: pc_id(),
        msg: CoreMessage::EnterAck { seq },
    }
}

/// Pushes the Mac's cursor past its right edge and lets the PC accept.
fn cross_to_the_pc(agent: &mut Agent<FakeLink>, fake: &Fake, link: &FakeLink) -> u64 {
    let seq = enter_the_pc(agent, fake, link);
    link.deliver(enter_ack(seq));
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

/// An executor on `os` that syncs the clipboard with `peer`, which is
/// connected and takes clipboard text; something new is on the clipboard.
fn syncing_executor(os: Os, peer: DeviceId) -> (Executor<FakeLink>, Fake, FakeLink) {
    let (mut exec, fake, link) = executor(os, false);
    exec.share_clipboard(ClipboardSettings {
        enabled: true,
        max_text_bytes: 1_048_576,
    });
    exec.peer_up(
        peer,
        Features {
            clipboard: true,
            files: false,
        },
    );
    fake.copy("copied");
    (exec, fake, link)
}

fn copied() -> CoreMessage {
    CoreMessage::ClipboardText {
        generation: 1,
        text: "copied".to_owned(),
    }
}

#[test]
fn the_clipboard_follows_its_enter_even_when_another_message_comes_next() {
    let (mut exec, _, link) = syncing_executor(Os::MacOs, pc_id());
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
    let heartbeat = CoreMessage::Heartbeat { seq: 1 };
    let _ = exec.execute(vec![
        Action::Send {
            to: pc_id(),
            msg: enter.clone(),
        },
        Action::Send {
            to: pc_id(),
            msg: heartbeat.clone(),
        },
    ]);
    assert_eq!(link.sent(), [enter, copied(), heartbeat]);
}

/// A `Leave` from the PC back to the Mac.
fn leave_to_the_mac(seq: u64) -> CoreMessage {
    CoreMessage::Leave {
        seq,
        to: mac_id(),
        entry: EntryPoint {
            display_id: 1,
            edge: Edge::Right,
            position: Point::new(1511.0, 400.0),
        },
    }
}

#[test]
fn the_controlled_side_sends_its_clipboard_before_its_leave() {
    // The clipboard is read and sent before the Leave, so the controller
    // has the text before it takes its keyboard back; letting go of what
    // was injected and hiding the cursor come after, as the core asked.
    let (mut exec, fake, link) = syncing_executor(Os::Windows, mac_id());
    let _ = exec.execute(vec![inject_from_mac(KeyCode::LEFT_SHIFT, true)]);
    fake.state().calls.clear();
    let leave = leave_to_the_mac(1);
    let _ = exec.execute(vec![
        Action::Send {
            to: mac_id(),
            msg: leave.clone(),
        },
        Action::ReleaseAllInjected,
        Action::ConcealCursor,
    ]);
    assert_eq!(link.sent(), [copied(), leave]);
    let state = fake.state();
    assert_eq!(
        state.calls,
        [
            Call::ReadClipboard,
            Call::Inject(key(KeyCode::LEFT_SHIFT, false)),
            Call::ReleaseAll,
            Call::Conceal,
        ]
    );
}

#[test]
fn a_leave_goes_even_when_the_clipboard_cannot_be_read_or_sent() {
    for (read, send) in [(false, true), (true, false)] {
        let (mut exec, fake, link) = syncing_executor(Os::Windows, mac_id());
        fake.state().fail_clipboard_read = !read;
        if !send {
            link.net().refuse_clipboard = Some(|| SendError::Overloaded);
        }
        let leave = leave_to_the_mac(1);
        let _ = exec.execute(vec![
            Action::Send {
                to: mac_id(),
                msg: leave.clone(),
            },
            Action::ReleaseAllInjected,
        ]);
        assert_eq!(link.sent(), [leave], "read {read}, send {send}");
        // Nothing is left over for later: the Leave's chance has passed.
        let _ = exec.execute(vec![Action::Send {
            to: mac_id(),
            msg: CoreMessage::Heartbeat { seq: 2 },
        }]);
        assert_eq!(link.sent_clipboard(), []);
    }
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

// ----- the clipboard and files -----------------------------------------------

/// What a connection between two current builds negotiates: input,
/// clipboard text and files.
const EVERYTHING: [&str; 3] = [caps::INPUT, caps::CLIPBOARD_TEXT, caps::FILES];

/// The V key.
const V: KeyCode = KeyCode::keyboard(0x19);

/// A test agent with everything around it: its fake platform and network,
/// what it told the app, and the files it showed.
struct Rig {
    agent: Agent<FakeLink>,
    fake: Fake,
    link: FakeLink,
    app: mpsc::Receiver<AgentUpdate>,
    shown: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    /// The other computer's id and name, as its connection gives them.
    peer: (DeviceId, String),
    /// The other computer's OS and screen.
    peer_screen: (Os, DisplayInfo),
}

impl Rig {
    /// A Mac with the PC, which calls itself `name`, on its right, with its
    /// settings changed by `adjust`. Not connected yet.
    fn mac(name: &str, adjust: impl FnOnce(&mut Settings)) -> Rig {
        Rig::start(
            Fake::new(1512.0, 982.0),
            settings(mac_id(), Os::MacOs, pc_id(), name, Edge::Right),
            adjust,
            (Os::Windows, display(1536.0, 864.0)),
        )
    }

    /// A PC with the Mac, which calls itself `name`, on its left, with its
    /// settings changed by `adjust`. Not connected yet.
    fn pc(name: &str, adjust: impl FnOnce(&mut Settings)) -> Rig {
        Rig::start(
            Fake::new(1536.0, 864.0),
            settings(pc_id(), Os::Windows, mac_id(), name, Edge::Left),
            adjust,
            (Os::MacOs, display(1512.0, 982.0)),
        )
    }

    fn start(
        (platform, fake): (Platform, Fake),
        mut settings: Settings,
        adjust: impl FnOnce(&mut Settings),
        peer_screen: (Os, DisplayInfo),
    ) -> Rig {
        let shown = Arc::new(Mutex::new(Vec::new()));
        let record = shown.clone();
        settings.show_files = ShowFiles::with(move |files| {
            record
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(files.to_vec());
        });
        adjust(&mut settings);
        let peer = (settings.peer.id.clone(), settings.peer.name.clone());
        let link = FakeLink::default();
        let mut agent = Agent::start(
            platform,
            link.clone(),
            settings,
            Arc::new(Signals::default()),
        )
        .expect("agent");
        let (tx, app) = mpsc::channel();
        agent.report_to(tx);
        Rig {
            agent,
            fake,
            link,
            app,
            shown,
            peer,
            peer_screen,
        }
    }

    /// The other computer connects, over a link that negotiated
    /// `capabilities`.
    fn connect(&mut self, capabilities: &[&str]) {
        let (id, name) = &self.peer;
        let (os, screen) = &self.peer_screen;
        self.deliver(peer_up_with(
            id.clone(),
            name,
            *os,
            screen.clone(),
            capabilities,
        ));
    }

    /// The other computer's connection closes.
    fn disconnect(&mut self) {
        self.deliver(NetEvent::PeerDown {
            peer: self.peer.0.clone(),
            reason: DownReason::TimedOut,
        });
    }

    /// Hands `event` to the agent, as the network would.
    fn deliver(&mut self, event: NetEvent) {
        self.link.deliver(event);
        let _ = self.agent.poll();
    }

    /// Pushes the Mac's cursor past its right edge and lets the PC accept.
    fn cross(&mut self) -> u64 {
        cross_to_the_pc(&mut self.agent, &self.fake, &self.link)
    }

    /// The PC hands the cursor back to the Mac through its left edge, as
    /// the session `seq` ends; `first` arrives right before the `Leave`, as
    /// the PC's clipboard does.
    fn come_back(&mut self, seq: u64, first: Option<NetEvent>) {
        if let Some(event) = first {
            self.link.deliver(event);
        }
        self.link.deliver(NetEvent::Message {
            from: pc_id(),
            msg: leave_to_the_mac(seq),
        });
        let _ = self.agent.poll();
        assert_eq!(self.agent.state(), State::Idle);
    }

    /// What the agent told the app since the last call.
    fn updates(&self) -> Vec<AgentUpdate> {
        self.app.try_iter().collect()
    }

    /// The files the agent showed in Finder or Explorer, as it would have.
    fn shown(&self) -> Vec<Vec<PathBuf>> {
        self.shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Clipboard text from `from`.
fn clipboard_from(from: DeviceId, text: &str) -> NetEvent {
    NetEvent::Message {
        from,
        msg: CoreMessage::ClipboardText {
            generation: 1,
            text: text.to_owned(),
        },
    }
}

/// Where in `link`'s messages the first one `is` matches is.
fn position_of(link: &FakeLink, is: impl Fn(&CoreMessage) -> bool) -> usize {
    link.sent()
        .iter()
        .position(is)
        .expect("the message was sent")
}

#[test]
fn the_clipboard_goes_to_the_pc_right_after_enter() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("Clipboard PC", |_| {});
    rig.connect(&EVERYTHING);
    rig.fake.copy("clipboard-secret-going-over");
    rig.cross();
    let enter = position_of(&rig.link, |msg| matches!(msg, CoreMessage::Enter { .. }));
    assert_eq!(
        rig.link.net().sent.get(enter + 1),
        Some(&(
            pc_id(),
            CoreMessage::ClipboardText {
                generation: 1,
                text: "clipboard-secret-going-over".to_owned()
            }
        )),
        "right after the Enter, so it arrives before any key"
    );
    assert_eq!(rig.link.sent_clipboard().len(), 1);

    let log = logging::tests::captured_text();
    assert!(
        log.contains("Copy and paste now work between this computer and Clipboard PC."),
        "said once the connection carries clipboard text"
    );
    assert!(
        !log.contains("clipboard-secret"),
        "the log never has clipboard text"
    );
}

#[test]
fn the_clipboard_goes_to_the_mac_right_before_leave() {
    let mut rig = Rig::pc("Mac", |_| {});
    rig.connect(&EVERYTHING);
    let seq = mac_enters(&mut rig.agent, &rig.link);
    // Ctrl+C from the Mac's keyboard copied something on the PC.
    rig.fake.copy("copied on the PC");
    // The Mac moves the cursor out through the PC's left edge.
    rig.deliver(NetEvent::Message {
        from: mac_id(),
        msg: CoreMessage::Motion {
            seq,
            mark: MotionMark {
                n: 1,
                total_dx: -50.0,
                total_dy: 0.0,
            },
        },
    });
    assert_eq!(rig.agent.state(), State::Idle);
    let leave = position_of(&rig.link, |msg| matches!(msg, CoreMessage::Leave { .. }));
    let sent = rig.link.net().sent.clone();
    assert_eq!(sent[leave].0, mac_id());
    assert_eq!(
        leave.checked_sub(1).and_then(|text| sent.get(text)),
        Some(&(
            mac_id(),
            CoreMessage::ClipboardText {
                generation: 1,
                text: "copied on the PC".to_owned()
            }
        )),
        "right before the Leave, to the computer the cursor goes to"
    );
}

/// The Mac writes the PC's text while it still controls the PC, before it
/// takes its keyboard back: Cmd+V right after coming back pastes the text
/// copied on the PC, not the Mac's old one.
#[test]
fn text_from_the_pc_is_on_the_clipboard_before_the_mac_has_its_keyboard_back() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    let seq = rig.cross();
    rig.fake.state().calls.clear();
    rig.come_back(seq, Some(clipboard_from(pc_id(), "copied on the PC")));
    let state = rig.fake.state();
    assert_eq!(state.clipboard.as_deref(), Some("copied on the PC"));
    let written = state
        .calls
        .iter()
        .position(|call| *call == Call::WriteClipboard)
        .expect("written");
    let keyboard_back = state
        .calls
        .iter()
        .position(|call| *call == Call::SetMode(CaptureMode::Passive))
        .expect("the keyboard came back");
    assert!(written < keyboard_back, "{:?}", state.calls);
}

#[test]
fn what_was_copied_before_sharing_started_stays_here() {
    let (platform, fake) = Fake::new(1512.0, 982.0);
    fake.copy("copied before sharing started");
    let mut rig = Rig::start(
        (platform, fake),
        settings(mac_id(), Os::MacOs, pc_id(), "PC", Edge::Right),
        |_| {},
        (Os::Windows, display(1536.0, 864.0)),
    );
    rig.connect(&EVERYTHING);
    rig.cross();
    assert_eq!(rig.link.sent_clipboard(), []);
    assert_eq!(rig.fake.state().clipboard_reads, 0, "not even read");
}

/// A Mac whose clipboard could not say its change count as sharing
/// started (it took too long, say), with `copied` on it from before.
fn mac_with_an_unknown_start(copied: &str) -> Rig {
    let (platform, fake) = Fake::new(1512.0, 982.0);
    fake.copy(copied);
    fake.state().fail_clipboard_count = true;
    let rig = Rig::start(
        (platform, fake),
        settings(mac_id(), Os::MacOs, pc_id(), "PC", Edge::Right),
        |_| {},
        (Os::Windows, display(1536.0, 864.0)),
    );
    rig.fake.state().fail_clipboard_count = false;
    rig
}

#[test]
fn a_clipboard_count_unknown_at_the_start_never_pushes_what_was_there() {
    let mut rig = mac_with_an_unknown_start("copied before sharing started");
    rig.connect(&EVERYTHING);
    let seq = rig.cross();
    assert_eq!(rig.link.sent_clipboard(), [], "fail closed");
    assert_eq!(rig.fake.state().clipboard_reads, 0, "not even read");
    rig.come_back(seq, None);
    rig.fake.copy("copied while sharing");
    rig.cross();
    assert_eq!(
        rig.link.sent_clipboard(),
        [(pc_id(), "copied while sharing".to_owned())]
    );
}

#[test]
fn a_clipboard_count_unknown_at_the_start_is_read_again_between_sessions() {
    let mut rig = mac_with_an_unknown_start("copied before sharing started");
    rig.connect(&EVERYTHING);
    // The slower checks run every few seconds; this one runs now.
    rig.agent.next_display_check = Instant::now();
    rig.agent.tick();
    rig.fake.copy("copied while sharing");
    rig.cross();
    assert_eq!(
        rig.link.sent_clipboard(),
        [(pc_id(), "copied while sharing".to_owned())],
        "the first crossing carries it"
    );
}

#[test]
fn the_clipboard_goes_once_and_not_again_until_it_changes() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    rig.fake.copy("one");
    let seq = rig.cross();
    rig.come_back(seq, None);
    let seq = rig.cross();
    assert_eq!(
        rig.link.sent_clipboard(),
        [(pc_id(), "one".to_owned())],
        "unchanged, so not sent again"
    );
    assert_eq!(rig.fake.state().clipboard_reads, 1, "and not even read");

    rig.come_back(seq, None);
    rig.fake.copy("two");
    rig.cross();
    assert_eq!(
        rig.link.sent_clipboard(),
        [(pc_id(), "one".to_owned()), (pc_id(), "two".to_owned())]
    );
}

#[test]
fn a_clipboard_that_could_not_be_read_is_tried_again_at_the_next_crossing() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    rig.fake.copy("clipboard-secret-read-later");
    // The app that copied did not answer in time.
    rig.fake.state().fail_clipboard_read = true;
    let seq = rig.cross();
    assert_eq!(rig.link.sent_clipboard(), []);
    rig.come_back(seq, None);

    rig.fake.state().fail_clipboard_read = false;
    rig.cross();
    assert_eq!(
        rig.link.sent_clipboard(),
        [(pc_id(), "clipboard-secret-read-later".to_owned())],
        "nothing was recorded, so it went this time"
    );
    let log = logging::tests::captured_text();
    assert!(
        log.contains("could not read the clipboard; the next crossing tries again"),
        "{log}"
    );
    assert!(!log.contains("clipboard-secret"));
}

#[test]
fn a_clipboard_that_cannot_be_read_is_explained_in_activity_once() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("Unreadable PC", |_| {});
    rig.connect(&EVERYTHING);
    // As the Mac's clipboard is when macOS does not let Hop read it.
    rig.fake.state().fail_clipboard_read = true;
    rig.fake.copy("clipboard-secret-unread");
    let seq = rig.cross();
    rig.come_back(seq, None);
    rig.fake.copy("clipboard-secret-unread-again");
    rig.cross();
    assert_eq!(rig.link.sent_clipboard(), []);
    let log = logging::tests::captured_text();
    let line = "Copied text did not go to Unreadable PC: the fake refuses to read the clipboard.";
    assert_eq!(log.matches(line).count(), 1, "once a run:\n{log}");
    assert!(!log.contains("clipboard-secret"));
}

#[test]
fn text_from_the_other_computer_is_pasted_here_and_never_sent_back() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    rig.fake.copy("copied on the Mac");
    let seq = rig.cross();
    // The cursor comes back with the PC's clipboard right after the Leave.
    rig.come_back(seq, Some(clipboard_from(pc_id(), "copied on the PC")));
    assert_eq!(
        rig.fake.state().clipboard.as_deref(),
        Some("copied on the PC")
    );

    let seq = rig.cross();
    assert_eq!(
        rig.link.sent_clipboard(),
        [(pc_id(), "copied on the Mac".to_owned())],
        "the PC's text is not echoed back"
    );
    rig.come_back(seq, None);
    rig.fake.copy("copied on the Mac again");
    rig.cross();
    assert_eq!(
        rig.link.sent_clipboard().last(),
        Some(&(pc_id(), "copied on the Mac again".to_owned()))
    );
}

#[test]
fn text_that_could_not_be_written_is_written_by_a_later_tick() {
    let _ = logging::tests::capture();
    let mut rig = Rig::pc("Mac", |_| {});
    rig.connect(&EVERYTHING);
    // Another app holds the PC's clipboard as the Mac's text arrives.
    rig.fake.state().fail_clipboard_write = true;
    rig.deliver(clipboard_from(mac_id(), "clipboard-secret-from-the-mac"));
    assert_eq!(rig.fake.state().clipboard, None);
    rig.agent.tick();
    rig.fake.state().fail_clipboard_write = false;
    // The first try again comes 100 ms after the failure.
    thread::sleep(Duration::from_millis(120));
    rig.agent.tick();
    assert_eq!(
        rig.fake.state().clipboard.as_deref(),
        Some("clipboard-secret-from-the-mac")
    );
    // Written once, and never sent back.
    let seq = mac_enters(&mut rig.agent, &rig.link);
    rig.deliver(NetEvent::Message {
        from: mac_id(),
        msg: CoreMessage::Motion {
            seq,
            mark: MotionMark {
                n: 1,
                total_dx: -50.0,
                total_dy: 0.0,
            },
        },
    });
    assert_eq!(rig.link.sent_clipboard(), []);
    let log = logging::tests::captured_text();
    assert!(
        log.contains("clipboard text on this computer's clipboard after all"),
        "{log}"
    );
    assert!(!log.contains("clipboard-secret"));
}

#[test]
fn text_still_waiting_is_not_tried_again_while_a_crossing_waits_for_its_answer() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    let seq = rig.cross();
    // Another app holds the Mac's clipboard as the PC's text comes back.
    rig.fake.state().fail_clipboard_write = true;
    rig.come_back(seq, Some(clipboard_from(pc_id(), "copied on the PC")));
    rig.fake.state().fail_clipboard_write = false;
    // A try again is due 100 ms after the failure, just as the cursor
    // crosses again and the PC has not answered yet: the clipboard is
    // left alone.
    thread::sleep(Duration::from_millis(120));
    let seq = enter_the_pc(&mut rig.agent, &rig.fake, &rig.link);
    let calls = rig.fake.state().clipboard_calls;
    rig.agent.tick();
    assert_eq!(rig.fake.state().clipboard_calls, calls, "not even asked");
    assert_eq!(rig.fake.state().clipboard, None);
    // The first tick after the answer tries, and the text is written.
    rig.deliver(enter_ack(seq));
    assert_eq!(
        rig.agent.state(),
        State::Controlling {
            target: pc_id(),
            seq
        }
    );
    rig.agent.tick();
    assert_eq!(
        rig.fake.state().clipboard.as_deref(),
        Some("copied on the PC")
    );
}

/// On a Mac whose clipboard waits on a slow app, every clipboard call gives
/// up after `CLIPBOARD_TIMEOUT`. With the PC's text still waiting to be
/// written there, crossing again waits for the `Enter`'s own look at the
/// clipboard only, so the PC's answer still comes in time.
#[test]
fn a_clipboard_that_hangs_with_text_waiting_does_not_make_a_crossing_fail() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    let seq = rig.cross();
    rig.fake.state().stall_clipboard = true;
    // Writing the PC's text gives up, so a try again is due by the time
    // the cursor crosses again.
    rig.come_back(seq, Some(clipboard_from(pc_id(), "copied on the PC")));
    let seq = enter_the_pc(&mut rig.agent, &rig.fake, &rig.link);
    let calls = rig.fake.state().clipboard_calls;
    rig.agent.tick();
    assert_eq!(rig.fake.state().clipboard_calls, calls, "no try meanwhile");
    rig.deliver(enter_ack(seq));
    assert_eq!(
        rig.agent.state(),
        State::Controlling {
            target: pc_id(),
            seq
        },
        "the answer came in time"
    );
}

#[test]
fn text_that_arrives_is_on_the_clipboard_before_the_next_key_is_typed() {
    let mut rig = Rig::pc("Mac", |_| {});
    rig.connect(&EVERYTHING);
    // The Mac crosses with its clipboard and types Ctrl+V at once: all of
    // it waits in the queue together.
    rig.link.deliver(NetEvent::Message {
        from: mac_id(),
        msg: CoreMessage::Enter {
            seq: 1,
            entry: EntryPoint {
                display_id: 1,
                edge: Edge::Left,
                position: Point::new(1.0, 400.0),
            },
            held_keys: Vec::new(),
            source_os: Os::MacOs,
        },
    });
    rig.link
        .deliver(clipboard_from(mac_id(), "copied on the Mac"));
    rig.link
        .deliver(key_input(1, 0, KeyCode::LEFT_CONTROL, true));
    rig.link.deliver(key_input(1, 0, V, true));
    let _ = rig.agent.poll();
    let state = rig.fake.state();
    assert_eq!(state.clipboard.as_deref(), Some("copied on the Mac"));
    let written = state
        .calls
        .iter()
        .position(|call| *call == Call::WriteClipboard)
        .expect("written");
    let pasted = state
        .calls
        .iter()
        .position(|call| *call == Call::Inject(key(V, true)))
        .expect("V typed");
    assert!(written < pasted, "{:?}", state.calls);
}

#[test]
fn a_computer_without_clipboard_sync_gets_no_text_and_its_text_is_not_written() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("PC from before M4", |_| {});
    rig.connect(&[caps::INPUT]);
    rig.fake.copy("copied on the Mac");
    let seq = rig.cross();
    assert_eq!(rig.link.sent_clipboard(), []);
    assert_eq!(rig.fake.state().clipboard_reads, 0, "not even read");
    // The network delivers no text without the capability; the agent does
    // not write any either.
    rig.come_back(seq, Some(clipboard_from(pc_id(), "from the PC")));
    // Nor text from a computer Hop does not share with.
    rig.deliver(clipboard_from(
        DeviceId::new("b".repeat(32)),
        "from a stranger",
    ));
    assert_eq!(
        rig.fake.state().clipboard.as_deref(),
        Some("copied on the Mac")
    );
    let log = logging::tests::captured_text();
    assert!(!log.contains("this computer and PC from before M4."));
}

#[test]
fn with_clipboard_sync_off_nothing_is_read_sent_or_written() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("PC with sync off on the Mac", |settings| {
        settings.clipboard = ClipboardSettings::OFF;
    });
    rig.connect(&EVERYTHING);
    rig.fake.copy("copied on the Mac");
    let seq = rig.cross();
    rig.come_back(seq, Some(clipboard_from(pc_id(), "copied on the PC")));
    assert_eq!(rig.link.sent_clipboard(), []);
    let state = rig.fake.state();
    assert_eq!(state.clipboard.as_deref(), Some("copied on the Mac"));
    assert_eq!(state.clipboard_reads, 0);
    assert!(!state.calls.contains(&Call::WriteClipboard));
    let log = logging::tests::captured_text();
    assert!(!log.contains("this computer and PC with sync off on the Mac."));
}

#[test]
fn text_a_closing_connection_did_not_take_goes_at_the_next_crossing() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    rig.fake.copy("one");
    rig.link.net().refuse_clipboard = Some(|| SendError::NotConnected);
    let seq = rig.cross();
    assert_eq!(rig.link.sent_clipboard(), []);
    rig.come_back(seq, None);
    rig.link.net().refuse_clipboard = None;
    rig.cross();
    assert_eq!(rig.link.sent_clipboard(), [(pc_id(), "one".to_owned())]);
}

#[test]
fn text_that_can_never_go_is_not_read_again_until_the_clipboard_changes() {
    let mut rig = Rig::mac("PC", |settings| settings.clipboard.max_text_bytes = 16);
    rig.connect(&EVERYTHING);
    // Over this computer's limit: never cut short, so nothing goes.
    rig.fake.copy("more than sixteen bytes of text");
    let seq = rig.cross();
    rig.come_back(seq, None);
    // Within it, but the network cannot carry it.
    rig.fake.copy("short enough");
    rig.link.net().refuse_clipboard = Some(|| {
        SendError::Encode(ProtoError::ClipboardTooLarge {
            len: 2_000_000,
            max: 1_048_576,
        })
    });
    let seq = rig.cross();
    rig.come_back(seq, None);
    rig.link.net().refuse_clipboard = None;
    rig.cross();
    assert_eq!(rig.link.sent_clipboard(), [], "neither is offered again");
    assert_eq!(rig.fake.state().clipboard_reads, 2, "each was read once");
}

#[test]
fn the_drop_zone_follows_the_edge_toward_the_pc_while_it_is_connected() {
    let mut rig = Rig::mac("Zone PC", |_| {});
    assert_eq!(rig.fake.state().zones, [], "none before the PC connects");
    rig.connect(&EVERYTHING);
    {
        let state = rig.fake.state();
        // The whole right edge but its corners, also beside the PC's lower
        // screen: the cursor crosses there too.
        assert_eq!(
            state.zones,
            [DropZone {
                display: Rect::new(0.0, 0.0, 1512.0, 982.0),
                edge: Edge::Right,
                start: 10.0,
                end: 972.0,
            }]
        );
        assert_eq!(state.label, "Drop to send to Zone PC");
    }

    // A bigger screen moves the zone with the edge.
    rig.fake.state().displays = vec![display(1728.0, 1117.0)];
    rig.agent.next_display_check = Instant::now();
    rig.agent.tick();
    assert_eq!(
        rig.fake.state().zones,
        [DropZone {
            display: Rect::new(0.0, 0.0, 1728.0, 1117.0),
            edge: Edge::Right,
            start: 10.0,
            end: 1107.0,
        }]
    );

    rig.disconnect();
    assert_eq!(rig.fake.state().zones, [], "gone with the connection");
    rig.connect(&EVERYTHING);
    assert_eq!(rig.fake.state().zones.len(), 1, "back with it");
    rig.agent.quit();
    assert_eq!(rig.fake.state().zones, [], "gone when Hop quits");
}

#[test]
fn there_is_no_drop_zone_unless_both_computers_take_files() {
    let mut old = Rig::mac("PC", |_| {});
    old.connect(&[caps::INPUT, caps::CLIPBOARD_TEXT]);
    assert_eq!(old.fake.state().zones, [], "the PC cannot take files");

    let mut off = Rig::mac("PC", |settings| settings.files = false);
    off.connect(&EVERYTHING);
    assert_eq!(off.fake.state().zones, [], "receiving files is off here");
}

#[test]
fn a_computer_without_a_drop_target_says_so_in_the_log_once() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("PC", |_| {});
    rig.fake.state().no_drop_target = true;
    rig.connect(&EVERYTHING);
    rig.disconnect();
    rig.connect(&EVERYTHING);
    let log = logging::tests::captured_text();
    assert_eq!(
        log.matches("this computer has no drop target yet").count(),
        1,
        "{log}"
    );
}

#[test]
fn files_dropped_at_the_edge_go_to_the_pc() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("Drop PC", |_| {});
    rig.connect(&EVERYTHING);
    rig.fake.drop_files(&[
        "/Users/krish/Desktop/file-secret-plans.txt",
        "/Users/krish/Desktop/file-secret-folder",
    ]);
    rig.fake
        .drop_files(&["/Users/krish/Desktop/file-secret-notes.md"]);
    rig.agent.tick();
    assert_eq!(
        rig.link.sent_files(),
        [
            (
                pc_id(),
                vec![
                    PathBuf::from("/Users/krish/Desktop/file-secret-plans.txt"),
                    PathBuf::from("/Users/krish/Desktop/file-secret-folder"),
                ]
            ),
            (
                pc_id(),
                vec![PathBuf::from("/Users/krish/Desktop/file-secret-notes.md")]
            ),
        ]
    );
    let log = logging::tests::captured_text();
    assert!(log.contains("Sending 2 items to Drop PC…"), "{log}");
    assert!(log.contains("Sending 1 item to Drop PC…"), "{log}");
    assert!(!log.contains("file-secret"), "the log never names a file");
}

#[test]
fn files_dropped_while_the_pc_cannot_take_them_are_not_sent() {
    let _ = logging::tests::capture();
    let mut gone = Rig::mac("Gone PC", |_| {});
    gone.connect(&EVERYTHING);
    gone.disconnect();
    gone.fake.drop_files(&["/Users/krish/file-secret-late.txt"]);
    gone.agent.tick();

    let mut old = Rig::mac("Old PC for files", |_| {});
    old.connect(&[caps::INPUT]);
    old.fake.drop_files(&["/Users/krish/file-secret-old.txt"]);
    old.agent.tick();

    assert_eq!(gone.link.sent_files(), []);
    assert_eq!(old.link.sent_files(), []);
    let log = logging::tests::captured_text();
    assert!(
        log.contains("The files you dropped were not sent: Gone PC is not connected."),
        "{log}"
    );
    assert!(
        log.contains(
            "The files you dropped were not sent: Old PC for files cannot receive files (its \
             Hop is older, or receiving files is turned off there)."
        ),
        "{log}"
    );
    assert!(!log.contains("file-secret"));
}

#[test]
fn files_that_arrive_are_shown_and_named_to_the_app_alone() {
    let _ = logging::tests::capture();
    let mut rig = Rig::pc("Sending Mac", |_| {});
    rig.connect(&EVERYTHING);
    let folder = PathBuf::from("Downloads").join("Hop Received");
    let files = vec![
        folder.join("file-secret-report.pdf"),
        folder.join("file-secret-photo (2).jpg"),
    ];
    rig.deliver(NetEvent::Transfer(TransferEvent::Received {
        from: mac_id(),
        id: 7,
        files: files.clone(),
    }));
    assert_eq!(
        rig.updates(),
        [
            connected("Sending Mac", Os::MacOs),
            AgentUpdate::FilesReceived {
                from: "Sending Mac".to_owned(),
                names: vec![
                    "file-secret-report.pdf".to_owned(),
                    "file-secret-photo (2).jpg".to_owned()
                ],
            }
        ]
    );
    assert_eq!(rig.shown(), [files], "shown in their folder");
    let log = logging::tests::captured_text();
    assert!(
        log.contains("Received 2 files from Sending Mac. Their folder opens now."),
        "{log}"
    );
    assert!(!log.contains("file-secret"), "the log never names a file");
}

#[test]
fn sent_and_failed_transfers_become_status_lines_only() {
    let _ = logging::tests::capture();
    let mut rig = Rig::mac("Status PC", |_| {});
    rig.connect(&EVERYTHING);
    for transfer in [
        TransferEvent::Sent {
            to: pc_id(),
            id: 1,
            files: 3,
            skipped: 1,
        },
        TransferEvent::Failed {
            peer: pc_id(),
            id: 2,
            outgoing: true,
            reason: TransferFailure::Refused(Refusal::Busy),
        },
        TransferEvent::Failed {
            peer: pc_id(),
            id: 9,
            outgoing: false,
            reason: TransferFailure::Disconnected,
        },
        // This computer could not use its Received folder: it says so too.
        TransferEvent::Failed {
            peer: pc_id(),
            id: 10,
            outgoing: false,
            reason: TransferFailure::Refused(Refusal::Off),
        },
    ] {
        rig.deliver(NetEvent::Transfer(transfer));
    }
    assert_eq!(rig.updates(), [connected("Status PC", Os::Windows)]);
    assert_eq!(rig.shown(), Vec::<Vec<PathBuf>>::new());
    let log = logging::tests::captured_text();
    for line in [
        "Sent 3 files to Status PC. Left out 1 item: a folder, or a file Hop could not read.",
        "Status PC did not take the files: it is busy with other files. Drop these again once \
         those have arrived.",
        "Not all the files from Status PC arrived: the connection closed. The ones that arrived \
         complete are in the Hop Received folder.",
        "Could not take the files from Status PC: Hop cannot save them in this computer's Hop \
         Received folder. On a Mac, switch on Downloads Folder for Hop in System Settings > \
         Privacy & Security > Files & Folders.",
    ] {
        assert!(log.contains(line), "{line:?} is not in the log:\n{log}");
    }
}

#[test]
fn files_that_arrive_during_a_session_are_reported_apart_from_it() {
    let mut rig = Rig::mac("PC", |_| {});
    rig.connect(&EVERYTHING);
    let seq = rig.cross();
    rig.deliver(NetEvent::Transfer(TransferEvent::Received {
        from: pc_id(),
        id: 1,
        files: vec![
            PathBuf::from("Downloads")
                .join("Hop Received")
                .join("a.txt"),
        ],
    }));
    rig.come_back(seq, None);
    assert_eq!(
        rig.updates(),
        [
            connected("PC", Os::Windows),
            AgentUpdate::Session(Session::Controlling),
            AgentUpdate::FilesReceived {
                from: "PC".to_owned(),
                names: vec!["a.txt".to_owned()],
            },
            AgentUpdate::Session(Session::Idle),
        ]
    );
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
