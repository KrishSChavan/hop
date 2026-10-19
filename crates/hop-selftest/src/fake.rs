//! A pretend machine for testing the self-test without touching the real
//! mouse and keyboard.
//!
//! [`World`] models what the self-test can observe of an OS: a cursor on a
//! display, a capture backend with a suppression lease, an injector, a test
//! window with a text box and a button, and knobs that make each part
//! misbehave the way a buggy backend might. A pretend user on a thread of
//! its own reads the window's instructions and does what they say.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hop_platform::{
    CaptureMode, CapturedEvent, CursorControl, Displays, InputCapture, InputInjector, NoDrops,
    Permission, PermissionState, Permissions, Platform, PlatformError, Result, SystemState,
    TestWindow, TextClipboard,
};
use hop_types::{DisplayInfo, InputEvent, KeyCode, MouseButton, Os, Point, Rect, ScrollUnit};

use crate::keys::{KEY_A, KEY_C, command_key, key_for_char};
use crate::observe::Sink;
use crate::options::SelfTestOptions;
use crate::report::{Report, StepId};
use crate::session::START_PROMPT;

pub(crate) type Shared = Arc<Mutex<World>>;

/// Locks the world, even after a pretend backend panicked while holding it.
pub(crate) fn lock(world: &Shared) -> MutexGuard<'_, World> {
    world.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The pretend machine.
pub(crate) struct World {
    // ----- how it behaves -----
    pub accessibility: PermissionState,
    pub secure_input: bool,
    /// This backend's own suppression lease, shorter than the real one.
    pub lease: Duration,
    pub lease_never_lapses: bool,
    /// A broken backend: when the lease runs out it goes passive but leaves
    /// the cursor parked.
    pub lapse_keeps_cursor_parked: bool,
    /// A broken backend: a parked cursor still follows the mouse.
    pub park_moves_cursor: bool,
    pub suppress_moves_cursor: bool,
    pub suppress_swallows_injected: bool,
    pub capture_sees_injected: bool,
    pub injected_look_physical: bool,
    pub shift_broken: bool,
    /// The test window never gets the keyboard focus: typed keys go to
    /// some other app.
    pub keys_go_elsewhere: bool,
    pub clicks_per_click: u32,
    /// Injected moves land this far off on both axes.
    pub move_offset: f64,
    /// At a screen edge, report only the motion the cursor made (the bug the
    /// platform contract warns about).
    pub clamp_attempted_motion: bool,
    pub capture_fails: bool,
    pub window_fails: bool,
    pub panic_on_button_center: bool,
    pub panic_when_parked: bool,
    pub panic_on_release_all: bool,
    // ----- state -----
    pub displays: Vec<DisplayInfo>,
    pub cursor: Point,
    pub suppress_until: Option<Instant>,
    pub parked: Option<Point>,
    pub sink: Option<Sink>,
    pub window_open: bool,
    pub instructions: String,
    pub text: String,
    pub select_all: bool,
    pub shift: bool,
    pub command: bool,
    pub clicks: u32,
    pub button_center: Point,
    pub held_keys: BTreeSet<KeyCode>,
    pub held_buttons: BTreeSet<MouseButton>,
    /// What the self-test did, in order: "capture start", "suppress",
    /// "passive", "release all", "park", "restore x y", ...
    pub log: Vec<String>,
    /// How many key events the self-test injected.
    pub injected_key_events: u32,
}

impl Default for World {
    fn default() -> Self {
        Self {
            accessibility: PermissionState::Granted,
            secure_input: false,
            lease: Duration::from_millis(300),
            lease_never_lapses: false,
            lapse_keeps_cursor_parked: false,
            park_moves_cursor: false,
            suppress_moves_cursor: false,
            suppress_swallows_injected: false,
            capture_sees_injected: true,
            injected_look_physical: false,
            shift_broken: false,
            keys_go_elsewhere: false,
            clicks_per_click: 1,
            move_offset: 0.0,
            clamp_attempted_motion: false,
            capture_fails: false,
            window_fails: false,
            panic_on_button_center: false,
            panic_when_parked: false,
            panic_on_release_all: false,
            displays: vec![DisplayInfo {
                id: 1,
                frame: Rect::new(0.0, 0.0, 1512.0, 982.0),
                scale: 2.0,
                primary: true,
            }],
            cursor: Point::new(300.0, 491.0),
            suppress_until: None,
            parked: None,
            sink: None,
            window_open: false,
            instructions: String::new(),
            text: String::new(),
            select_all: false,
            shift: false,
            command: false,
            clicks: 0,
            button_center: Point::new(700.0, 400.0),
            held_keys: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            log: Vec::new(),
            injected_key_events: 0,
        }
    }
}

