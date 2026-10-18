//! End-to-end scenarios on the simulated desk (see `sim`): a Mac with a
//! Windows PC to its right. The simulation checks the spec's invariants after
//! every single event, so each scenario checks them too.

mod sim;

use hop_core::{Action, CaptureMode, CoreConfig, EndReason, Event, Layout, Notice, State};
use hop_types::{
    CoreMessage, Edge, EntryPoint, InputEvent, KeyCode, MotionMark, MouseButton, Os, Point,
    RejectReason,
};
use sim::{KEY_A, KEY_C, MAC, PC, Sim, SimConfig, display, key, mac_laptop, pc_screen};

fn controlling(sim: &Sim, seq: u64) -> State {
    State::Controlling {
        target: sim.id(PC),
        seq,
    }
}

fn controlled(sim: &Sim, seq: u64) -> State {
    State::Controlled {
        by: sim.id(MAC),
        seq,
    }
}

fn ended(sim: &Sim, d: usize, reason: EndReason) -> bool {
    sim.dev[d].notices.contains(&Notice::SessionEnded {
        peer: sim.id(1 - d),
        reason,
    })
}

/// The Mac pushes right from the middle of its screen and the PC accepts.
fn cross_mac_to_pc(sim: &mut Sim) {
    sim.motion(MAC, 2000.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(sim, 1));
    assert_eq!(sim.state(PC), controlled(sim, 1));
}

#[test]
fn crossing_carries_a_held_cmd_and_typing_follows() {
    let mut sim = Sim::mac_and_pc();
    sim.key(MAC, KeyCode::LEFT_META, true);
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(
        sim.state(MAC),
        State::Entering {
            target: sim.id(PC),
            seq: 1
        }
    );
    // Cmd is released on the Mac, so no Mac app sees it stuck down.
    assert_eq!(
        sim.dev[MAC].injected_events(),
        vec![(key(KeyCode::LEFT_META, false), Os::MacOs)]
    );
    let enters = sim.dev[MAC].sent_matching(|m| matches!(m, CoreMessage::Enter { .. }));
    let CoreMessage::Enter {
        entry, held_keys, ..
    } = &enters[0]
    else {
        panic!("expected an Enter");
    };
    assert_eq!(held_keys, &vec![KeyCode::LEFT_META]);
    assert_eq!(
        entry.position,
        Point::new(1.0, 432.0),
        "middle maps to middle"
    );

    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert_eq!(sim.cursor(PC), Point::new(1.0, 432.0));

    sim.tap(MAC, KEY_C);
    sim.key(MAC, KeyCode::LEFT_META, false);
    sim.settle();
    // The PC presses Cmd at the crossing, then gets C and the Cmd release,
    // all as Mac key codes (the runtime maps Cmd to Ctrl).
    assert_eq!(
        sim.dev[PC].injected_events(),
        vec![
            (key(KeyCode::LEFT_META, true), Os::MacOs),
            (key(KEY_C, true), Os::MacOs),
            (key(KEY_C, false), Os::MacOs),
            (key(KeyCode::LEFT_META, false), Os::MacOs),
        ]
    );
    // Nothing reached the Mac's own apps while it was controlling.
    assert_eq!(sim.dev[MAC].injected.len(), 1);
}

#[test]
fn input_while_entering_is_buffered_and_flushed_in_order() {
    let mut sim = Sim::mac_and_pc();
    sim.motion(MAC, 2000.0, 0.0);
    sim.tap(MAC, KEY_A);
    sim.motion(MAC, 10.0, 0.0);
    sim.motion(MAC, 10.0, 5.0);
    sim.button(MAC, MouseButton::Left, true);
    sim.button(MAC, MouseButton::Left, false);
    sim.settle();
    let pc = &sim.dev[PC];
    let events: Vec<InputEvent> = pc.injected.iter().map(|i| i.event).collect();
    assert_eq!(
        events,
        vec![
            key(KEY_A, true),
            key(KEY_A, false),
            sim::click(MouseButton::Left, true),
            sim::click(MouseButton::Left, false),
        ]
    );
    // The buffered motion was applied before the click.
    assert_eq!(pc.injected[2].cursor, Point::new(21.0, 437.0));
}

#[test]
fn pushing_past_the_pc_left_edge_returns_to_the_mac() {
    let mut sim = Sim::mac_and_pc();
    sim.key(MAC, KeyCode::LEFT_SHIFT, true);
    cross_mac_to_pc(&mut sim);
    sim.motion(MAC, 300.0, 0.0);
    sim.settle();
    assert_eq!(sim.cursor(PC), Point::new(301.0, 432.0));

    sim.motion(MAC, -200.0, 0.0);
    sim.motion(MAC, -200.0, 0.0);
    // One latency later the PC has decided the cursor left and released
    // what it injected; its Leave is still on the way to the Mac.
    sim.advance(5);
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    let leaves = sim.dev[PC].sent_matching(|m| matches!(m, CoreMessage::Leave { .. }));
    assert_eq!(leaves.len(), 1);

    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    // PC y=432 of 864 maps back to Mac y=491 of 982, one unit inside.
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    // Shift is still physically held, so the Mac presses it again locally.
    assert_eq!(
        sim.dev[MAC].injected_events().last(),
        Some(&(key(KeyCode::LEFT_SHIFT, true), Os::MacOs))
    );
    assert!(ended(&sim, MAC, EndReason::CursorLeft));
    assert!(ended(&sim, PC, EndReason::CursorLeft));

    // Crossing again works right away and starts a new session.
    sim.motion(MAC, 5.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 3));
}

