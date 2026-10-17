//! Injecting input with `SendInput`, tagged so capture knows it is Hop's.

use std::collections::BTreeMap;

use hop_keymap::ButtonTracker;
use hop_platform::{INJECTED_TAG, InputInjector, PlatformError, Result};
use hop_types::{InputEvent, KeyCode, MouseButton, Point, ScrollUnit};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};
use windows::core::Error as WinError;

use crate::logic::coords::{normalize, to_physical};
use crate::logic::keys::{KeyStroke, stroke_for};
use crate::logic::scroll::WheelAccumulator;
use crate::os;

// SendInput is told the size of one INPUT and walks the array by it, so the
// binding must match the Windows SDK's x64 layout exactly. Checked when
// compiling, so a change in the bindings cannot slip through.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<INPUT>() == 40);

/// The contract's injector. Remembers every key and button it holds down,
/// so a session can always end with nothing stuck.
#[derive(Debug, Default)]
pub(crate) struct WinInjector {
    /// Held keys, with the stroke used at key-down so the key-up matches it.
    keys: BTreeMap<KeyCode, KeyStroke>,
    buttons: ButtonTracker,
    wheel: WheelAccumulator,
}

impl InputInjector for WinInjector {
    fn move_cursor_to(&mut self, to: Point) -> Result<()> {
        let screen = os::virtual_screen();
        let target = to_physical(to, os::primary_scale());
        let x = normalize(target.x, screen.left, screen.width());
        let y = normalize(target.y, screen.top, screen.height());
        // Absolute over the whole virtual desktop, so every monitor is
        // reachable. Held buttons stay held, which makes this a drag.
        let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        send(&[mouse_input(x, y, 0, flags)], "move the cursor")
    }

    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match *event {
            // Motion arrives through `move_cursor_to`.
            InputEvent::PointerMotion { .. } => Ok(()),
            InputEvent::Button { button, pressed } => self.button(button, pressed),
            InputEvent::Key { code, pressed, .. } => self.key(code, pressed),
            InputEvent::Scroll { dx, dy, unit } => self.scroll(dx, dy, unit),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let mut first_error = None;
        // Buttons first, so a drag ends while its modifiers are still in the
        // state the user had.
        for button in self.buttons.release_all() {
            if let Err(error) = send(&[button_input(button, false)], "release a mouse button") {
                first_error.get_or_insert(error);
            }
        }
        // Then keys. Modifiers sort after the other keyboard keys, so a
        // shortcut like Ctrl+C lets go of C before Ctrl.
        for stroke in std::mem::take(&mut self.keys).into_values() {
            if let Err(error) = send(&[key_input(stroke, false)], "release a key") {
                first_error.get_or_insert(error);
            }
        }
        self.wheel.reset();
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for WinInjector {
    fn drop(&mut self) {
        // Fail open: never leave a key or button down.
        let _ = self.release_all();
    }
}

impl WinInjector {
    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let what = if pressed {
            "press a mouse button"
        } else {
            "release a mouse button"
        };
        let result = send(&[button_input(button, pressed)], what);
        if !pressed {
            self.buttons.release(button);
        } else if result.is_ok() {
            self.buttons.press(button);
        }
        result
    }

    fn key(&mut self, code: KeyCode, pressed: bool) -> Result<()> {
        if pressed {
            // A key with no Windows equivalent is skipped, as the contract
            // says. An auto-repeat reuses the stroke of the first key-down.
            let Some(stroke) = self.keys.get(&code).copied().or_else(|| stroke_for(code)) else {
                return Ok(());
            };
            let result = send(&[key_input(stroke, true)], "press a key");
            if result.is_ok() {
                self.keys.insert(code, stroke);
            }
            result
        } else {
            // Release with the stroke used to press, if Hop pressed it.
            let Some(stroke) = self.keys.remove(&code).or_else(|| stroke_for(code)) else {
                return Ok(());
            };
            send(&[key_input(stroke, false)], "release a key")
        }
    }

    fn scroll(&mut self, dx: f64, dy: f64, unit: ScrollUnit) -> Result<()> {
        let (horizontal, vertical) = self.wheel.take(dx, dy, unit);
        let mut inputs = Vec::with_capacity(2);
        // mouseData is a DWORD that Windows reads as signed for wheels.
        if vertical != 0 {
            inputs.push(mouse_input(0, 0, vertical as u32, MOUSEEVENTF_WHEEL));
        }
        if horizontal != 0 {
            inputs.push(mouse_input(0, 0, horizontal as u32, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        send(&inputs, "scroll")
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_TAG as usize,
            },
        },
    }
}

fn button_input(button: MouseButton, pressed: bool) -> INPUT {
    let pick = |down, up| if pressed { down } else { up };
    let (flags, data) = match button {
        MouseButton::Left => (pick(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP), 0),
        MouseButton::Right => (pick(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP), 0),
        MouseButton::Middle => (pick(MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP), 0),
        MouseButton::Back => (
            pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
            u32::from(XBUTTON1),
        ),
        MouseButton::Forward => (
            pick(MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
            u32::from(XBUTTON2),
        ),
    };
    mouse_input(0, 0, data, flags)
}

fn key_input(stroke: KeyStroke, pressed: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if stroke.by_scancode {
        flags |= KEYEVENTF_SCANCODE;
    }
    if stroke.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(stroke.vk),
                wScan: stroke.scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_TAG as usize,
            },
        },
    }
}

/// Sends `inputs` as one batch (Windows inserts a batch without other input
/// in between).
fn send(inputs: &[INPUT], what: &str) -> Result<()> {
    // SAFETY: `inputs` are fully initialized INPUT structures, and the size
    // passed is that of one INPUT, as SendInput requires.
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        return Ok(());
    }
    let error = WinError::from_thread();
    Err(PlatformError::Os(format!(
        "SendInput could not {what} ({sent} of {} events went in): {error}. Windows also \
         refuses input into apps running as administrator while Hop is not.",
        inputs.len()
    )))
}