impl World {
    pub(crate) fn suppressing(&self) -> bool {
        self.suppress_until
            .is_some_and(|until| Instant::now() < until)
    }

    pub(crate) fn logged(&self, entry: &str) -> bool {
        self.log.iter().any(|line| line == entry)
    }

    fn position(&self) -> Point {
        self.parked.unwrap_or(self.cursor)
    }

    /// `p` kept on screen, as an OS keeps its cursor.
    fn clamp(&self, p: Point) -> Point {
        let bounds = self
            .displays
            .iter()
            .map(|d| d.frame)
            .reduce(|a, b| a.union(&b))
            .unwrap_or_default();
        bounds.clamp(p, 1.0)
    }

    fn emit(&mut self, event: InputEvent, cursor: Point, physical: bool) {
        if let Some(sink) = self.sink.as_mut() {
            sink(CapturedEvent {
                event,
                cursor,
                physical,
            });
        }
    }

    /// A real backend frees a parked cursor by itself once the suppression
    /// lease runs out (the fail-open rule). This one does too, unless told
    /// not to. The cursor stays where it was parked, now free to move.
    fn apply_lapse(&mut self) {
        let lapsed = self
            .suppress_until
            .is_some_and(|until| Instant::now() >= until);
        if lapsed
            && !self.lapse_keeps_cursor_parked
            && let Some(at) = self.parked.take()
        {
            self.cursor = at;
            self.log.push("lapse frees the cursor".to_string());
        }
    }

    /// The pretend user's own input.
    pub(crate) fn physical(&mut self, event: InputEvent) {
        self.apply_lapse();
        let before = self.position();
        let suppressed = self.suppressing();
        let mut reported = event;
        match event {
            InputEvent::PointerMotion { dx, dy } if self.parked.is_some() => {
                if self.park_moves_cursor
                    && let Some(at) = self.parked
                {
                    self.parked = Some(self.clamp(Point::new(at.x + dx, at.y + dy)));
                }
            }
            InputEvent::PointerMotion { dx, dy } => {
                if !suppressed || self.suppress_moves_cursor {
                    let to = self.clamp(Point::new(self.cursor.x + dx, self.cursor.y + dy));
                    if self.clamp_attempted_motion {
                        reported = InputEvent::PointerMotion {
                            dx: to.x - self.cursor.x,
                            dy: to.y - self.cursor.y,
                        };
                    }
                    self.cursor = to;
                }
            }
            InputEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            } if !suppressed => self.clicks += 1,
            InputEvent::Key { code, pressed, .. } if !suppressed => self.type_key(code, pressed),
            _ => {}
        }
        self.emit(reported, before, true);
    }

    fn injected(&mut self, event: InputEvent) {
        let before = self.position();
        let swallowed = self.suppress_swallows_injected && self.suppressing();
        match event {
            InputEvent::Key { code, pressed, .. } => {
                if pressed {
                    self.held_keys.insert(code);
                } else {
                    self.held_keys.remove(&code);
                }
                self.injected_key_events += 1;
                if !swallowed && !self.keys_go_elsewhere {
                    self.type_key(code, pressed);
                }
            }
            InputEvent::Button { button, pressed } => {
                if pressed {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
                let at = self.position();
                let on_button =
                    (at.x - self.button_center.x).hypot(at.y - self.button_center.y) <= 10.0;
                if !swallowed && !pressed && button == MouseButton::Left && on_button {
                    self.clicks += self.clicks_per_click;
                }
            }
            InputEvent::PointerMotion { .. } | InputEvent::Scroll { .. } => {}
        }
        if self.capture_sees_injected {
            self.emit(event, before, self.injected_look_physical);
        }
    }

    fn move_to(&mut self, to: Point) {
        let before = self.position();
        let target = self.clamp(Point::new(to.x + self.move_offset, to.y + self.move_offset));
        if self.parked.is_none() {
            self.cursor = target;
        }
        if self.capture_sees_injected {
            let motion = InputEvent::PointerMotion {
                dx: target.x - before.x,
                dy: target.y - before.y,
            };
            self.emit(motion, before, self.injected_look_physical);
        }
    }

    /// The test window's text box reacting to a key.
    fn type_key(&mut self, code: KeyCode, pressed: bool) {
        if !self.window_open {
            return;
        }
        if code == KeyCode::LEFT_SHIFT || code == KeyCode::RIGHT_SHIFT {
            self.shift = pressed;
            return;
        }
        if code == command_key(Os::current()) {
            self.command = pressed;
            return;
        }
        if !pressed {
            return;
        }
        if self.command {
            if code == KEY_A {
                self.select_all = true;
            }
            return;
        }
        let Some(c) = char_for(code) else {
            return;
        };
        let c = if self.shift && !self.shift_broken {
            c.to_ascii_uppercase()
        } else {
            c
        };
        if std::mem::take(&mut self.select_all) {
            self.text.clear();
        }
        self.text.push(c);
    }
}