/// Coming home with Shift held, then letting go of Shift and returning. The
/// Mac's injector pressed Shift again for local apps, and must not still hold
/// it afterwards.
fn come_home_holding_shift(sim: &mut Sim) {
    sim.key(MAC, KeyCode::LEFT_SHIFT, true);
    cross_mac_to_pc(sim);
    sim.motion(MAC, -2000.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(sim.dev[MAC].injector_keys.contains(&KeyCode::LEFT_SHIFT));
}

/// Regression: the physical key-up of a modifier pressed again on the way
/// home went straight to local apps and never reached the injector, which
/// kept the modifier down and added it to the next session's keys.
#[test]
fn letting_go_of_a_modifier_pressed_again_at_home_releases_the_injector_too() {
    let mut sim = Sim::mac_and_pc();
    come_home_holding_shift(&mut sim);

    sim.key(MAC, KeyCode::LEFT_SHIFT, false);
    assert!(
        sim.dev[MAC].injector_keys.is_empty(),
        "the injector still holds {:?}",
        sim.dev[MAC].injector_keys
    );
    assert_eq!(
        sim.dev[MAC].injected_events().last(),
        Some(&(key(KeyCode::LEFT_SHIFT, false), Os::MacOs))
    );
}

/// The second line of defence: a device that becomes controlled first
/// releases whatever its injector still holds, so nothing left over from an
/// earlier session reaches the new one.
#[test]
fn becoming_controlled_first_releases_anything_still_injected() {
    let mut sim = Sim::mac_and_pc();
    come_home_holding_shift(&mut sim);

    let releases_before = sim.dev[MAC].release_all;
    sim.motion(PC, -4000.0, 0.0);
    sim.settle();
    assert!(
        matches!(sim.state(MAC), State::Controlled { .. }),
        "the PC should now control the Mac, but the Mac is {:?}",
        sim.state(MAC)
    );
    assert_eq!(sim.dev[MAC].release_all, releases_before + 1);
    assert!(sim.dev[MAC].injector_keys.is_empty());
}

#[test]
fn moving_the_pc_trackpad_takes_control_back() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KEY_A, true);
    sim.settle();
    assert!(sim.dev[PC].remote_keys.contains(&KEY_A));

    // Small jitter under the 4-unit threshold is tolerated...
    sim.motion(PC, 2.0, 0.0);
    sim.motion(PC, 0.0, 1.5);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    // ... but real motion takes control back, releasing the held A.
    sim.motion(PC, 1.0, 0.0);
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    assert!(sim.dev[PC].remote_keys.is_empty());
    assert_eq!(
        sim.dev[PC].sent.last().map(|(_, m)| m.clone()),
        Some(CoreMessage::TakeBack { seq: 1 })
    );
    // The PC's own trackpad moved its cursor normally.
    assert_eq!(sim.cursor(PC), Point::new(4.0, 433.5));

    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(
        sim.cursor(MAC),
        Point::new(1511.0, 491.0),
        "back at the exit point"
    );
    assert!(ended(&sim, MAC, EndReason::TakenBack));
}

#[test]
fn a_leave_that_arrives_after_a_take_back_is_ignored() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.motion(PC, 5.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    // A Leave of the session the take-back ended (only a broken peer sends
    // one) neither moves the cursor nor starts anything; the simulation
    // checks it acts exactly like a tick.
    let leave = CoreMessage::Leave {
        seq: 1,
        to: sim.id(MAC),
        entry: EntryPoint {
            display_id: 1,
            edge: Edge::Right,
            position: Point::new(1511.0, 100.0),
        },
    };
    sim.deliver(MAC, leave);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
}

#[test]
fn a_duplicate_enter_ack_changes_nothing() {
    let mut sim = Sim::mac_and_pc();
    sim.motion(MAC, 2000.0, 0.0);
    // Typed while waiting for the ack, so it sits in the buffer.
    sim.tap(MAC, KEY_A);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    // A second ack must not flush the buffer again or restart the session.
    let mut twin = sim.dev[MAC].core.clone();
    let expected = twin.handle(sim.now, Event::Tick);
    let actions = sim.deliver(MAC, CoreMessage::EnterAck { seq: 1 });
    assert_eq!(actions, expected);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(
        sim.dev[PC].injected_events(),
        vec![
            (key(KEY_A, true), Os::MacOs),
            (key(KEY_A, false), Os::MacOs)
        ]
    );
}

#[test]
fn a_key_press_on_the_pc_takes_control_back() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    // Releases and scrolling are not take-back input.
    sim.key(PC, KEY_A, false);
    sim.scroll(PC, 1.0);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    sim.key(PC, KEY_A, true);
    assert_eq!(sim.state(PC), State::Idle);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
}

#[test]
fn simultaneous_crossing_lower_id_keeps_control() {
    let mut sim = Sim::mac_and_pc();
    sim.motion(MAC, 2000.0, 0.0);
    sim.motion(PC, -2000.0, 0.0);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    assert!(matches!(sim.state(PC), State::Entering { .. }));
    sim.settle();
    // "mac" < "pc": the Mac wins, the PC aborts its own crossing and accepts.
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert!(ended(&sim, PC, EndReason::LostTieBreak));
    let rejects = sim.dev[MAC].sent_matching(|m| matches!(m, CoreMessage::EnterReject { .. }));
    assert_eq!(rejects.len(), 1, "the winner rejects the loser's Enter");
    // The loser also releases its own crossing (session 2), in case the
    // winner had accepted it; here the winner ignores that as stale.
    let releases = sim.dev[PC].sent_matching(|m| matches!(m, CoreMessage::Release { .. }));
    assert_eq!(releases, vec![CoreMessage::Release { seq: 2 }]);
    // The late reject of the PC's own Enter changed nothing either.
    sim.advance(1000);
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.state(PC), controlled(&sim, 1));
}

#[test]
fn simultaneous_crossing_is_decided_by_id_not_by_os() {
    let mut sim = Sim::new(SimConfig {
        mac_id: "zz-mac",
        pc_id: "aa-pc",
        ..SimConfig::default()
    });
    sim.motion(MAC, 2000.0, 0.0);
    sim.motion(PC, -2000.0, 0.0);
    sim.settle();
    assert_eq!(
        sim.state(PC),
        State::Controlling {
            target: sim.id(MAC),
            seq: 1
        }
    );
    assert_eq!(
        sim.state(MAC),
        State::Controlled {
            by: sim.id(PC),
            seq: 1
        }
    );
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
}

#[test]
fn a_tie_break_loser_whose_rival_enter_was_lost_is_told_busy() {
    let mut sim = Sim::mac_and_pc();
    // The Mac's Enter is lost on the way...
    sim.net.blackhole[PC] = true;
    sim.motion(MAC, 2000.0, 0.0);
    sim.net.blackhole[PC] = false;
    // ... so the PC, crossing at the same moment, only hears the Mac's
    // answer to its own Enter: Busy, because the Mac's lower id wins.
    sim.motion(PC, -2000.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(sim.dev[PC].capture, CaptureMode::Passive);
    assert_eq!(sim.cursor(PC), Point::new(0.0, 432.0), "back where it left");
    assert!(ended(&sim, PC, EndReason::Rejected(RejectReason::Busy)));
    // The Mac never gets an answer to its Enter and gives up in time.
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    sim.advance(500);
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::EnterTimeout));
}

#[test]
fn a_crossing_given_up_during_a_tie_break_leaves_both_idle() {
    let mut sim = Sim::mac_and_pc();
    sim.motion(MAC, 2000.0, 0.0);
    sim.motion(PC, -2000.0, 0.0);
    // The Mac gives up before either Enter arrives. Being idle, it then
    // accepts the PC's Enter, while the PC, losing the tie-break, accepts
    // the Mac's. Each side's Release then ends what the other accepted.
    sim.chord(MAC);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::EmergencyChord));
    assert!(ended(&sim, PC, EndReason::LostTieBreak));
    assert!(ended(&sim, PC, EndReason::Released));
    assert!(ended(&sim, MAC, EndReason::Released));
}

