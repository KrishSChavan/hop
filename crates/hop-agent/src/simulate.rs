//! `hop simulate`: a scripted session between a simulated Mac and a simulated
//! Windows PC, run through the real hand-off core and keymap.
//!
//! Nothing touches the OS. Each simulated machine has a cursor that its "OS"
//! moves and clamps to the screen, and the network delivers messages after a
//! short delay. The script crosses to the PC, types Cmd+C, clicks, comes
//! back, is taken back from the PC's own trackpad, and survives a Wi-Fi drop.

use std::collections::BTreeSet;

use hop_core::{Action, CaptureMode, Core, CoreConfig, EndReason, Event, Layout, Millis, Notice};
use hop_keymap::{ModifierPolicy, Translator};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, Edge, InputEvent, KeyCode, MouseButton, Os, Point, Rect,
};

/// One-way network delay, roughly home Wi-Fi.
const LATENCY: Millis = 4;
/// Time between simulated input events, like a 125 Hz mouse.
const STEP: Millis = 8;
/// How often each core gets a `Tick` so its timers can fire.
const TICK: Millis = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Mac,
    Pc,
}

struct Machine {
    name: &'static str,
    os: Os,
    screen: Rect,
    core: Core,
    cursor: Point,
    capture: CaptureMode,
    /// Keys Hop is holding down here for the other machine, after the
    /// Cmd/Ctrl swap. Created when a session starts.
    keys: Option<Translator>,
    buttons: BTreeSet<MouseButton>,
}

struct Packet {
    at: Millis,
    to: Side,
    from: DeviceId,
    msg: CoreMessage,
}

/// What happened during a run, for the summary and for tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub crossings_to_pc: u32,
    pub returns_to_mac: u32,
    pub take_backs: u32,
    pub timeouts: u32,
    pub swapped_keys: u32,
    pub keys_left_down: usize,
    pub buttons_left_down: usize,
}

struct Sim {
    now: Millis,
    mac: Machine,
    pc: Machine,
    in_flight: Vec<Packet>,
    online: bool,
    print: bool,
    summary: Summary,
}

fn display(width: f64, height: f64, scale: f64) -> DisplayInfo {
    DisplayInfo {
        id: 1,
        frame: Rect::new(0.0, 0.0, width, height),
        scale,
        primary: true,
    }
}

impl Sim {
    fn new(print: bool) -> Sim {
        let mac_id = DeviceId::new("mac");
        let pc_id = DeviceId::new("pc");
        let mac_display = display(1512.0, 982.0, 2.0);
        let pc_display = display(1536.0, 864.0, 1.25);

        // Each machine has its own view of the layout: the PC is right of
        // the Mac, and the Mac is left of the PC.
        let mac_layout = Layout::two_devices(
            mac_id.clone(),
            vec![mac_display.clone()],
            pc_id.clone(),
            Edge::Right,
            vec![pc_display.clone()],
        )
        .expect("the demo layout is valid");
        let pc_layout = Layout::two_devices(
            pc_id.clone(),
            vec![pc_display.clone()],
            mac_id.clone(),
            Edge::Left,
            vec![mac_display.clone()],
        )
        .expect("the demo layout is valid");

        let machine = |name, os, id: &DeviceId, display: &DisplayInfo, layout| Machine {
            name,
            os,
            screen: display.frame,
            core: Core::new(id.clone(), os, CoreConfig::default(), layout),
            cursor: display.frame.center(),
            capture: CaptureMode::Passive,
            keys: None,
            buttons: BTreeSet::new(),
        };
        let mut sim = Sim {
            now: 0,
            mac: machine("Mac", Os::MacOs, &mac_id, &mac_display, mac_layout),
            pc: machine("PC", Os::Windows, &pc_id, &pc_display, pc_layout),
            in_flight: Vec::new(),
            online: true,
            print,
            summary: Summary::default(),
        };

        // Both machines have found each other on the network.
        sim.event(
            Side::Mac,
            Event::PeerConnected {
                peer: pc_id,
                os: Os::Windows,
                displays: vec![pc_display],
            },
        );
        sim.event(
            Side::Pc,
            Event::PeerConnected {
                peer: mac_id,
                os: Os::MacOs,
                displays: vec![mac_display],
            },
        );
        sim
    }

