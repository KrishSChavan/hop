//! A fake platform and a fake network for tests. Nothing here touches the
//! OS: the "cursor" is a number, injected events are only recorded, and
//! captured events are whatever the test hands in.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hop_net::{NetEvent, SendError};
use hop_platform::{
    CaptureMode, CapturedEvent, CursorControl, Displays, InputCapture, InputInjector, Permission,
    PermissionState, Permissions, Platform, PlatformError, Result, SystemState, TextClipboard,
};
use hop_types::{
    CoreMessage, DeviceId, DisplayInfo, InputEvent, KeyCode, MouseButton, Point, Rect,
};

use super::link::Link;

/// One platform call, as the fake saw it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Call {
    StartCapture,
    StopCapture,
    SetMode(CaptureMode),
    Park,
    Restore(Point),
    Move(Point),
    Inject(InputEvent),
    ReleaseAll,
}

type Sink = Box<dyn FnMut(CapturedEvent) + Send>;

/// Everything the fake platform remembers. Tests read it and change it.
pub(crate) struct FakeState {
    /// Every call, in order.
    pub(crate) calls: Vec<Call>,
    sink: Option<Sink>,
    /// The capture mode in force. Setting it to `Passive` from a test is
    /// what a lapsed lease looks like.
    pub(crate) mode: CaptureMode,
    pub(crate) cursor: Point,
    pub(crate) parked: bool,
    /// Keys and buttons injected and not yet released.
    pub(crate) keys_down: BTreeSet<KeyCode>,
    pub(crate) buttons_down: BTreeSet<MouseButton>,
    pub(crate) displays: Vec<DisplayInfo>,
    pub(crate) fail_inject: bool,
    pub(crate) fail_suppress: bool,
    pub(crate) fail_park: bool,
}

impl FakeState {
    /// The key events injected so far, as (key, pressed).
    pub(crate) fn injected_keys(&self) -> Vec<(KeyCode, bool)> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Inject(InputEvent::Key { code, pressed, .. }) => Some((*code, *pressed)),
                _ => None,
            })
            .collect()
    }
}

/// The test's handle on a fake platform.
#[derive(Clone)]
pub(crate) struct Fake {
    state: Arc<Mutex<FakeState>>,
}

impl Fake {
    /// A fake platform with one display of `width` x `height`, the cursor
    /// in its middle.
    pub(crate) fn new(width: f64, height: f64) -> (Platform, Fake) {
        let display = DisplayInfo {
            id: 1,
            frame: Rect::new(0.0, 0.0, width, height),
            scale: 1.0,
            primary: true,
        };
        let state = Arc::new(Mutex::new(FakeState {
            calls: Vec::new(),
            sink: None,
            mode: CaptureMode::Passive,
            cursor: display.frame.center(),
            parked: false,
            keys_down: BTreeSet::new(),
            buttons_down: BTreeSet::new(),
            displays: vec![display],
            fail_inject: false,
            fail_suppress: false,
            fail_park: false,
        }));
        let part = || Part(state.clone());
        let platform = Platform {
            capture: Box::new(part()),
            injector: Box::new(part()),
            cursor: Box::new(part()),
            displays: Box::new(part()),
            clipboard: Box::new(part()),
            permissions: Box::new(part()),
            system: Box::new(part()),
        };
        (platform, Fake { state })
    }

    /// The fake's state, locked.
    pub(crate) fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands a physical event to capture's sink, as the capture thread
    /// would. While capture is passive, motion moves the cursor the way an
    /// OS does: stopped at the screen's edge.
    pub(crate) fn physical(&self, event: InputEvent) {
        let (sink, cursor) = {
            let mut state = self.state();
            let cursor = state.cursor;
            if let (CaptureMode::Passive, InputEvent::PointerMotion { dx, dy }) =
                (state.mode, event)
                && let Some(display) = state.displays.first()
            {
                state.cursor = display
                    .frame
                    .clamp(Point::new(cursor.x + dx, cursor.y + dy), 1.0);
            }
            (state.sink.take(), cursor)
        };
        let Some(mut sink) = sink else {
            return;
        };
        sink(CapturedEvent {
            event,
            cursor,
            physical: true,
        });
        self.state().sink = Some(sink);
    }

    /// Presses or releases a key on this fake computer's keyboard.
    pub(crate) fn key(&self, code: KeyCode, pressed: bool) {
        self.physical(InputEvent::Key {
            code,
            pressed,
            repeat: false,
        });
    }

    /// Moves this fake computer's mouse.
    pub(crate) fn motion(&self, dx: f64, dy: f64) {
        self.physical(InputEvent::PointerMotion { dx, dy });
    }
}

/// One of the platform's parts; they all share the state.
struct Part(Arc<Mutex<FakeState>>);