#[test]
fn ghost_sessions_after_lost_messages_time_out() {
    let mut sim = Sim::mac_and_pc();
    // Both cross at once: the Mac starts session 1, the PC session 2.
    sim.motion(MAC, 2000.0, 0.0);
    sim.motion(PC, -2000.0, 0.0);
    // Then the Wi-Fi loses every message, and the Mac gives up with the
    // emergency chord; its Release is lost.
    sim.net.blackhole = [true, true];
    sim.chord(MAC);
    // The two Enters were already in the air. The idle Mac accepts the PC's;
    // the PC, losing the tie-break, accepts the Mac's; every answer is lost.
    sim.advance(5);
    let mac_ghost = State::Controlled {
        by: sim.id(PC),
        seq: 2,
    };
    assert_eq!(sim.state(MAC), mac_ghost);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    // Nobody drives either cursor. Once the Wi-Fi is back each side hears
    // the other's heartbeats, but they belong to the other session, so they
    // keep nothing alive.
    sim.net.blackhole = [false, false];
    sim.advance(1400);
    assert_eq!(sim.state(MAC), mac_ghost);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    for d in [MAC, PC] {
        let heartbeats = sim.dev[d].sent_matching(|m| matches!(m, CoreMessage::Heartbeat { .. }));
        assert!(heartbeats.len() >= 4, "{heartbeats:?}");
    }
    sim.advance(200);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::PeerTimeout));
    assert!(ended(&sim, PC, EndReason::PeerTimeout));
}

#[test]
fn an_ack_that_loses_the_race_with_the_enter_timeout_is_undone() {
    // 300 ms each way: the PC accepts, but its ack reaches the Mac 600 ms
    // after the crossing, 100 ms after the Mac gave up.
    let mut sim = Sim::new(SimConfig {
        latency: (300, 300),
        ..SimConfig::default()
    });
    sim.key(MAC, KeyCode::LEFT_SHIFT, true);
    sim.motion(MAC, 2000.0, 0.0);
    sim.advance(300);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert!(sim.dev[PC].remote_keys.contains(&KeyCode::LEFT_SHIFT));
    sim.advance(200);
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::EnterTimeout));
    // The late ack is stale for the Mac, and the Mac's Release, sent when
    // it gave up, frees the PC and the Shift it pressed.
    sim.advance(300);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, PC, EndReason::Released));
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    assert!(sim.dev[PC].remote_keys.is_empty());
}

#[test]
fn enter_times_out_when_the_peer_never_answers() {
    let mut sim = Sim::mac_and_pc();
    sim.net.blackhole[PC] = true;
    sim.motion(MAC, 2000.0, 0.0);
    sim.advance(499);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    sim.advance(1);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    assert!(ended(&sim, MAC, EndReason::EnterTimeout));
    // A Release goes out in case the Enter arrives late.
    assert_eq!(
        sim.dev[MAC].sent.last().map(|(_, m)| m.clone()),
        Some(CoreMessage::Release { seq: 1 })
    );

    // Pushing on does not retry at once; the edge re-arms when the cursor
    // stops pushing against it.
    sim.net.blackhole[PC] = false;
    sim.motion(MAC, 20.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    sim.motion(MAC, -3.0, 0.0);
    sim.motion(MAC, 20.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 3));
}

#[test]
fn a_rejected_crossing_restores_the_cursor() {
    let mut sim = Sim::mac_and_pc();
    // The PC never sees the Enter; answer for it by hand.
    sim.net.blackhole[PC] = true;
    sim.motion(MAC, 2000.0, 0.0);
    let reject = CoreMessage::EnterReject {
        seq: 1,
        reason: RejectReason::Unavailable,
    };
    sim.deliver(MAC, reject);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    assert!(ended(
        &sim,
        MAC,
        EndReason::Rejected(RejectReason::Unavailable)
    ));
}

#[test]
fn a_device_without_displays_refuses_to_be_entered() {
    let mut sim = Sim::mac_and_pc();
    // The PC's agent reports no displays for a moment, say while its
    // screens are being rearranged.
    let layout = Layout::two_devices(sim.id(PC), vec![], sim.id(MAC), Edge::Left, vec![])
        .expect("valid layout");
    sim.handle(PC, Event::LayoutChanged(layout));
    sim.motion(MAC, 2000.0, 0.0);
    sim.settle();
    let rejects = sim.dev[PC].sent_matching(|m| matches!(m, CoreMessage::EnterReject { .. }));
    assert_eq!(
        rejects,
        vec![CoreMessage::EnterReject {
            seq: 1,
            reason: RejectReason::Unavailable
        }]
    );
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    assert!(ended(
        &sim,
        MAC,
        EndReason::Rejected(RejectReason::Unavailable)
    ));
}

#[test]
fn typing_too_much_while_entering_gives_up_the_crossing() {
    let mut sim = Sim::mac_and_pc();
    sim.net.blackhole[PC] = true;
    sim.motion(MAC, 2000.0, 0.0);
    // 64 key events fill the buffer of a crossing that is still waiting...
    for _ in 0..32 {
        sim.tap(MAC, KEY_A);
    }
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    // ... and the next one gives the crossing up: dropping only part of the
    // input could lose a key-up and leave a key stuck down on the PC.
    sim.key(MAC, KEY_A, true);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    assert!(ended(&sim, MAC, EndReason::BufferFull));
    assert_eq!(
        sim.dev[MAC].sent.last().map(|(_, m)| m.clone()),
        Some(CoreMessage::Release { seq: 1 })
    );
}

#[test]
fn an_enter_from_the_controlled_device_supersedes_the_session() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    let enter = |seq, edge, position, source_os| CoreMessage::Enter {
        seq,
        entry: EntryPoint {
            display_id: 1,
            edge,
            position,
        },
        held_keys: vec![],
        source_os,
    };
    // A copy of the Enter that started the session changes nothing.
    let copy = enter(1, Edge::Left, Point::new(1.0, 432.0), Os::MacOs);
    sim.deliver(PC, copy);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    // The PC sends Enter only while idle, so the session the Mac thinks it
    // has with it is over: the Mac fails open and accepts the new one.
    let actions = sim.deliver(
        MAC,
        enter(2, Edge::Right, Point::new(1511.0, 100.0), Os::Windows),
    );
    assert!(actions.contains(&Action::Send {
        to: sim.id(PC),
        msg: CoreMessage::EnterAck { seq: 2 }
    }));
    assert_eq!(
        sim.state(MAC),
        State::Controlled {
            by: sim.id(PC),
            seq: 2
        }
    );
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert!(ended(&sim, MAC, EndReason::Superseded));
}