    fn machine(&mut self, side: Side) -> &mut Machine {
        match side {
            Side::Mac => &mut self.mac,
            Side::Pc => &mut self.pc,
        }
    }

    fn side_of(&self, id: &DeviceId) -> Side {
        if id.as_str() == "mac" {
            Side::Mac
        } else {
            Side::Pc
        }
    }

    fn say(&self, side: Side, text: impl AsRef<str>) {
        if self.print {
            let name = match side {
                Side::Mac => self.mac.name,
                Side::Pc => self.pc.name,
            };
            println!("  {:>6} ms  {name:<3}  {}", self.now, text.as_ref());
        }
    }

    fn section(&self, title: &str) {
        if self.print {
            println!("\n{title}");
        }
    }

    /// Hand an event to one machine's core and carry out what it decides.
    fn event(&mut self, side: Side, event: Event) {
        let now = self.now;
        let actions = self.machine(side).core.handle(now, event);
        for action in actions {
            self.act(side, action);
        }
    }

    /// Physical input on one machine: the user touched its mouse or keyboard.
    fn physical(&mut self, side: Side, event: InputEvent) {
        let machine = self.machine(side);
        let before = machine.cursor;
        let os_moves_cursor = machine.capture == CaptureMode::Passive;
        self.event(
            side,
            Event::LocalInput {
                event,
                cursor: before,
                physical: true,
            },
        );
        // While Hop is not swallowing input, the OS moves the cursor itself
        // and stops it at the screen edge.
        if let (true, InputEvent::PointerMotion { dx, dy }) = (os_moves_cursor, event) {
            let machine = self.machine(side);
            machine.cursor = machine
                .screen
                .clamp(Point::new(before.x + dx, before.y + dy), 1.0);
        }
    }

    fn act(&mut self, side: Side, action: Action) {
        match action {
            Action::Send { to, msg } => {
                if let Some(text) = describe(&msg, self.machine(side).os) {
                    let to_name = self.machine(self.side_of(&to)).name;
                    let lost = if self.online {
                        ""
                    } else {
                        "  (lost: the Wi-Fi is down)"
                    };
                    self.say(side, format!("→ {to_name}: {text}{lost}"));
                }
                if self.online {
                    let from = DeviceId::new(match side {
                        Side::Mac => "mac",
                        Side::Pc => "pc",
                    });
                    self.in_flight.push(Packet {
                        at: self.now + LATENCY,
                        to: self.side_of(&to),
                        from,
                        msg,
                    });
                }
            }
            Action::SetCaptureMode(mode) => self.machine(side).capture = mode,
            Action::ParkCursor => self.say(
                side,
                "hides its cursor; its mouse and keyboard now drive the other machine",
            ),
            Action::RestoreCursor { at } => {
                self.machine(side).cursor = at;
                self.say(
                    side,
                    format!("shows its cursor again at ({:.0}, {:.0})", at.x, at.y),
                );
            }
            Action::MoveCursorTo { to } => self.machine(side).cursor = to,
            Action::Inject { event, source_os } => self.inject(side, event, source_os),
            Action::ReleaseAllInjected => {
                let machine = self.machine(side);
                let os = machine.os;
                let keys = machine
                    .keys
                    .take()
                    .map(|mut t| t.release_all())
                    .unwrap_or_default();
                let buttons = std::mem::take(&mut machine.buttons);
                if !keys.is_empty() || !buttons.is_empty() {
                    let names: Vec<String> = keys
                        .iter()
                        .map(|k| key_label(*k, os))
                        .chain(buttons.iter().map(|b| format!("{b:?} button")))
                        .collect();
                    self.say(
                        side,
                        format!("lets go of everything Hop held: {}", names.join(", ")),
                    );
                }
            }
            Action::Notify(notice) => self.notice(side, notice),
        }
    }

