//! A fake platform and a fake network for tests. Nothing here touches the
//! OS: the "cursor" is a number, injected events are only recorded, and
//! captured events are whatever the test hands in.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hop_net::{NetEvent, SendError};
use hop_platform::{
    CLIPBOARD_TIMEOUT, CaptureMode, CapturedEvent, CursorControl, Displays, DropCatcher, DropZone,
    InputCapture, InputInjector, Permission, PermissionState, Permissions, Platform, PlatformError,
    Result, SystemState, TextClipboard,
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
    Conceal,
    Reveal,
    Move(Point),
    Inject(InputEvent),
    ReleaseAll,
    /// The clipboard's text was read.
    ReadClipboard,
    /// Text was put on the clipboard (it is in [`FakeState::clipboard`]).
    WriteClipboard,
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
    /// Whether the cursor is hidden where a session left it.
    pub(crate) concealed: bool,
    /// Keys and buttons injected and not yet released.
    pub(crate) keys_down: BTreeSet<KeyCode>,
    pub(crate) buttons_down: BTreeSet<MouseButton>,
    pub(crate) displays: Vec<DisplayInfo>,
    pub(crate) fail_inject: bool,
    pub(crate) fail_suppress: bool,
    pub(crate) fail_park: bool,
    /// Hiding the cursor and showing it again fail.
    pub(crate) fail_conceal: bool,
    /// The clipboard's text, if it holds any, and its change count.
    pub(crate) clipboard: Option<String>,
    pub(crate) clipboard_count: u64,
    /// Reading the change count fails.
    pub(crate) fail_clipboard_count: bool,
    /// Reading the clipboard's text fails, as a read that took too long
    /// does.
    pub(crate) fail_clipboard_read: bool,
    /// Writing the clipboard fails.
    pub(crate) fail_clipboard_write: bool,
    /// Every clipboard call waits [`CLIPBOARD_TIMEOUT`] and then fails, as
    /// every call does on a Mac whose clipboard waits on an app that is
    /// slow to hand over what it copied.
    pub(crate) stall_clipboard: bool,
    /// How many times the clipboard's text was read.
    pub(crate) clipboard_reads: u32,
    /// How many times the clipboard was asked anything (its change count,
    /// its text, or to take text), whether it answered or not.
    pub(crate) clipboard_calls: u32,
    /// The drop zones the agent set last, and the label it gave them.
    pub(crate) zones: Vec<DropZone>,
    pub(crate) label: String,
    /// There is no drop target: setting zones fails as with
    /// [`hop_platform::NoDrops`], and clearing them works.
    pub(crate) no_drop_target: bool,
    /// Drops waiting for the agent, oldest first.
    pub(crate) drops: VecDeque<Vec<PathBuf>>,
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
            concealed: false,
            keys_down: BTreeSet::new(),
            buttons_down: BTreeSet::new(),
            displays: vec![display],
            fail_inject: false,
            fail_suppress: false,
            fail_park: false,
            fail_conceal: false,
            clipboard: None,
            clipboard_count: 0,
            fail_clipboard_count: false,
            fail_clipboard_read: false,
            fail_clipboard_write: false,
            stall_clipboard: false,
            clipboard_reads: 0,
            clipboard_calls: 0,
            zones: Vec::new(),
            label: String::new(),
            no_drop_target: false,
            drops: VecDeque::new(),
        }));
        let part = || Part(state.clone());
        let platform = Platform {
            capture: Box::new(part()),
            injector: Box::new(part()),
            cursor: Box::new(part()),
            displays: Box::new(part()),
            clipboard: Box::new(part()),
            drops: Box::new(part()),
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

    /// Copies `text` in some app on this fake computer: the clipboard holds
    /// it, and its change count moves on.
    pub(crate) fn copy(&self, text: &str) {
        let mut state = self.state();
        state.clipboard = Some(text.to_owned());
        state.clipboard_count += 1;
    }

    /// Drops `paths` on the drop zone, as the drop target would hand them
    /// to the agent.
    pub(crate) fn drop_files(&self, paths: &[&str]) {
        let paths = paths.iter().map(PathBuf::from).collect();
        self.state().drops.push_back(paths);
    }
}

/// One of the platform's parts; they all share the state.
struct Part(Arc<Mutex<FakeState>>);

impl Part {
    fn state(&self) -> MutexGuard<'_, FakeState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a clipboard call. While the clipboard stalls, the call waits
    /// as long as a real one may, without holding the state, and then
    /// fails as a real one does.
    fn clipboard_call(&self) -> Result<()> {
        let stalled = {
            let mut state = self.state();
            state.clipboard_calls += 1;
            state.stall_clipboard
        };
        if stalled {
            std::thread::sleep(CLIPBOARD_TIMEOUT);
            return Err(PlatformError::Os(format!(
                "the fake clipboard did not answer within {} ms",
                CLIPBOARD_TIMEOUT.as_millis()
            )));
        }
        Ok(())
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
        // Parking ends a conceal, as the contract says.
        state.concealed = false;
        state.calls.push(Call::Park);
        Ok(())
    }