#[test]
fn a_new_crossing_supersedes_a_session_whose_release_was_lost() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KEY_A, true);
    sim.settle();
    // The emergency chord ends the session, but its Release is lost, so the
    // PC still believes the Mac controls it and still holds A down.
    sim.net.blackhole[PC] = true;
    sim.chord(MAC);
    sim.net.blackhole[PC] = false;
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert!(sim.dev[PC].remote_keys.contains(&KEY_A));
    // Crossing again starts session 3, which replaces the stale one.
    sim.motion(MAC, -3.0, 0.0);
    sim.motion(MAC, 20.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 3));
    assert_eq!(sim.state(PC), controlled(&sim, 3));
    assert!(ended(&sim, PC, EndReason::Superseded));
    assert_eq!(
        sim.dev[PC].release_all, 3,
        "as session 1 began, as it was superseded, and as session 3 began"
    );
    assert!(sim.dev[PC].remote_keys.is_empty());
}

#[test]
fn peer_timeout_mid_session_frees_both_sides() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KEY_A, true);
    sim.advance(400);
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    // Heartbeats keep an idle session alive.
    sim.advance(5000);
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.state(PC), controlled(&sim, 1));

    // The Wi-Fi goes quiet in both directions without a disconnect.
    sim.net.blackhole = [true, true];
    let t = sim.now;
    sim.advance(1200);
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    sim.advance(310);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert!(sim.now - t <= 1510);
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    assert!(ended(&sim, MAC, EndReason::PeerTimeout));
    assert!(ended(&sim, PC, EndReason::PeerTimeout));
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
}

#[test]
fn heartbeats_flow_only_while_nothing_else_does() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    let heartbeats = |sim: &Sim, d: usize| {
        sim.dev[d]
            .sent_matching(|m| matches!(m, CoreMessage::Heartbeat { .. }))
            .len()
    };
    // A quiet second: one heartbeat each way every 250 ms.
    let (mac, pc) = (heartbeats(&sim, MAC), heartbeats(&sim, PC));
    sim.advance(1000);
    assert_eq!(heartbeats(&sim, MAC) - mac, 4);
    assert_eq!(heartbeats(&sim, PC) - pc, 4);
    // Typing keeps the controller's side busy, so it sends none.
    let mac = heartbeats(&sim, MAC);
    for _ in 0..10 {
        sim.tap(MAC, KEY_A);
        sim.advance(100);
    }
    assert_eq!(heartbeats(&sim, MAC), mac);
    // Pointer motion alone does not count: a lost datagram proves nothing.
    let mac = heartbeats(&sim, MAC);
    for _ in 0..10 {
        sim.motion(MAC, 1.0, 0.0);
        sim.advance(100);
    }
    assert_eq!(heartbeats(&sim, MAC) - mac, 4);
}

#[test]
fn disconnect_mid_session_frees_both_sides() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.button(MAC, MouseButton::Left, true);
    sim.settle();
    assert!(sim.dev[PC].remote_buttons.contains(&MouseButton::Left));
    sim.disconnect();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert!(ended(&sim, MAC, EndReason::Disconnected));
    assert!(ended(&sim, PC, EndReason::Disconnected));

    // No crossing while disconnected; the button is still held anyway.
    sim.button(MAC, MouseButton::Left, false);
    sim.motion(MAC, 50.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    sim.reconnect();
    sim.motion(MAC, 50.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 3));
}

#[test]
fn emergency_chord_ends_the_session_and_escape_is_not_forwarded() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    for code in [
        KeyCode::LEFT_CONTROL,
        KeyCode::LEFT_ALT,
        KeyCode::LEFT_SHIFT,
    ] {
        sim.key(MAC, code, true);
    }
    sim.key(MAC, KeyCode::ESCAPE, true);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert_eq!(
        sim.dev[MAC].sent.last().map(|(_, m)| m.clone()),
        Some(CoreMessage::Release { seq: 1 })
    );
    assert!(ended(&sim, MAC, EndReason::EmergencyChord));
    sim.settle();
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    let escape_seen = sim.dev[PC]
        .injected
        .iter()
        .any(|i| matches!(i.event, InputEvent::Key { code, .. } if code == KeyCode::ESCAPE));
    assert!(!escape_seen);
    assert!(ended(&sim, PC, EndReason::Released));

    // Still pushing right does not cross straight back.
    sim.motion(MAC, 30.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
}

#[test]
fn emergency_chord_works_while_entering() {
    let mut sim = Sim::mac_and_pc();
    sim.net.blackhole[PC] = true;
    for code in [
        KeyCode::LEFT_CONTROL,
        KeyCode::LEFT_ALT,
        KeyCode::LEFT_SHIFT,
    ] {
        sim.key(MAC, code, true);
    }
    sim.motion(MAC, 2000.0, 0.0);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    sim.key(MAC, KeyCode::ESCAPE, true);
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::EmergencyChord));
}

// ----- stalls (`LocalStall`, docs/M2-DESIGN.md) ------------------------------

/// The last `Enter` the Mac sent: its held keys.
fn last_enter_held_keys(sim: &Sim) -> Vec<KeyCode> {
    let enters = sim.dev[MAC].sent_matching(|m| matches!(m, CoreMessage::Enter { .. }));
    match enters.last() {
        Some(CoreMessage::Enter { held_keys, .. }) => held_keys.clone(),
        _ => panic!("the Mac sent no Enter"),
    }
}

#[test]
fn a_stall_while_controlling_gives_input_back_and_frees_the_pc() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KeyCode::LEFT_META, true);
    sim.key(MAC, KEY_A, true);
    sim.settle();
    assert!(sim.dev[PC].remote_keys.contains(&KEY_A));

    // The agent finds the Mac's suppression lease lapsed.
    let actions = sim.stall(MAC);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.dev[MAC].capture, CaptureMode::Passive);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0), "the exit point");
    assert!(actions.contains(&Action::Send {
        to: sim.id(PC),
        msg: CoreMessage::Release { seq: 1 }
    }));
    assert!(ended(&sim, MAC, EndReason::Stalled));
    // Nothing was injected for the Mac's own apps: the keys typed while
    // controlling never reached them.
    assert!(sim.dev[MAC].injected.is_empty());

    sim.settle();
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, PC, EndReason::Released));
    assert_eq!(
        sim.dev[PC].release_all, 2,
        "once as the session began, once as it ended"
    );
    assert!(sim.dev[PC].remote_keys.is_empty());

    // The edge is disarmed: pushing on does not cross straight back.
    sim.motion(MAC, 30.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
}