    fn inject(&mut self, side: Side, event: InputEvent, source_os: Os) {
        let machine = self.machine(side);
        let os = machine.os;
        match event {
            InputEvent::Key { code, pressed, .. } => {
                let keys = machine.keys.get_or_insert_with(|| {
                    Translator::new(ModifierPolicy::default(), source_os, os)
                });
                let injected = if pressed {
                    keys.press(code)
                } else {
                    keys.release(code)
                };
                let verb = if pressed { "presses" } else { "releases" };
                if injected == code {
                    self.say(side, format!("{verb} {}", key_label(injected, os)));
                } else {
                    if pressed {
                        self.summary.swapped_keys += 1;
                    }
                    self.say(
                        side,
                        format!(
                            "{verb} {}  (the {}'s {}, swapped)",
                            key_label(injected, os),
                            if source_os == Os::MacOs { "Mac" } else { "PC" },
                            key_label(code, source_os)
                        ),
                    );
                }
            }
            InputEvent::Button { button, pressed } => {
                let at = machine.cursor;
                if pressed {
                    machine.buttons.insert(button);
                } else {
                    machine.buttons.remove(&button);
                }
                let verb = if pressed { "presses" } else { "releases" };
                self.say(
                    side,
                    format!(
                        "{verb} the {button:?} mouse button at ({:.0}, {:.0})",
                        at.x, at.y
                    ),
                );
            }
            InputEvent::Scroll { dy, .. } => self.say(side, format!("scrolls by {dy}")),
            InputEvent::PointerMotion { .. } => {}
        }
    }

    fn notice(&mut self, side: Side, notice: Notice) {
        match notice {
            Notice::ControlStarted { .. } => {
                if side == Side::Mac {
                    self.summary.crossings_to_pc += 1;
                }
                self.say(side, "is now controlling the other machine");
            }
            Notice::ControlledBy { .. } => {
                let at = self.machine(side).cursor;
                self.say(
                    side,
                    format!(
                        "is now controlled by the other machine; its cursor is at ({:.0}, {:.0})",
                        at.x, at.y
                    ),
                );
            }
            Notice::SessionEnded { reason, .. } => {
                if side == Side::Mac {
                    match reason {
                        EndReason::CursorLeft => self.summary.returns_to_mac += 1,
                        EndReason::TakenBack => self.summary.take_backs += 1,
                        _ => {}
                    }
                }
                if matches!(reason, EndReason::PeerTimeout) {
                    self.summary.timeouts += 1;
                }
                self.say(side, format!("session over: {}", reason_text(&reason)));
            }
            Notice::PeerDisplaysRejected { error, .. } => {
                self.say(
                    side,
                    format!("cannot use the other machine's screens: {error}"),
                );
            }
        }
    }

    /// Let `ms` milliseconds pass: deliver messages and let timers fire.
    fn advance(&mut self, ms: Millis) {
        for _ in 0..ms {
            self.now += 1;
            let now = self.now;
            let (due, later): (Vec<Packet>, Vec<Packet>) = std::mem::take(&mut self.in_flight)
                .into_iter()
                .partition(|p| p.at <= now);
            self.in_flight = later;
            for packet in due {
                self.event(
                    packet.to,
                    Event::PeerMessage {
                        from: packet.from,
                        msg: packet.msg,
                    },
                );
            }
            if now.is_multiple_of(TICK) {
                self.event(Side::Mac, Event::Tick);
                self.event(Side::Pc, Event::Tick);
            }
        }
    }

    fn mouse(&mut self, side: Side, dx: f64, dy: f64, steps: u32) {
        for _ in 0..steps {
            self.physical(side, InputEvent::PointerMotion { dx, dy });
            self.advance(STEP);
        }
    }

    /// Move until `done` says so, at most `max_steps` events.
    fn mouse_until(
        &mut self,
        side: Side,
        dx: f64,
        dy: f64,
        max_steps: u32,
        done: impl Fn(&Sim) -> bool,
    ) {
        for _ in 0..max_steps {
            if done(self) {
                return;
            }
            self.mouse(side, dx, dy, 1);
        }
    }

    fn key(&mut self, side: Side, code: KeyCode, pressed: bool) {
        self.physical(
            side,
            InputEvent::Key {
                code,
                pressed,
                repeat: false,
            },
        );
        self.advance(STEP);
    }

