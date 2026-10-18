//! Property tests: random interleavings of physical input on both devices,
//! bursts of input on one device (so sessions last long enough to type, drag
//! and come back), message delivery with random latency, lost motion
//! datagrams, ticks, disconnects, network outages that lose or delay
//! messages, the emergency chord, a monitor plugged in and out, and the
//! agent reporting a stall (`LocalStall`), sometimes after losing a key-up.
//! The simulation (see `sim`) checks the spec's invariants after every event,
//! including that messages from stale sessions change nothing and that no
//! key or button is left stuck on either side; these tests add made-up stale
//! messages, eventual agreement once input stops, and determinism.

mod sim;

use hop_core::{CoreConfig, Event, State};
use hop_types::{
    CoreMessage, DisplayInfo, Edge, EntryPoint, KeyCode, MotionMark, MouseButton, Point,
    RejectReason,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use sim::{KEY_A, MAC, PC, Sim, SimConfig, display, key, mac_laptop, pc_screen};

const KEYS: [KeyCode; 6] = [
    KeyCode::LEFT_CONTROL,
    KeyCode::LEFT_ALT,
    KeyCode::LEFT_SHIFT,
    KeyCode::ESCAPE,
    KeyCode::LEFT_META,
    KEY_A,
];

#[derive(Clone, Debug)]
enum Step {
    /// Physical pointer motion on `dev`.
    Motion { dev: usize, dx: f64, dy: f64 },
    /// One of `KEYS` goes down or up on `dev`.
    Key {
        dev: usize,
        key: usize,
        pressed: bool,
    },
    /// The left mouse button goes down or up on `dev`, and stays that way,
    /// as in a drag.
    Button { dev: usize, pressed: bool },
    /// A left click on `dev`: down, then up.
    Click { dev: usize },
    /// One wheel notch on `dev`.
    Scroll { dev: usize },
    /// Time passes; messages arrive and both cores get ticks.
    Advance { ms: u64 },
    /// The connection closes and both cores are told.
    Disconnect,
    /// A closed connection opens again.
    Reconnect,
    /// The Wi-Fi stops carrying messages to `to`, or in both directions
    /// when `to` is `None`, until a `Heal`. Unlike a disconnect, neither core
    /// is told: only its timers can notice. A `stall` holds reliable
    /// messages back until the heal, as a real connection does; otherwise
    /// they are lost.
    Outage { to: Option<usize>, stall: bool },
    /// The outage ends. Lost messages stay lost; held ones go out.
    Heal,
    /// The emergency chord on `dev`: Left Control, Left Alt and Left Shift
    /// go down, then Escape; then all four come up again.
    Chord { dev: usize },
    /// `taps` presses and releases of a letter key on `dev` with no time
    /// passing, more events than a crossing's input buffer holds.
    Typing { dev: usize, taps: usize },
    /// The Mac's external monitor is plugged in, or out if it was in.
    Monitor,
    /// Deliver a message that does not belong to the receiver's current
    /// session, and check that it changes nothing.
    Stale { dev: usize, kind: u8, offset: u64 },
    /// A run of input on `dev` alone, a few milliseconds apart: crossing,
    /// typing with modifiers, dragging and coming back, without the other
    /// device's input taking control back at once.
    Burst { dev: usize, acts: Vec<Act> },
    /// The agent on `dev` can no longer vouch for its input and feeds
    /// `LocalStall`. With `lose` set, the user first lets go of that key
    /// (if held) without the core hearing of it: the loss being reported.
    Stall { dev: usize, lose: Option<usize> },
}

/// One input in a [`Step::Burst`].
#[derive(Clone, Debug)]
enum Act {
    /// Pointer motion. With `toward` set, `dx` is positive and the motion
    /// pushes toward the other device (true) or away from it.
    Move {
        toward: Option<bool>,
        dx: f64,
        dy: f64,
    },
    Key {
        key: usize,
        pressed: bool,
    },
    Button {
        pressed: bool,
    },
    Wait {
        ms: u64,
    },
    /// A [`Step::Stall`] in the middle of the burst, on the burst's device
    /// (usually the controller by then) or, with `peer`, on the other one.
    Stall {
        peer: bool,
        lose: Option<usize>,
    },
}

fn act() -> impl Strategy<Value = Act> {
    // Stalls are rare, so they land mid-session with keys or a button held
    // without cutting most sessions short.
    prop_oneof![
        20 => (-120.0f64..120.0, -80.0f64..80.0)
            .prop_map(|(dx, dy)| Act::Move { toward: None, dx, dy }),
        8 => (any::<bool>(), 50.0f64..1500.0, -40.0f64..40.0)
            .prop_map(|(toward, dx, dy)| Act::Move { toward: Some(toward), dx, dy }),
        12 => (0..KEYS.len(), any::<bool>()).prop_map(|(key, pressed)| Act::Key { key, pressed }),
        8 => any::<bool>().prop_map(|pressed| Act::Button { pressed }),
        16 => (0u64..60).prop_map(|ms| Act::Wait { ms }),
        1 => (any::<bool>(), prop::option::of(0..KEYS.len()))
            .prop_map(|(peer, lose)| Act::Stall { peer, lose }),
    ]
}

/// `dx` (positive) as a horizontal motion of `dev` toward the other device
/// (`toward` true) or away from it.
fn signed_dx(dev: usize, toward: bool, dx: f64) -> f64 {
    if (dev == MAC) == toward { dx } else { -dx }
}

/// Pushes toward the other device (`toward` true) or away from it.
fn push(dev: usize, toward: bool, dx: f64, dy: f64) -> Step {
    Step::Motion {
        dev,
        dx: signed_dx(dev, toward, dx),
        dy,
    }
}

fn step() -> impl Strategy<Value = Step> {
    let dev = 0usize..2;
    prop_oneof![
        6 => (dev.clone(), -300.0f64..300.0, -200.0f64..200.0)
            .prop_map(|(dev, dx, dy)| Step::Motion { dev, dx, dy }),
        // Long pushes toward the shared edge make crossings (and returns)
        // frequent, including simultaneous ones.
        3 => (dev.clone(), 200.0f64..2500.0, -100.0f64..100.0)
            .prop_map(|(dev, dx, dy)| push(dev, true, dx, dy)),
        2 => (dev.clone(), 200.0f64..2500.0, -100.0f64..100.0)
            .prop_map(|(dev, dx, dy)| push(dev, false, dx, dy)),
        3 => (dev.clone(), 0..KEYS.len(), any::<bool>())
            .prop_map(|(dev, key, pressed)| Step::Key { dev, key, pressed }),
        // Mostly whole clicks: a button left down blocks every crossing.
        1 => (dev.clone(), any::<bool>()).prop_map(|(dev, pressed)| Step::Button { dev, pressed }),
        2 => dev.clone().prop_map(|dev| Step::Click { dev }),
        1 => dev.clone().prop_map(|dev| Step::Scroll { dev }),
        6 => (0u64..400).prop_map(|ms| Step::Advance { ms }),
        // Long enough for the enter and peer timeouts to fire, during an
        // outage or after a lost message.
        2 => (400u64..2500).prop_map(|ms| Step::Advance { ms }),
        1 => Just(Step::Disconnect),
        1 => Just(Step::Reconnect),
        // A heal is twice as likely as a new outage, so the network is down
        // about a third of the time.
        1 => (prop::option::of(dev.clone()), any::<bool>())
            .prop_map(|(to, stall)| Step::Outage { to, stall }),
        2 => Just(Step::Heal),
        1 => dev.clone().prop_map(|dev| Step::Chord { dev }),
        1 => (dev.clone(), 20usize..50).prop_map(|(dev, taps)| Step::Typing { dev, taps }),
        1 => Just(Step::Monitor),
        2 => (dev.clone(), 0u8..8, 1u64..4)
            .prop_map(|(dev, kind, offset)| Step::Stale { dev, kind, offset }),
        4 => (dev.clone(), prop::collection::vec(act(), 1..40))
            .prop_map(|(dev, acts)| Step::Burst { dev, acts }),
        1 => (dev, prop::option::of(0..KEYS.len()))
            .prop_map(|(dev, lose)| Step::Stall { dev, lose }),
    ]
}

/// A monitor above the Mac's laptop, right-aligned with it.
fn monitor() -> DisplayInfo {
    display(2, -408.0, -1080.0, 1920.0, 1080.0, 1.0)
}

/// A second PC monitor to the right of its screen, away from the Mac,
/// top-aligned with it and taller.
fn pc_monitor() -> DisplayInfo {
    display(2, 1536.0, 0.0, 1920.0, 1080.0, 1.0)
}

fn world() -> impl Strategy<Value = SimConfig> {
    (
        (0u64..30, 0u64..120),
        0.0f64..0.6,
        any::<u64>(),
        5u64..30,
        any::<bool>(),
        any::<bool>(),
        (any::<bool>(), any::<bool>()),
    )
        .prop_map(
            |((min, spread), drop_motion, seed, tick_ms, delay, dead_zone, monitors)| {
                let mut mac_displays = vec![mac_laptop()];
                if monitors.0 {
                    mac_displays.push(monitor());
                }
                let mut pc_displays = vec![pc_screen()];
                if monitors.1 {
                    pc_displays.push(pc_monitor());
                }
                SimConfig {
                    mac_displays,
                    pc_displays,
                    core: CoreConfig {
                        edge_delay_ms: if delay { 100 } else { 0 },
                        corner_dead_zone: if dead_zone { 10.0 } else { 0.0 },
                        ..CoreConfig::default()
                    },
                    latency: (min, min + spread),
                    drop_motion,
                    seed,
                    tick_ms,
                    ..SimConfig::default()
                }
            },
        )
}

fn run(config: SimConfig, steps: &[Step]) -> Sim {
    let mut sim = Sim::new(config);
    for step in steps {
        match *step {
            Step::Motion { dev, dx, dy } => sim.motion(dev, dx, dy),
            Step::Key { dev, key, pressed } => sim.key(dev, KEYS[key], pressed),
            Step::Button { dev, pressed } => sim.button(dev, MouseButton::Left, pressed),
            Step::Click { dev } => {
                sim.button(dev, MouseButton::Left, true);
                sim.button(dev, MouseButton::Left, false);
            }
            Step::Scroll { dev } => sim.scroll(dev, 1.0),
            Step::Advance { ms } => sim.advance(ms),
            Step::Disconnect => {
                if sim.net.connected {
                    sim.disconnect();
                }
            }
            Step::Reconnect => sim.reconnect(),
            Step::Outage { to, stall } => {
                let lines = if stall {
                    &mut sim.net.stall
                } else {
                    &mut sim.net.blackhole
                };
                match to {
                    Some(dev) => lines[dev] = true,
                    None => *lines = [true, true],
                }
            }
            Step::Heal => sim.heal(),
            Step::Chord { dev } => sim.chord(dev),
            Step::Typing { dev, taps } => {
                for _ in 0..taps {
                    sim.tap(dev, KEY_A);
                }
            }
            Step::Monitor => {
                let mut displays = vec![mac_laptop()];
                if sim.dev[MAC].displays.len() == 1 {
                    displays.push(monitor());
                }
                sim.set_displays(MAC, displays);
            }
            Step::Stale { dev, kind, offset } => check_stale(&mut sim, dev, kind, offset),
            Step::Burst { dev, ref acts } => {
                for act in acts {
                    match *act {
                        Act::Move { toward, dx, dy } => {
                            let dx = toward.map_or(dx, |toward| signed_dx(dev, toward, dx));
                            sim.motion(dev, dx, dy);
                        }
                        Act::Key { key, pressed } => sim.key(dev, KEYS[key], pressed),
                        Act::Button { pressed } => sim.button(dev, MouseButton::Left, pressed),
                        Act::Wait { ms } => sim.advance(ms),
                        Act::Stall { peer, lose } => {
                            stall(&mut sim, if peer { 1 - dev } else { dev }, lose);
                        }
                    }
                }
            }
            Step::Stall { dev, lose } => stall(&mut sim, dev, lose),
        }
    }
    sim
}

/// The agent on `dev` reports a stall, after losing the key-up of
/// `KEYS[lose]` if that key is held.
fn stall(sim: &mut Sim, dev: usize, lose: Option<usize>) {
    if let Some(key) = lose {
        sim.lose_key_up(dev, KEYS[key]);
    }
    sim.stall(dev);
}

/// "Messages from stale sessions change nothing": delivering one must have
/// exactly the effect of a plain `Tick` at the same moment (timers and
/// heartbeats still run), on the actions and on the whole core state.
fn check_stale(sim: &mut Sim, dev: usize, kind: u8, offset: u64) {
    let current = match sim.state(dev) {
        State::Idle => 0,
        State::Entering { seq, .. }
        | State::Controlling { seq, .. }
        | State::Controlled { seq, .. } => seq,
    };
    let seq = current.wrapping_add(offset);
    let mark = MotionMark {
        n: 1_000,
        total_dx: 500.0,
        total_dy: -500.0,
    };
    let entry = EntryPoint {
        display_id: 1,
        edge: Edge::Right,
        position: Point::new(10.0, 10.0),
    };
    let msg = match kind {
        0 => CoreMessage::EnterAck { seq },
        1 => CoreMessage::EnterReject {
            seq,
            reason: RejectReason::Busy,
        },
        2 => CoreMessage::Leave {
            seq,
            to: sim.id(dev),
            entry,
        },
        3 => CoreMessage::TakeBack { seq },
        4 => CoreMessage::Release { seq },
        5 => CoreMessage::Input {
            seq,
            mark,
            events: vec![key(KEY_A, true)],
        },
        6 => CoreMessage::Motion { seq, mark },
        _ => CoreMessage::Heartbeat { seq },
    };
    let mut twin = sim.dev[dev].core.clone();
    let expected = twin.handle(sim.now, Event::Tick);
    let actual = sim.deliver(dev, msg.clone());
    assert_eq!(actual, expected, "stale {msg:?} acted");
    assert_eq!(
        format!("{:?}", sim.dev[dev].core),
        format!("{twin:?}"),
        "stale {msg:?} changed the core"
    );
}

/// Once input stops and the network drains, the devices agree: both idle,
/// or one controls the other in the same session. An outage that is still
/// going on must have ended any session by then.
fn check_agreement(sim: &mut Sim) {
    let outage = sim.outage();
    sim.advance(3_000);
    let (mac, pc) = (sim.state(MAC), sim.state(PC));
    let (mac_id, pc_id) = (sim.id(MAC), sim.id(PC));
    let agreed = match (&mac, &pc) {
        (State::Idle, State::Idle) => true,
        _ if outage => false,
        (State::Controlling { target, seq: a }, State::Controlled { by, seq: b }) => {
            *target == pc_id && *by == mac_id && a == b
        }
        (State::Controlled { by, seq: b }, State::Controlling { target, seq: a }) => {
            *target == mac_id && *by == pc_id && a == b
        }
        _ => false,
    };
    assert!(
        agreed,
        "devices disagree after settling (outage: {outage}): {mac:?} / {pc:?}"
    );
}

/// The proptest settings: `cases` random runs, unless the `PROPTEST_CASES`
/// environment variable asks for more (or fewer), as for a long soak:
/// `PROPTEST_CASES=20000 cargo test -p hop-core --release --test properties`.
/// A failing case is saved next to this file and retried first next time.
fn config(cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
            "proptest-regressions",
        ))),
        ..ProptestConfig::default()
    };
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = cases;
    }
    config
}