impl Part {
    fn state(&self) -> MutexGuard<'_, FakeState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn refused(what: &str) -> PlatformError {
    PlatformError::Os(format!("the fake refuses to {what}"))
}

impl InputCapture for Part {
    fn start(&mut self, sink: Sink) -> Result<()> {
        let mut state = self.state();
        state.sink = Some(sink);
        state.calls.push(Call::StartCapture);
        Ok(())
    }

    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        let mut state = self.state();
        if mode == CaptureMode::Suppress && state.fail_suppress {
            return Err(refused("suppress input"));
        }
        state.mode = mode;
        state.calls.push(Call::SetMode(mode));
        Ok(())
    }

    fn mode(&self) -> CaptureMode {
        self.state().mode
    }

    fn stop(&mut self) {
        let mut state = self.state();
        state.sink = None;
        state.mode = CaptureMode::Passive;
        state.calls.push(Call::StopCapture);
    }
}

impl InputInjector for Part {
    fn move_cursor_to(&mut self, to: Point) -> Result<()> {
        let mut state = self.state();
        if state.fail_inject {
            return Err(refused("move the cursor"));
        }
        state.cursor = to;
        state.calls.push(Call::Move(to));
        Ok(())
    }

    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        let mut state = self.state();
        if state.fail_inject {
            return Err(refused("inject"));
        }
        match *event {
            InputEvent::Key { code, pressed, .. } => {
                if pressed {
                    state.keys_down.insert(code);
                } else {
                    state.keys_down.remove(&code);
                }
            }
            InputEvent::Button { button, pressed } => {
                if pressed {
                    state.buttons_down.insert(button);
                } else {
                    state.buttons_down.remove(&button);
                }
            }
            InputEvent::PointerMotion { .. } | InputEvent::Scroll { .. } => {}
        }
        state.calls.push(Call::Inject(*event));
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let mut state = self.state();
        state.keys_down.clear();
        state.buttons_down.clear();
        state.calls.push(Call::ReleaseAll);
        Ok(())
    }
}

impl CursorControl for Part {
    fn position(&self) -> Result<Point> {
        Ok(self.state().cursor)
    }

    fn park(&mut self) -> Result<()> {
        let mut state = self.state();
        if state.fail_park {
            return Err(refused("park the cursor"));
        }
        state.parked = true;
        state.calls.push(Call::Park);
        Ok(())
    }

    fn restore(&mut self, at: Point) -> Result<()> {
        let mut state = self.state();
        state.parked = false;
        state.cursor = at;
        state.calls.push(Call::Restore(at));
        Ok(())
    }
}

impl Displays for Part {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(self.state().displays.clone())
    }
}

impl TextClipboard for Part {
    fn change_count(&self) -> u64 {
        0
    }

    fn read_text(&self) -> Result<Option<String>> {
        Err(PlatformError::Unsupported)
    }

    fn write_text(&mut self, _: &str) -> Result<()> {
        Err(PlatformError::Unsupported)
    }
}

impl Permissions for Part {
    fn state(&self, _: Permission) -> PermissionState {
        PermissionState::Granted
    }

    fn request(&self, _: Permission) -> Result<()> {
        Ok(())
    }
}

impl SystemState for Part {
    fn secure_input_active(&self) -> bool {
        false
    }
}

/// What the fake network remembers.
#[derive(Default)]
pub(crate) struct FakeNet {
    /// Every message the agent sent, in order.
    pub(crate) sent: Vec<(DeviceId, CoreMessage)>,
    /// Events waiting for the agent.
    pub(crate) incoming: VecDeque<NetEvent>,
    /// The displays the agent last announced, if it changed them.
    pub(crate) announced: Option<Vec<DisplayInfo>>,
}

/// A fake network: sends are recorded, events are whatever the test queues.
#[derive(Clone, Default)]
pub(crate) struct FakeLink(Arc<Mutex<FakeNet>>);

impl FakeLink {
    /// The fake network's state, locked.
    pub(crate) fn net(&self) -> MutexGuard<'_, FakeNet> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `event` for the agent.
    pub(crate) fn deliver(&self, event: NetEvent) {
        self.net().incoming.push_back(event);
    }

    /// Every message sent so far, without the recipient.
    pub(crate) fn sent(&self) -> Vec<CoreMessage> {
        self.net().sent.iter().map(|(_, msg)| msg.clone()).collect()
    }
}

impl Link for FakeLink {
    fn send(&self, to: &DeviceId, msg: CoreMessage) -> std::result::Result<(), SendError> {
        self.net().sent.push((to.clone(), msg));
        Ok(())
    }

    fn try_recv(&self) -> Option<NetEvent> {
        self.net().incoming.pop_front()
    }

    fn set_displays(&self, displays: Vec<DisplayInfo>) {
        self.net().announced = Some(displays);
    }

    fn local_network_denied(&self) -> bool {
        false
    }
}