    fn click(&mut self, side: Side) {
        for pressed in [true, false] {
            self.physical(
                side,
                InputEvent::Button {
                    button: MouseButton::Left,
                    pressed,
                },
            );
            self.advance(STEP);
        }
    }

    fn pc_is_controlled(&self) -> bool {
        matches!(self.pc.core.state(), hop_core::State::Controlled { .. })
    }

    fn mac_is_idle(&self) -> bool {
        matches!(self.mac.core.state(), hop_core::State::Idle)
    }
}

/// Run the scripted session. With `print`, narrate it on stdout.
pub fn run(print: bool) -> Summary {
    let mut sim = Sim::new(print);
    if print {
        println!("Hop simulation: a Mac (1512 x 982) with a Windows PC (1536 x 864) to its right.");
        println!(
            "Nothing here touches this computer: input, cursors and the network are simulated."
        );
    }

    sim.section("1. Move the Mac's mouse to the right, past the edge of its screen");
    sim.mouse_until(Side::Mac, 40.0, 0.0, 40, Sim::pc_is_controlled);
    sim.mouse(Side::Mac, 30.0, 12.0, 10);
    let at = sim.pc.cursor;
    sim.say(
        Side::Pc,
        format!(
            "its cursor followed the Mac's mouse to ({:.0}, {:.0})",
            at.x, at.y
        ),
    );

    sim.section("2. Press Cmd+C on the Mac's keyboard");
    sim.key(Side::Mac, KeyCode::LEFT_META, true);
    sim.key(Side::Mac, KeyCode::keyboard(0x06), true); // C
    sim.key(Side::Mac, KeyCode::keyboard(0x06), false);
    sim.key(Side::Mac, KeyCode::LEFT_META, false);
    sim.advance(20);

    sim.section("3. Click on the PC with the Mac's trackpad");
    sim.click(Side::Mac);
    sim.advance(20);

    sim.section("4. Move back to the left, onto the Mac");
    sim.mouse_until(Side::Mac, -60.0, 0.0, 60, Sim::mac_is_idle);
    sim.advance(20);

    sim.section("5. Go to the PC again, then touch the PC's own trackpad");
    sim.mouse_until(Side::Mac, 60.0, 0.0, 60, Sim::pc_is_controlled);
    sim.mouse(Side::Mac, 20.0, 0.0, 3);
    sim.mouse(Side::Pc, 6.0, 0.0, 1);
    sim.advance(20);

    sim.section("6. Go to the PC again, hold Shift, then the Wi-Fi drops");
    // A session that ended without the cursor coming back leaves the edge
    // disarmed until the mouse stops pushing against it, so back off first.
    sim.mouse(Side::Mac, -40.0, 0.0, 3);
    sim.mouse_until(Side::Mac, 60.0, 0.0, 60, Sim::pc_is_controlled);
    sim.mouse(Side::Mac, 20.0, 0.0, 3);
    sim.key(Side::Mac, KeyCode::LEFT_SHIFT, true);
    sim.online = false;
    sim.say(
        Side::Mac,
        "(the Wi-Fi drops: nothing gets through for 2 seconds)",
    );
    sim.advance(2000);
    sim.online = true;
    sim.key(Side::Mac, KeyCode::LEFT_SHIFT, false);

    sim.summary.keys_left_down = sim.pc.keys.as_ref().map_or(0, |k| k.pressed().count())
        + sim.mac.keys.as_ref().map_or(0, |k| k.pressed().count());
    sim.summary.buttons_left_down = sim.pc.buttons.len() + sim.mac.buttons.len();
    if print {
        let s = &sim.summary;
        println!("\nSummary");
        println!("  crossings to the PC: {}", s.crossings_to_pc);
        println!(
            "  returns to the Mac through the edge: {}",
            s.returns_to_mac
        );
        println!("  taken back by the PC's own trackpad: {}", s.take_backs);
        println!(
            "  sessions ended by the Wi-Fi drop: {} (one on each machine)",
            s.timeouts
        );
        println!("  keys swapped Cmd -> Ctrl: {}", s.swapped_keys);
        println!(
            "  keys or buttons Hop is still holding down anywhere: {}",
            s.keys_left_down + s.buttons_left_down
        );
    }
    sim.summary
}