#[test]
fn a_stall_while_entering_gives_the_crossing_up_and_frees_the_pc() {
    let mut sim = Sim::mac_and_pc();
    sim.motion(MAC, 2000.0, 0.0);
    // Typed while waiting for the ack, so it sits in the buffer.
    sim.tap(MAC, KEY_A);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    sim.stall(MAC);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    assert!(ended(&sim, MAC, EndReason::Stalled));
    // The PC accepts the Enter that was already on its way; the Release
    // right behind it ends that session again, and its late ack is stale.
    sim.settle();
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, PC, EndReason::Released));
    assert_eq!(sim.state(MAC), State::Idle);
    // The buffered A was dropped with the crossing, never sent.
    assert!(
        sim.dev[MAC]
            .sent_matching(|m| matches!(m, CoreMessage::Input { .. }))
            .is_empty()
    );
    assert!(sim.dev[PC].injected.is_empty());
}

#[test]
fn a_stall_while_controlled_takes_control_back() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KEY_A, true);
    sim.settle();
    assert!(sim.dev[PC].remote_keys.contains(&KEY_A));

    // An injection fails on the PC, say, and its agent reports it.
    let actions = sim.stall(PC);
    assert_eq!(sim.state(PC), State::Idle);
    assert!(actions.contains(&Action::ReleaseAllInjected));
    assert!(sim.dev[PC].remote_keys.is_empty());
    assert_eq!(
        sim.dev[PC].sent.last().map(|(_, m)| m.clone()),
        Some(CoreMessage::TakeBack { seq: 1 })
    );
    assert!(ended(&sim, PC, EndReason::Stalled));

    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::TakenBack));
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0), "the exit point");
}

#[test]
fn a_stall_with_nothing_going_on_changes_nothing() {
    let mut sim = Sim::mac_and_pc();
    // The simulation also checks that each acts exactly like a tick.
    assert_eq!(sim.stall(MAC), vec![]);
    assert_eq!(sim.stall(MAC), vec![]);
    cross_mac_to_pc(&mut sim);
}

/// The reason the core stops vouching for held keys: a key-up missed with
/// the stall must not leave a modifier stuck down on the peer.
#[test]
fn a_key_up_lost_before_a_stall_does_not_stick_on_the_pc() {
    let mut sim = Sim::mac_and_pc();
    sim.key(MAC, KeyCode::LEFT_META, true);
    // The user lets go of Cmd, but the agent's input queue is full, so the
    // core never hears of it; the agent reports the loss.
    sim.lose_key_up(MAC, KeyCode::LEFT_META);
    sim.stall(MAC);
    cross_mac_to_pc(&mut sim);
    assert_eq!(last_enter_held_keys(&sim), vec![]);
    assert!(sim.dev[PC].remote_keys.is_empty());
}

/// Lost, never stuck: a key the user keeps holding through a stall does not
/// follow the cursor until it is pressed again.
#[test]
fn a_modifier_held_through_a_stall_is_not_pressed_on_the_pc() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KeyCode::LEFT_META, true);
    sim.settle();
    sim.stall(MAC);
    sim.settle();
    // Back across, still holding Cmd.
    sim.motion(MAC, -3.0, 0.0);
    sim.motion(MAC, 20.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 3));
    assert_eq!(last_enter_held_keys(&sim), vec![]);
    assert!(sim.dev[PC].remote_keys.is_empty());
    // Letting go is forwarded as a harmless key-up, and pressing Cmd again
    // works as usual.
    sim.key(MAC, KeyCode::LEFT_META, false);
    sim.key(MAC, KeyCode::LEFT_META, true);
    sim.settle();
    assert!(sim.dev[PC].remote_keys.contains(&KeyCode::LEFT_META));
}

/// Forgetting a key outright would leave it stuck down on the Mac: no
/// key-up at the crossing, and then capture swallows the real one.
#[test]
fn a_key_held_through_a_stall_is_released_locally_at_the_next_crossing() {
    let mut sim = Sim::mac_and_pc();
    sim.key(MAC, KeyCode::LEFT_SHIFT, true);
    assert_eq!(sim.stall(MAC), vec![], "the stall leaves local apps alone");
    assert!(sim.dev[MAC].os_keys.contains(&KeyCode::LEFT_SHIFT));
    cross_mac_to_pc(&mut sim);
    assert_eq!(
        sim.dev[MAC].injected_events(),
        vec![(key(KeyCode::LEFT_SHIFT, false), Os::MacOs)]
    );
    // Capture swallows the real key-up, and no Mac app sees Shift stuck down
    // (the simulation checks that after every event).
    sim.key(MAC, KeyCode::LEFT_SHIFT, false);
    sim.settle();
    assert!(sim.dev[MAC].os_keys.is_empty());
}

#[test]
fn a_stall_releases_a_modifier_pressed_again_at_home() {
    let mut sim = Sim::mac_and_pc();
    come_home_holding_shift(&mut sim);
    // Its physical key-up may be what the stall missed, so the injector's
    // copy goes at once.
    let actions = sim.stall(MAC);
    assert_eq!(
        actions,
        vec![Action::Inject {
            event: key(KeyCode::LEFT_SHIFT, false),
            source_os: Os::MacOs
        }]
    );
    assert!(sim.dev[MAC].injector_keys.is_empty());
    // Letting go of Shift afterwards injects nothing more.
    let injected = sim.dev[MAC].injected.len();
    sim.key(MAC, KeyCode::LEFT_SHIFT, false);
    assert_eq!(sim.dev[MAC].injected.len(), injected);
}

#[test]
fn the_emergency_chord_works_with_its_keys_held_through_a_stall() {
    let mut sim = Sim::mac_and_pc();
    for code in [
        KeyCode::LEFT_CONTROL,
        KeyCode::LEFT_ALT,
        KeyCode::LEFT_SHIFT,
    ] {
        sim.key(MAC, code, true);
    }
    sim.stall(MAC);
    cross_mac_to_pc(&mut sim);
    sim.key(MAC, KeyCode::ESCAPE, true);
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::EmergencyChord));
}