    fn restore(&mut self, at: Point) -> Result<()> {
        let mut state = self.state();
        state.parked = false;
        state.concealed = false;
        state.cursor = at;
        state.calls.push(Call::Restore(at));
        Ok(())
    }

    fn conceal(&mut self) -> Result<()> {
        let mut state = self.state();
        if state.fail_conceal {
            return Err(refused("hide the cursor"));
        }
        state.concealed = true;
        state.calls.push(Call::Conceal);
        Ok(())
    }

    fn reveal(&mut self) -> Result<()> {
        let mut state = self.state();
        if state.fail_conceal {
            return Err(refused("show the cursor"));
        }
        state.concealed = false;
        state.calls.push(Call::Reveal);
        Ok(())
    }
}

impl Displays for Part {
    fn displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(self.state().displays.clone())
    }
}

/// A clipboard in memory; no test reads or writes the real one.
impl TextClipboard for Part {
    fn change_count(&self) -> Result<u64> {
        self.clipboard_call()?;
        let state = self.state();
        if state.fail_clipboard_count {
            return Err(refused("read the clipboard's change count"));
        }
        Ok(state.clipboard_count)
    }

    fn read_text(&self, max_bytes: usize) -> Result<Option<String>> {
        self.clipboard_call()?;
        let mut state = self.state();
        if state.fail_clipboard_read {
            return Err(refused("read the clipboard"));
        }
        state.clipboard_reads += 1;
        state.calls.push(Call::ReadClipboard);
        Ok(state
            .clipboard
            .clone()
            .filter(|text| text.len() <= max_bytes))
    }

    fn write_text(&mut self, text: &str) -> Result<u64> {
        self.clipboard_call()?;
        let mut state = self.state();
        if state.fail_clipboard_write {
            return Err(refused("write the clipboard"));
        }
        state.clipboard = Some(text.to_owned());
        state.clipboard_count += 1;
        state.calls.push(Call::WriteClipboard);
        Ok(state.clipboard_count)
    }
}

impl DropCatcher for Part {
    fn set_zones(&mut self, zones: Vec<DropZone>, label: &str) -> Result<()> {
        let mut state = self.state();
        if state.no_drop_target && !zones.is_empty() {
            return Err(PlatformError::Unsupported);
        }
        state.zones = zones;
        state.label = label.to_owned();
        Ok(())
    }

    fn take_drops(&mut self) -> Vec<Vec<PathBuf>> {
        self.state().drops.drain(..).collect()
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
    /// Every set of files the agent sent, in order, with the recipient.
    pub(crate) files: Vec<(DeviceId, Vec<PathBuf>)>,
    /// Every peer the agent nudged, in order.
    pub(crate) nudges: Vec<DeviceId>,
    /// While set, clipboard text is not sent but refused with the error
    /// this makes; every other message still goes.
    pub(crate) refuse_clipboard: Option<fn() -> SendError>,
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

    /// Every set of files sent so far, with the recipient.
    pub(crate) fn sent_files(&self) -> Vec<(DeviceId, Vec<PathBuf>)> {
        self.net().files.clone()
    }

    /// Every clipboard text sent so far, with the recipient.
    pub(crate) fn sent_clipboard(&self) -> Vec<(DeviceId, String)> {
        self.net()
            .sent
            .iter()
            .filter_map(|(to, msg)| match msg {
                CoreMessage::ClipboardText { text, .. } => Some((to.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Every peer nudged so far, in order.
    pub(crate) fn nudges(&self) -> Vec<DeviceId> {
        self.net().nudges.clone()
    }
}

impl Link for FakeLink {
    fn send(&self, to: &DeviceId, msg: CoreMessage) -> std::result::Result<(), SendError> {
        let mut net = self.net();
        if let (CoreMessage::ClipboardText { .. }, Some(refuse)) = (&msg, net.refuse_clipboard) {
            return Err(refuse());
        }
        net.sent.push((to.clone(), msg));
        Ok(())
    }

    /// Records the files; transfers are numbered from 1, in order.
    fn send_files(
        &self,
        to: &DeviceId,
        paths: Vec<PathBuf>,
    ) -> std::result::Result<u64, SendError> {
        let mut net = self.net();
        net.files.push((to.clone(), paths));
        Ok(u64::try_from(net.files.len()).unwrap_or(u64::MAX))
    }

    fn nudge(&self, to: &DeviceId) {
        self.net().nudges.push(to.clone());
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