/// A short description of a message worth narrating; `None` for the
/// constant chatter (motion and heartbeats).
fn describe(msg: &CoreMessage, sender_os: Os) -> Option<String> {
    Some(match msg {
        CoreMessage::Enter {
            entry, held_keys, ..
        } => {
            let held = if held_keys.is_empty() {
                String::new()
            } else {
                let names: Vec<String> =
                    held_keys.iter().map(|k| key_label(*k, sender_os)).collect();
                format!(", holding {}", names.join("+"))
            };
            format!(
                "\"my cursor is coming over to you, at ({:.0}, {:.0}){held}\"",
                entry.position.x, entry.position.y
            )
        }
        CoreMessage::EnterAck { .. } => "\"OK, go ahead\"".to_owned(),
        CoreMessage::EnterReject { reason, .. } => format!("\"not now ({reason:?})\""),
        CoreMessage::Leave { entry, .. } => format!(
            "\"the cursor left my screen; it comes back to you at ({:.0}, {:.0})\"",
            entry.position.x, entry.position.y
        ),
        CoreMessage::TakeBack { .. } => "\"I'm taking control back\"".to_owned(),
        CoreMessage::Release { .. } => "\"the session is over\"".to_owned(),
        CoreMessage::Input { events, .. } => {
            let parts: Vec<String> = events
                .iter()
                .map(|event| match event {
                    InputEvent::Key { code, pressed, .. } => {
                        format!(
                            "{} {}",
                            key_label(*code, sender_os),
                            if *pressed { "down" } else { "up" }
                        )
                    }
                    InputEvent::Button { button, pressed } => {
                        format!("{button:?} button {}", if *pressed { "down" } else { "up" })
                    }
                    InputEvent::Scroll { .. } => "scroll".to_owned(),
                    InputEvent::PointerMotion { .. } => "motion".to_owned(),
                })
                .collect();
            format!("input: {}", parts.join(", "))
        }
        CoreMessage::Motion { .. } | CoreMessage::Heartbeat { .. } => return None,
        CoreMessage::ClipboardText { text, .. } => format!("clipboard text ({} bytes)", text.len()),
    })
}

/// What a key is called on `os`'s keyboard.
fn key_label(code: KeyCode, os: Os) -> String {
    let mac = os == Os::MacOs;
    match code {
        KeyCode::LEFT_META | KeyCode::RIGHT_META => if mac { "Cmd" } else { "Win" }.to_owned(),
        KeyCode::LEFT_CONTROL | KeyCode::RIGHT_CONTROL => "Ctrl".to_owned(),
        KeyCode::LEFT_ALT | KeyCode::RIGHT_ALT => if mac { "Option" } else { "Alt" }.to_owned(),
        KeyCode::LEFT_SHIFT | KeyCode::RIGHT_SHIFT => "Shift".to_owned(),
        _ => match hop_keymap::name(code) {
            Some(name) => name
                .strip_prefix("Key")
                .or_else(|| name.strip_prefix("Digit"))
                .unwrap_or(name)
                .to_owned(),
            None => format!("key {:#x}", code.0),
        },
    }
}

fn reason_text(reason: &EndReason) -> String {
    match reason {
        EndReason::CursorLeft => "the cursor came back".to_owned(),
        EndReason::TakenBack => "the PC's own mouse or keyboard was used".to_owned(),
        EndReason::PeerTimeout => "no word from the other machine for 1.5 seconds".to_owned(),
        EndReason::Released => "the other machine ended it".to_owned(),
        EndReason::EmergencyChord => "the emergency keys were pressed".to_owned(),
        EndReason::Disconnected => "the other machine disconnected".to_owned(),
        EndReason::EnterTimeout => "the other machine didn't answer".to_owned(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scripted_session_goes_as_planned_and_leaves_nothing_held() {
        let summary = run(false);
        assert_eq!(
            summary,
            Summary {
                crossings_to_pc: 3,
                returns_to_mac: 1,
                take_backs: 1,
                timeouts: 2,
                swapped_keys: 1,
                keys_left_down: 0,
                buttons_left_down: 0,
            }
        );
    }
}