#[test]
fn a_button_held_through_a_stall_still_blocks_crossing() {
    let mut sim = Sim::mac_and_pc();
    sim.button(MAC, MouseButton::Left, true);
    sim.stall(MAC);
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(
        sim.state(MAC),
        State::Idle,
        "no crossing in the middle of a drag"
    );
    sim.button(MAC, MouseButton::Left, false);
    sim.motion(MAC, 10.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
}

#[test]
fn stale_session_messages_change_nothing() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    let pc_cursor = sim.cursor(PC);
    let mark = MotionMark {
        n: 9,
        total_dx: 300.0,
        total_dy: 0.0,
    };
    let stale_for_pc = [
        CoreMessage::Motion { seq: 0, mark },
        CoreMessage::Input {
            seq: 2,
            mark,
            events: vec![key(KEY_A, true)],
        },
        CoreMessage::Release { seq: 7 },
        CoreMessage::Heartbeat { seq: 3 },
        // Right seq, wrong direction: a controller-bound message.
        CoreMessage::TakeBack { seq: 1 },
    ];
    for msg in stale_for_pc {
        let actions = sim.deliver(PC, msg);
        assert!(actions.is_empty(), "{actions:?}");
    }
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert_eq!(sim.cursor(PC), pc_cursor);
    assert!(sim.dev[PC].injected.is_empty());

    let entry = hop_types::EntryPoint {
        display_id: 1,
        edge: hop_types::Edge::Right,
        position: Point::new(1511.0, 10.0),
    };
    let stale_for_mac = [
        CoreMessage::TakeBack { seq: 9 },
        CoreMessage::Leave {
            seq: 0,
            to: sim.id(MAC),
            entry,
        },
        CoreMessage::EnterReject {
            seq: 4,
            reason: hop_types::RejectReason::Busy,
        },
        // Right seq, wrong direction: a controlled-bound message.
        CoreMessage::Release { seq: 1 },
    ];
    for msg in stale_for_mac {
        let actions = sim.deliver(MAC, msg);
        assert!(actions.is_empty(), "{actions:?}");
    }
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
}

#[test]
fn stale_heartbeats_do_not_keep_a_session_alive() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.net.blackhole = [true, true];
    for _ in 0..10 {
        sim.advance(200);
        sim.deliver(PC, CoreMessage::Heartbeat { seq: 99 });
    }
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, PC, EndReason::PeerTimeout));
}

#[test]
fn messages_from_an_earlier_session_are_ignored() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.motion(MAC, -2000.0, 0.0);
    sim.settle();
    sim.motion(MAC, 10.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(PC), controlled(&sim, 3));
    let late = CoreMessage::Input {
        seq: 1,
        mark: MotionMark::default(),
        events: vec![key(KEY_A, true)],
    };
    assert!(sim.deliver(PC, late).is_empty());
    assert!(sim.deliver(PC, CoreMessage::Release { seq: 1 }).is_empty());
    assert_eq!(sim.state(PC), controlled(&sim, 3));
}

#[test]
fn corner_dead_zone_blocks_a_crossing() {
    let mut sim = Sim::mac_and_pc();
    // To (1400, 5): 5 units from the top corner, inside the 10-unit zone.
    sim.motion(MAC, 644.0, -486.0);
    assert_eq!(sim.cursor(MAC), Point::new(1400.0, 5.0));
    sim.motion(MAC, 500.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 5.0), "the OS clamps it");
    // Near the bottom corner too.
    sim.motion(MAC, 0.0, 971.0);
    sim.motion(MAC, 50.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    // Exactly at the zone's border is allowed.
    sim.motion(MAC, -100.0, -966.0);
    assert_eq!(sim.cursor(MAC), Point::new(1411.0, 10.0));
    sim.motion(MAC, 500.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.cursor(PC), Point::new(1.0, 10.0 / 982.0 * 864.0));
}

#[test]
fn corner_dead_zone_blocks_leaving_the_pc() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.motion(MAC, 100.0, -429.0);
    sim.settle();
    assert_eq!(sim.cursor(PC), Point::new(101.0, 3.0));
    sim.motion(MAC, -500.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert_eq!(sim.cursor(PC), Point::new(0.0, 3.0), "clamped instead");
}