proptest! {
    #![proptest_config(config(400))]

    #[test]
    fn invariants_hold_under_random_interleavings(
        config in world(),
        steps in prop::collection::vec(step(), 1..150),
    ) {
        let mut sim = run(config, &steps);
        check_agreement(&mut sim);
    }
}

proptest! {
    #![proptest_config(config(64))]

    #[test]
    fn the_same_inputs_give_the_same_outputs(
        config in world(),
        steps in prop::collection::vec(step(), 1..80),
    ) {
        let a = run(config.clone(), &steps);
        let b = run(config, &steps);
        for d in [MAC, PC] {
            prop_assert_eq!(format!("{:?}", a.dev[d].log), format!("{:?}", b.dev[d].log));
        }
    }
}

/// Guards against a vacuous property test: random runs from a fixed seed
/// must start sessions in both directions, bring the cursor back through the
/// edge from both sides, press buttons on both sides for the peer (drags that
/// block exits), stall in every role, and reach every common kind of session
/// ending, each several times.
///
/// The other endings are left to targeted tests because random runs reach
/// them too rarely to count on, or not at all: `Superseded` and
/// `Rejected(Busy)` need a message lost at one precise moment (see the
/// scenarios `a_new_crossing_supersedes_a_session_whose_release_was_lost`,
/// `an_enter_from_the_controlled_device_supersedes_the_session` and
/// `a_tie_break_loser_whose_rival_enter_was_lost_is_told_busy`),
/// `Rejected(Unavailable)` a device without displays
/// (`a_device_without_displays_refuses_to_be_entered`), and
/// `UnreachableTarget` a third device (unit tests in `machine.rs`).
#[test]
fn random_runs_exercise_every_kind_of_ending() {
    use hop_core::{Action, EndReason, Notice};
    use hop_types::InputEvent;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use std::collections::BTreeMap;

    const OFTEN: usize = 3;
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[7; 32]),
    );
    let strategy = (world(), prop::collection::vec(step(), 1..150));
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..200 {
        let (config, steps) = strategy.new_tree(&mut runner).expect("strategy").current();
        let mut sim = run(config, &steps);
        check_agreement(&mut sim);
        for dev in &sim.dev {
            for (_, action) in &dev.log {
                let name = match action {
                    Action::Notify(Notice::SessionEnded { reason, .. }) => format!("{reason:?}"),
                    Action::Notify(Notice::ControlStarted { .. }) => {
                        format!("ControlStarted by {}", dev.id)
                    }
                    Action::Send {
                        msg: CoreMessage::Leave { .. },
                        ..
                    } => format!("Leave from {}", dev.id),
                    Action::Inject {
                        event: InputEvent::Button { pressed: true, .. },
                        source_os,
                    } if *source_os != dev.os => format!("button for the peer on {}", dev.id),
                    _ => continue,
                };
                *counts.entry(name).or_default() += 1;
            }
            for state in &dev.stalls {
                let role = match state {
                    State::Idle => "idle",
                    State::Entering { .. } | State::Controlling { .. } => "controlling",
                    State::Controlled { .. } => "controlled",
                };
                *counts
                    .entry(format!("LocalStall while {role}"))
                    .or_default() += 1;
            }
        }
    }
    println!("{counts:#?}");
    let endings = [
        EndReason::CursorLeft,
        EndReason::TakenBack,
        EndReason::Released,
        EndReason::EmergencyChord,
        EndReason::EnterTimeout,
        EndReason::PeerTimeout,
        EndReason::Disconnected,
        EndReason::LostTieBreak,
        EndReason::BufferFull,
        EndReason::Stalled,
    ];
    let names = endings.iter().map(|reason| format!("{reason:?}")).chain(
        ["idle", "controlling", "controlled"].map(|role| format!("LocalStall while {role}")),
    );
    let per_device = ["mac", "pc"].into_iter().flat_map(|id| {
        [
            format!("ControlStarted by {id}"),
            format!("Leave from {id}"),
            format!("button for the peer on {id}"),
        ]
    });
    for name in names.chain(per_device) {
        let seen = counts.get(&name).copied().unwrap_or(0);
        assert!(seen >= OFTEN, "{name} seen {seen} times in {counts:?}");
    }
}