/// The lowercase character a key types, for the keys the self-test uses.
fn char_for(code: KeyCode) -> Option<char> {
    ('a'..='z')
        .chain('0'..='9')
        .chain([' '])
        .find(|&c| key_for_char(c).is_some_and(|(key, _)| key == code))
}

struct Capture(Shared);

impl InputCapture for Capture {
    fn start(&mut self, sink: Sink) -> Result<()> {
        let mut w = lock(&self.0);
        w.log.push("capture start".to_string());
        if w.capture_fails {
            return Err(PlatformError::Os(
                "the hook could not be installed".to_string(),
            ));
        }
        w.sink = Some(sink);
        Ok(())
    }

    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        let mut w = lock(&self.0);
        match mode {
            CaptureMode::Passive => {
                w.suppress_until = None;
                w.log.push("passive".to_string());
            }
            CaptureMode::Suppress => {
                let lease = if w.lease_never_lapses {
                    Duration::from_secs(3600)
                } else {
                    w.lease
                };
                w.suppress_until = Some(Instant::now() + lease);
                w.log.push("suppress".to_string());
            }
        }
        Ok(())
    }

    fn mode(&self) -> CaptureMode {
        if lock(&self.0).suppressing() {
            CaptureMode::Suppress
        } else {
            CaptureMode::Passive
        }
    }

    fn stop(&mut self) {
        let mut w = lock(&self.0);
        w.sink = None;
        w.log.push("capture stop".to_string());
    }
}

struct Injector(Shared);

impl InputInjector for Injector {
    fn move_cursor_to(&mut self, to: Point) -> Result<()> {
        lock(&self.0).move_to(to);
        Ok(())
    }

    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        lock(&self.0).injected(*event);
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let panics = {
            let mut w = lock(&self.0);
            w.log.push("release all".to_string());
            w.held_keys.clear();
            w.held_buttons.clear();
            w.shift = false;
            w.command = false;
            w.panic_on_release_all
        };
        assert!(!panics, "the pretend injector broke while releasing keys");
        Ok(())
    }
}

struct Cursor(Shared);

impl CursorControl for Cursor {
    fn position(&self) -> Result<Point> {
        let (broken, at) = {
            let mut w = lock(&self.0);
            w.apply_lapse();
            (w.panic_when_parked && w.parked.is_some(), w.position())
        };
        assert!(!broken, "the pretend cursor broke while parked");
        Ok(at)
    }

    fn park(&mut self) -> Result<()> {
        let mut w = lock(&self.0);
        let at = w.cursor;
        w.parked = Some(at);
        w.log.push("park".to_string());
        Ok(())
    }