#[test]
fn a_held_button_blocks_crossing_from_the_mac() {
    let mut sim = Sim::mac_and_pc();
    sim.button(MAC, MouseButton::Left, true);
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    sim.button(MAC, MouseButton::Left, false);
    sim.motion(MAC, 10.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
}

#[test]
fn a_held_injected_button_blocks_leaving_the_pc() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    sim.button(MAC, MouseButton::Left, true);
    sim.motion(MAC, -500.0, 0.0);
    sim.settle();
    // Dragging on the PC stops at its edge.
    assert_eq!(sim.state(PC), controlled(&sim, 1));
    assert_eq!(sim.cursor(PC), Point::new(0.0, 432.0));
    sim.button(MAC, MouseButton::Left, false);
    sim.motion(MAC, -10.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(sim.state(MAC), State::Idle);
}

#[test]
fn edge_delay_requires_a_sustained_push() {
    let mut sim = Sim::with_core(CoreConfig {
        edge_delay_ms: 200,
        ..CoreConfig::default()
    });
    sim.motion(MAC, 2000.0, 0.0);
    sim.advance(100);
    sim.motion(MAC, 5.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    sim.advance(99);
    sim.motion(MAC, 5.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    sim.advance(1);
    sim.motion(MAC, 5.0, 0.0);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
}

#[test]
fn edge_delay_restarts_when_the_push_stops() {
    let mut sim = Sim::with_core(CoreConfig {
        edge_delay_ms: 200,
        ..CoreConfig::default()
    });
    sim.motion(MAC, 2000.0, 0.0);
    sim.advance(150);
    sim.motion(MAC, -5.0, 0.0);
    sim.advance(100);
    sim.motion(MAC, 10.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle, "the push restarted at t=250");
    sim.advance(150);
    sim.motion(MAC, 10.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    sim.advance(50);
    sim.motion(MAC, 10.0, 0.0);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
}

#[test]
fn lost_motion_does_not_misplace_a_click() {
    let mut sim = Sim::new(SimConfig {
        drop_motion: 1.0,
        ..SimConfig::default()
    });
    cross_mac_to_pc(&mut sim);
    sim.motion(MAC, 100.0, 0.0);
    sim.motion(MAC, 0.0, 50.0);
    sim.motion(MAC, -30.0, 20.0);
    sim.settle();
    assert!(sim.net.lost_motion >= 3);
    assert_eq!(sim.cursor(PC), Point::new(1.0, 432.0), "no motion arrived");
    sim.button(MAC, MouseButton::Left, true);
    sim.settle();
    let press = &sim.dev[PC].injected[0];
    assert_eq!(press.event, sim::click(MouseButton::Left, true));
    assert_eq!(press.cursor, Point::new(71.0, 502.0), "caught up first");
}

#[test]
fn reordered_motion_is_applied_once() {
    let mut sim = Sim::new(SimConfig {
        latency: (1, 40),
        seed: 7,
        ..SimConfig::default()
    });
    cross_mac_to_pc(&mut sim);
    for _ in 0..20 {
        sim.motion(MAC, 10.0, 5.0);
        sim.advance(2);
    }
    sim.settle();
    assert_eq!(sim.cursor(PC), Point::new(201.0, 532.0));
}

#[test]
fn pointer_speed_scales_motion_on_the_controlled_side() {
    let mut sim = Sim::with_core(CoreConfig {
        pointer_speed: 2.0,
        ..CoreConfig::default()
    });
    cross_mac_to_pc(&mut sim);
    sim.motion(MAC, 100.0, 50.0);
    sim.settle();
    assert_eq!(sim.cursor(PC), Point::new(201.0, 532.0));
}

#[test]
fn proportional_mapping_at_the_corners() {
    let mut sim = Sim::with_core(CoreConfig {
        corner_dead_zone: 0.0,
        ..CoreConfig::default()
    });
    // Top corner: Mac y=0 lands on PC y=0.
    sim.motion(MAC, 644.0, -491.0);
    assert_eq!(sim.cursor(MAC), Point::new(1400.0, 0.0));
    cross_mac_to_pc(&mut sim);
    assert_eq!(sim.cursor(PC), Point::new(1.0, 0.0));
    // And back: PC y=0 lands on Mac y=0.
    sim.motion(MAC, -500.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 0.0));

    // Bottom corner: the Mac's last row maps onto the PC's last row.
    sim.motion(MAC, -100.0, 981.0);
    assert_eq!(sim.cursor(MAC), Point::new(1411.0, 981.0));
    sim.motion(MAC, 500.0, 0.0);
    sim.settle();
    // 981/982 of 864 is 863.12, past the PC's last addressable row.
    assert_eq!(sim.cursor(PC), Point::new(1.0, 863.0));
    sim.motion(MAC, 0.0, 500.0);
    sim.settle();
    assert_eq!(
        sim.cursor(PC),
        Point::new(1.0, 863.0),
        "clamped to the last row"
    );
    sim.motion(MAC, -500.0, 0.0);
    sim.settle();
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 863.0 / 864.0 * 982.0));
}

/// The Mac with an external monitor above the laptop, right-aligned with it
/// so both right edges line up and face the PC.
fn mac_with_monitor_above() -> Sim {
    Sim::new(SimConfig {
        mac_displays: vec![
            mac_laptop(),
            display(2, -408.0, -1080.0, 1920.0, 1080.0, 1.0),
        ],
        ..SimConfig::default()
    })
}

#[test]
fn a_monitor_above_the_mac_keeps_edge_mapping_working() {
    let mut sim = mac_with_monitor_above();
    // Up from the laptop onto the monitor is a local move.
    sim.motion(MAC, 500.0, -600.0);
    assert_eq!(sim.cursor(MAC), Point::new(1256.0, -109.0));
    assert_eq!(sim.state(MAC), State::Idle);
    sim.motion(MAC, 0.0, 600.0);
    // The laptop's right edge still crosses, mapped over the whole PC edge.
    sim.motion(MAC, 1000.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.cursor(PC), Point::new(1.0, 491.0 / 982.0 * 864.0));
    // Coming back, the PC's edge maps onto the Mac display that overlaps it
    // most: the monitor (481 units of overlap against the laptop's 383).
    sim.motion(MAC, -500.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    let back = sim.cursor(MAC);
    assert_eq!(back.x, 1511.0);
    assert!((back.y - (-1080.0 + 1080.0 * (491.0 / 982.0))).abs() < 1e-9);
    // And the monitor's own right edge crosses too.
    sim.motion(MAC, -3.0, 0.0);
    sim.motion(MAC, 20.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 3));
}

#[test]
fn a_controlled_mac_moves_between_its_displays() {
    let mut sim = mac_with_monitor_above();
    sim.motion(PC, -2000.0, 0.0);
    sim.settle();
    assert_eq!(
        sim.state(MAC),
        State::Controlled {
            by: sim.id(PC),
            seq: 2
        }
    );
    // The PC's middle row lands on the monitor's middle row.
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, -540.0));
    // Down onto the laptop is not an exit...
    sim.motion(PC, -200.0, 700.0);
    sim.settle();
    assert_eq!(sim.cursor(MAC), Point::new(1311.0, 160.0));
    // ... and the laptop's right edge hands the cursor back to the PC.
    sim.motion(PC, 400.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert_eq!(sim.cursor(PC), Point::new(1.0, 160.0 / 982.0 * 864.0));
}

#[test]
fn a_controlled_cursor_slides_along_the_top_onto_the_next_display() {
    // The PC has a second monitor to its right, top-aligned with its screen.
    let mut sim = Sim::new(SimConfig {
        pc_displays: vec![pc_screen(), display(2, 1536.0, 0.0, 1920.0, 1080.0, 1.0)],
        ..SimConfig::default()
    });
    cross_mac_to_pc(&mut sim);
    sim.motion(MAC, 1500.0, -500.0);
    sim.settle();
    assert_eq!(sim.cursor(PC), Point::new(1501.0, 0.0));
    // Moving right along the top edge with a slight upward push, as a hand
    // does on the way to a menu or a tab: the cursor carries on onto the
    // monitor, as Windows itself would move it, instead of sticking at the
    // screen's last column.
    for _ in 0..5 {
        sim.motion(MAC, 15.0, -2.0);
        sim.settle();
    }
    assert_eq!(sim.cursor(PC), Point::new(1576.0, 0.0));
    assert_eq!(sim.state(PC), controlled(&sim, 1));
}

#[test]
fn a_controlled_mac_follows_a_diagonal_onto_the_monitor_and_out() {
    // The monitor above the laptop, right-aligned with it, as in
    // `mac_with_monitor_above`; no corner dead zone, so the exit point
    // alone decides.
    let mut sim = Sim::new(SimConfig {
        mac_displays: vec![
            mac_laptop(),
            display(2, -408.0, -1080.0, 1920.0, 1080.0, 1.0),
        ],
        core: CoreConfig {
            corner_dead_zone: 0.0,
            ..CoreConfig::default()
        },
        ..SimConfig::default()
    });
    sim.motion(PC, -2000.0, 0.0);
    sim.settle();
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, -540.0));
    // Down onto the laptop, close to its top-right corner.
    sim.motion(PC, -6.0, 543.0);
    sim.settle();
    assert_eq!(sim.cursor(MAC), Point::new(1505.0, 3.0));
    // Up and to the right: the motion passes onto the monitor above and
    // leaves through the monitor's right edge, which faces the PC. Following
    // only the laptop's edges, the cursor stuck in the laptop's corner.
    sim.motion(PC, 10.0, -10.0);
    sim.settle();
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, PC, EndReason::CursorLeft));
    // It left the monitor 4 units above its bottom corner, which maps near
    // the bottom of the PC's edge.
    assert_eq!(sim.cursor(PC), Point::new(1.0, 1076.0 / 1080.0 * 864.0));
}