    fn restore(&mut self, at: Point) -> Result<()> {
        let mut w = lock(&self.0);
        w.parked = None;
        let to = w.clamp(at);
        w.cursor = to;
        w.log.push(format!("restore {} {}", at.x, at.y));
        Ok(())
    }
}

struct Screens(Shared);

impl Displays for Screens {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(lock(&self.0).displays.clone())
    }
}

/// A clipboard the self-test must never touch: every call is logged, so the
/// tests can check that none was made.
struct Clipboard(Shared);

impl TextClipboard for Clipboard {
    fn change_count(&self) -> Result<u64> {
        lock(&self.0).log.push("clipboard".to_string());
        Err(PlatformError::Unsupported)
    }

    fn read_text(&self, _max_bytes: usize) -> Result<Option<String>> {
        lock(&self.0).log.push("clipboard".to_string());
        Err(PlatformError::Unsupported)
    }

    fn write_text(&mut self, _text: &str) -> Result<u64> {
        lock(&self.0).log.push("clipboard".to_string());
        Err(PlatformError::Unsupported)
    }
}

struct Access(Shared);

impl Permissions for Access {
    fn state(&self, permission: Permission) -> PermissionState {
        match permission {
            Permission::Accessibility => lock(&self.0).accessibility,
            Permission::LocalNetwork => PermissionState::NotNeeded,
        }
    }

    fn request(&self, _permission: Permission) -> Result<()> {
        lock(&self.0).log.push("permission request".to_string());
        Ok(())
    }
}

struct System(Shared);

impl SystemState for System {
    fn secure_input_active(&self) -> bool {
        lock(&self.0).secure_input
    }
}

/// The pretend test window.
pub(crate) struct Window(Shared);

impl TestWindow for Window {
    fn show(&mut self, instructions: &str) -> Result<()> {
        let mut w = lock(&self.0);
        w.log.push("window show".to_string());
        if w.window_fails {
            return Err(PlatformError::Unsupported);
        }
        w.window_open = true;
        w.instructions = instructions.to_string();
        Ok(())
    }

    fn set_instructions(&mut self, text: &str) -> Result<()> {
        lock(&self.0).instructions = text.to_string();
        Ok(())
    }

    fn focus_text(&mut self) -> Result<()> {
        Ok(())
    }

    fn text(&self) -> Result<String> {
        Ok(lock(&self.0).text.clone())
    }

    fn clear_text(&mut self) -> Result<()> {
        let mut w = lock(&self.0);
        w.text.clear();
        w.select_all = false;
        Ok(())
    }

    fn button_center(&self) -> Result<Point> {
        let (broken, at) = {
            let w = lock(&self.0);
            (w.panic_on_button_center, w.button_center)
        };
        assert!(!broken, "the pretend window broke");
        Ok(at)
    }

    fn button_clicks(&self) -> Result<u32> {
        Ok(lock(&self.0).clicks)
    }

    fn close(&mut self) {
        let mut w = lock(&self.0);
        w.window_open = false;
        w.instructions.clear();
        w.log.push("window close".to_string());
    }
}

/// What the pretend user does.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Script {
    /// Does what step 2 asks: moves, clicks, scrolls and presses the copy
    /// shortcut.
    pub follows_capture: bool,
    /// Moves the mouse back and forth in steps 5 and 6.
    pub wiggles: bool,
    /// In step 7, moves sideways by this much per event (positive is right).
    pub push: Option<f64>,
    /// Presses the emergency chord as soon as this step's instructions show.
    pub chord_in: Option<StepId>,
    /// Presses the emergency chord while the cursor is parked.
    pub chord_while_parked: bool,
    /// Clicks the button whenever a step asks to be started.
    pub clicks_start: bool,
}

impl Script {
    /// A user who does everything asked, and nothing else.
    pub(crate) fn cooperative() -> Self {
        Self {
            follows_capture: true,
            wiggles: true,
            push: Some(25.0),
            clicks_start: true,
            ..Self::default()
        }
    }
}

/// A pretend machine with a pretend user.
pub(crate) struct Fake {
    pub world: Shared,
    pub platform: Platform,
    pub window: Window,
    stop: Arc<AtomicBool>,
    user: Option<JoinHandle<()>>,
}