#[test]
fn a_wider_centered_monitor_moves_the_shared_edge() {
    // A 1920-wide monitor centered above the laptop sticks out 204 units on
    // each side; the PC sits flush against the group, so only the monitor's
    // right edge faces it and the laptop's right edge is a plain wall.
    let mut sim = Sim::new(SimConfig {
        mac_displays: vec![
            mac_laptop(),
            display(2, -204.0, -1080.0, 1920.0, 1080.0, 1.0),
        ],
        ..SimConfig::default()
    });
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 491.0));
    sim.motion(MAC, 0.0, -1031.0);
    sim.motion(MAC, 500.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    // Monitor y=-540 is its middle row, so it lands on the PC's middle row.
    assert_eq!(sim.cursor(PC), Point::new(1.0, 432.0));
}

#[test]
fn plugging_in_a_monitor_moves_the_shared_edge() {
    let mut sim = Sim::mac_and_pc();
    // A 1920-wide monitor goes up centered above the laptop. The agent
    // rebuilds its layout from the config, without the PC's displays.
    let monitor = display(2, -204.0, -1080.0, 1920.0, 1080.0, 1.0);
    sim.set_displays(MAC, vec![mac_laptop(), monitor]);
    // The core keeps the displays the PC reported and moves the PC flush
    // against the new, wider group.
    let pc = sim.dev[MAC].core.layout().device(&sim.id(PC)).cloned();
    let pc = pc.expect("the PC is still in the layout");
    assert_eq!(pc.displays(), &[pc_screen()]);
    assert_eq!(pc.origin(), Point::new(1716.0, -481.0));
    // The laptop's right edge is a wall now; the monitor's leads to the PC.
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    sim.motion(MAC, 0.0, -1031.0);
    sim.motion(MAC, 500.0, 0.0);
    sim.settle();
    assert_eq!(sim.state(MAC), controlling(&sim, 1));
    assert_eq!(sim.cursor(PC), Point::new(1.0, 432.0));
}

#[test]
fn an_unplugged_monitor_is_gone_before_timers_fire() {
    // Ticks far apart, so the enter timeout first comes due with the very
    // event that unplugs the monitor the crossing left from.
    let mut sim = Sim::new(SimConfig {
        mac_displays: vec![
            mac_laptop(),
            display(2, -408.0, -1080.0, 1920.0, 1080.0, 1.0),
        ],
        tick_ms: 1000,
        ..SimConfig::default()
    });
    sim.net.blackhole[PC] = true;
    sim.motion(MAC, 500.0, -600.0);
    sim.motion(MAC, 1000.0, 0.0);
    sim.advance(500);
    assert!(matches!(sim.state(MAC), State::Entering { .. }));
    sim.set_displays(MAC, vec![mac_laptop()]);
    assert_eq!(sim.state(MAC), State::Idle);
    assert!(ended(&sim, MAC, EndReason::EnterTimeout));
    // Back on the laptop, as close as it gets to where the cursor left.
    assert_eq!(sim.cursor(MAC), Point::new(1511.0, 0.0));
}

#[test]
fn a_crossing_needs_a_connected_neighbor() {
    let mut sim = Sim::mac_and_pc();
    sim.disconnect();
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
    // No neighbor beyond the left, top or bottom edges either.
    sim.reconnect();
    sim.motion(MAC, -3000.0, 0.0);
    sim.motion(MAC, 0.0, -3000.0);
    sim.motion(MAC, 0.0, 3000.0);
    assert_eq!(sim.state(MAC), State::Idle);
    assert_eq!(sim.cursor(MAC), Point::new(0.0, 981.0));
}

#[test]
fn hops_own_injected_events_are_ignored() {
    let mut sim = Sim::mac_and_pc();
    let cursor = sim.cursor(MAC);
    let echo = hop_core::Event::LocalInput {
        event: InputEvent::PointerMotion {
            dx: 5000.0,
            dy: 0.0,
        },
        cursor,
        physical: false,
    };
    assert!(sim.handle(MAC, echo).is_empty());
    cross_mac_to_pc(&mut sim);
    let echo = hop_core::Event::LocalInput {
        event: key(KEY_A, true),
        cursor,
        physical: false,
    };
    assert!(
        sim.handle(PC, echo).is_empty(),
        "no take-back from our own injection"
    );
    assert_eq!(sim.state(PC), controlled(&sim, 1));
}

#[test]
fn a_reconnect_ends_the_old_session() {
    let mut sim = Sim::mac_and_pc();
    cross_mac_to_pc(&mut sim);
    // A fresh Hello on the same link: the old session cannot be trusted.
    let peer = sim.id(MAC);
    let displays = sim.dev[MAC].displays.clone();
    sim.handle(
        PC,
        hop_core::Event::PeerConnected {
            peer,
            os: Os::MacOs,
            displays,
        },
    );
    assert_eq!(sim.state(PC), State::Idle);
    assert!(ended(&sim, PC, EndReason::Disconnected));
}

#[test]
fn invalid_peer_displays_are_reported_and_block_crossing() {
    let mut sim = Sim::mac_and_pc();
    let peer = sim.id(PC);
    let actions = sim.handle(
        MAC,
        hop_core::Event::PeerConnected {
            peer: peer.clone(),
            os: Os::Windows,
            displays: vec![display(1, f64::NAN, 0.0, 10.0, 10.0, 1.0)],
        },
    );
    assert!(matches!(
        actions.as_slice(),
        [Action::Notify(Notice::PeerDisplaysRejected { .. })]
    ));
    sim.motion(MAC, 2000.0, 0.0);
    assert_eq!(sim.state(MAC), State::Idle);
}

#[test]
fn an_enter_with_a_bogus_entry_point_lands_on_screen() {
    let mut sim = Sim::mac_and_pc();
    let enter = CoreMessage::Enter {
        seq: 5,
        entry: hop_types::EntryPoint {
            display_id: 42,
            edge: hop_types::Edge::Left,
            position: Point::new(f64::NAN, 1e300),
        },
        held_keys: vec![KEY_A, KeyCode::LEFT_SHIFT],
        source_os: Os::MacOs,
    };
    sim.deliver(PC, enter);
    assert_eq!(sim.state(PC), controlled(&sim, 5));
    assert_eq!(sim.cursor(PC), Point::new(768.0, 432.0));
    // Only modifiers are pressed for the peer.
    assert_eq!(
        sim.dev[PC].injected_events(),
        vec![(key(KeyCode::LEFT_SHIFT, true), Os::MacOs)]
    );
}