impl Fake {
    /// A well-behaved machine changed by `setup`, with a user following
    /// `script`.
    pub(crate) fn new(script: Script, setup: impl FnOnce(&mut World)) -> Self {
        let mut world = World::default();
        setup(&mut world);
        let world = Arc::new(Mutex::new(world));
        let platform = Platform {
            capture: Box::new(Capture(Arc::clone(&world))),
            injector: Box::new(Injector(Arc::clone(&world))),
            cursor: Box::new(Cursor(Arc::clone(&world))),
            displays: Box::new(Screens(Arc::clone(&world))),
            clipboard: Box::new(Clipboard(Arc::clone(&world))),
            drops: Box::new(NoDrops),
            permissions: Box::new(Access(Arc::clone(&world))),
            system: Box::new(System(Arc::clone(&world))),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let user = spawn_user(Arc::clone(&world), script, Arc::clone(&stop));
        Self {
            window: Window(Arc::clone(&world)),
            world,
            platform,
            stop,
            user: Some(user),
        }
    }

    pub(crate) fn run(&mut self, options: &SelfTestOptions, steps: &[StepId]) -> Report {
        crate::run_selected(&mut self.platform, &mut self.window, options, steps)
    }

    pub(crate) fn world(&self) -> MutexGuard<'_, World> {
        lock(&self.world)
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(user) = self.user.take() {
            let _ = user.join();
        }
    }
}

/// The step whose instructions the window shows, read from its heading.
fn showing(instructions: &str) -> Option<StepId> {
    let heading = instructions.lines().next()?;
    StepId::ALL
        .into_iter()
        .find(|step| step.heading() == heading)
}

fn key(code: KeyCode, pressed: bool) -> InputEvent {
    InputEvent::Key {
        code,
        pressed,
        repeat: false,
    }
}

fn press_chord(w: &mut World) {
    let chord = [
        KeyCode::LEFT_CONTROL,
        KeyCode::LEFT_ALT,
        KeyCode::LEFT_SHIFT,
        KeyCode::ESCAPE,
    ];
    for code in chord {
        w.physical(key(code, true));
    }
    for code in chord.into_iter().rev() {
        w.physical(key(code, false));
    }
}

fn spawn_user(world: Shared, script: Script, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut pressed_chord = false;
        let mut wiggle = 4.0;
        while !stop.load(Ordering::SeqCst) {
            {
                let mut w = lock(&world);
                let step = showing(&w.instructions);
                if !pressed_chord
                    && ((step.is_some() && step == script.chord_in)
                        || (script.chord_while_parked && w.parked.is_some()))
                {
                    pressed_chord = true;
                    press_chord(&mut w);
                }
                wiggle = -wiggle;
                if script.clicks_start && w.instructions.contains(START_PROMPT) {
                    for pressed in [true, false] {
                        w.physical(InputEvent::Button {
                            button: MouseButton::Left,
                            pressed,
                        });
                    }
                }
                match step {
                    Some(StepId::Capture) if script.follows_capture => {
                        let command = command_key(Os::current());
                        let events = [
                            InputEvent::PointerMotion {
                                dx: wiggle,
                                dy: 1.0,
                            },
                            InputEvent::Button {
                                button: MouseButton::Left,
                                pressed: true,
                            },
                            InputEvent::Button {
                                button: MouseButton::Left,
                                pressed: false,
                            },
                            InputEvent::Scroll {
                                dx: 0.0,
                                dy: 1.0,
                                unit: ScrollUnit::Line,
                            },
                            key(command, true),
                            key(KEY_C, true),
                            key(KEY_C, false),
                            key(command, false),
                        ];
                        for event in events {
                            w.physical(event);
                        }
                    }
                    Some(StepId::Suppression | StepId::Park) if script.wiggles => {
                        w.physical(InputEvent::PointerMotion {
                            dx: wiggle,
                            dy: 0.0,
                        });
                    }
                    Some(StepId::EdgePush) => {
                        if let Some(dx) = script.push {
                            w.physical(InputEvent::PointerMotion { dx, dy: 0.0 });
                        }
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    })
}
