//! The self-test window: an instruction line, a text field and a button, so
//! `hop selftest` can check injection without touching any other app.
//!
//! The window runs on a UI thread of its own with its own message loop.
//! Every method hands its work to that thread (see `worker`) and waits for
//! the answer, so the window is only ever touched by the thread that owns
//! it. It stays on top, so an injected click at the button cannot land in
//! another app's window.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hop_platform::{PlatformError, Result, TestWindow};
use hop_types::Point;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, VK_CONTROL, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, BS_PUSHBUTTON, BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow,
    ES_AUTOVSCROLL, ES_MULTILINE, ES_WANTRETURN, GetForegroundWindow, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, HMENU, IDC_ARROW, LoadCursorW,
    MSG, NONCLIENTMETRICSW, PostQuitMessage, SPI_GETNONCLIENTMETRICS, SW_SHOW, SWP_NOACTIVATE,
    SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_SETFONT,
    WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_TOPMOST, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::logic::coords::{BASE_DPI, PhysRect};
use crate::logic::keys::is_select_all;
use crate::os;
use crate::worker::{self, WM_APP_WAKE, Worker};

const CLASS: PCWSTR = w!("HopSelfTestWindow");

/// Child window ids, which `WM_COMMAND` reports.
const BUTTON_ID: usize = 3;

/// How long a caller waits for the window's thread.
const UI_TIMEOUT: Duration = Duration::from_secs(5);

/// The layout, in 96-DPI pixels, scaled to the window's DPI.
///
/// The instructions get most of the window. The self-test's longest step
/// text, with its heading, the countdown and the line saying how to stop
/// (the emergency chord), runs to about ten lines, and those last lines are
/// the ones that matter most; at the message font's roughly 15 pixels a
/// line, the label holds fourteen. A label clips what does not fit.
const CLIENT_WIDTH: i32 = 520;
const CLIENT_HEIGHT: i32 = 380;
const MARGIN: i32 = 16;
const LABEL_TOP: i32 = 12;
const LABEL_HEIGHT: i32 = 216;
const EDIT_TOP: i32 = 240;
const EDIT_HEIGHT: i32 = 72;
const BUTTON_TOP: i32 = 324;
const BUTTON_WIDTH: i32 = 160;
const BUTTON_HEIGHT: i32 = 40;

/// `SS_NOPREFIX`: show `&` as itself instead of underlining the next
/// character, since the instructions are arbitrary text.
const SS_NOPREFIX: u32 = 0x80;

/// `EM_SETSEL`, an edit control's "select this range" message.
const EM_SETSEL: u32 = 0x00B1;

const MAIN_STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);
/// Topmost, so injected clicks cannot land in a window that covers it.
const MAIN_EX_STYLE: WINDOW_EX_STYLE = WS_EX_TOPMOST;

/// The contract's test window.
pub(crate) struct WinTestWindow {
    /// `None` once closed.
    ui: Option<Ui>,
    clicks: Arc<AtomicU32>,
}

struct Ui {
    worker: Worker,
    thread: JoinHandle<()>,
}

/// The window's parts, on its thread.
struct Controls {
    main: HWND,
    label: HWND,
    edit: HWND,
    button: HWND,
    font: Option<HFONT>,
}

thread_local! {
    /// On the window's thread only.
    static CONTROLS: RefCell<Option<Controls>> = const { RefCell::new(None) };
    static CLICKS: RefCell<Option<Arc<AtomicU32>>> = const { RefCell::new(None) };
}

impl WinTestWindow {
    /// Creates the window, hidden, on a new UI thread.
    pub(crate) fn open() -> Result<Self> {
        let clicks = Arc::new(AtomicU32::new(0));
        let (ready, started) = mpsc::sync_channel(1);
        let thread_clicks = Arc::clone(&clicks);
        let thread = thread::Builder::new()
            .name("hop-selftest-window".into())
            .spawn(move || main(&ready, thread_clicks))
            .map_err(|error| {
                PlatformError::Os(format!(
                    "could not start the self-test window's thread: {error}"
                ))
            })?;
        let worker = started
            .recv_timeout(UI_TIMEOUT)
            .map_err(|_| PlatformError::Os("the self-test window did not open".into()))?
            .map_err(PlatformError::Os)?;
        Ok(Self {
            ui: Some(Ui { worker, thread }),
            clicks,
        })
    }

    /// Runs `job` with the window's parts on its thread.
    fn with_controls<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Controls) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let ui = self.ui.as_ref().ok_or_else(closed)?;
        ui.worker.run(UI_TIMEOUT, move || {
            CONTROLS
                .try_with(|controls| match controls.try_borrow() {
                    Ok(controls) => controls.as_ref().map_or_else(|| Err(closed()), job),
                    Err(_) => Err(PlatformError::Os("the self-test window is busy".into())),
                })
                .unwrap_or_else(|_| Err(closed()))
        })?
    }
}

fn closed() -> PlatformError {
    PlatformError::Os("the self-test window is closed".into())
}

impl TestWindow for WinTestWindow {
    fn show(&mut self, instructions: &str) -> Result<()> {
        let text = HSTRING::from(instructions);
        self.with_controls(move |controls| {
            set_text(controls.label, &text)?;
            // SAFETY: the window belongs to this thread.
            let _ = unsafe { ShowWindow(controls.main, SW_SHOW) };
            focus_text_field(controls)
        })
    }

    fn set_instructions(&mut self, text: &str) -> Result<()> {
        let text = HSTRING::from(text);
        self.with_controls(move |controls| set_text(controls.label, &text))
    }

    fn focus_text(&mut self) -> Result<()> {
        self.with_controls(focus_text_field)
    }

    fn text(&self) -> Result<String> {
        self.with_controls(|controls| Ok(read_text(controls.edit)))
    }

    fn clear_text(&mut self) -> Result<()> {
        self.with_controls(|controls| set_text(controls.edit, &HSTRING::new()))
    }

    fn button_center(&self) -> Result<Point> {
        let rect = self.with_controls(|controls| {
            let mut rect = RECT::default();
            // SAFETY: `rect` is a valid out-pointer; the button is ours.
            unsafe { GetWindowRect(controls.button, &mut rect) }
                .map_err(|error| os::os_error("GetWindowRect", &error))?;
            Ok(PhysRect::new(rect.left, rect.top, rect.right, rect.bottom))
        })?;
        let scale = os::primary_scale();
        Ok(Point::new(
            (f64::from(rect.left) + f64::from(rect.right)) / 2.0 / scale,
            (f64::from(rect.top) + f64::from(rect.bottom)) / 2.0 / scale,
        ))
    }

    fn button_clicks(&self) -> Result<u32> {
        if self.ui.is_none() {
            return Err(closed());
        }
        Ok(self.clicks.load(Ordering::Acquire))
    }

    fn close(&mut self) {
        let Some(ui) = self.ui.take() else {
            return;
        };
        let destroyed = ui.worker.run(UI_TIMEOUT, || {
            let main = CONTROLS
                .try_with(|controls| {
                    controls
                        .try_borrow()
                        .ok()
                        .and_then(|c| c.as_ref().map(|c| c.main))
                })
                .ok()
                .flatten();
            if let Some(main) = main {
                // SAFETY: the window belongs to this thread. WM_DESTROY ends
                // the thread's message loop.
                let _ = unsafe { DestroyWindow(main) };
            }
        });
        // Only wait for a thread that is known to be finishing.
        if destroyed.is_ok() {
            let _ = ui.thread.join();
        }
    }
}

impl Drop for WinTestWindow {
    fn drop(&mut self) {
        self.close();
    }
}

/// The window's thread.
fn main(ready: &mpsc::SyncSender<std::result::Result<Worker, String>>, clicks: Arc<AtomicU32>) {
    CLICKS.with(|slot| *slot.borrow_mut() = Some(clicks));
    let controls = match create() {
        Ok(controls) => controls,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let worker = Worker::install(controls.main, "the self-test window");
    let edit = controls.edit;
    CONTROLS.with(|slot| *slot.borrow_mut() = Some(controls));
    if ready.send(Ok(worker)).is_err() {
        destroy_all();
        return;
    }
    worker::message_loop_with(|msg| select_all_shortcut(msg, edit));
    destroy_all();
}

/// Ctrl+A in the text field selects all of it (see
/// `logic::keys::is_select_all`). Handled before the key is translated, so
/// it never also becomes a control character in the text.
fn select_all_shortcut(msg: &MSG, edit: HWND) -> bool {
    if msg.hwnd != edit {
        return false;
    }
    // SAFETY: plain calls; they read this thread's key state as of the
    // message just retrieved, which is what a shortcut must look at.
    let (ctrl, alt) = unsafe {
        (
            GetKeyState(i32::from(VK_CONTROL.0)) < 0,
            GetKeyState(i32::from(VK_MENU.0)) < 0,
        )
    };
    if !is_select_all(msg.message == WM_KEYDOWN, msg.wParam.0, ctrl, alt) {
        return false;
    }
    // SAFETY: the edit control belongs to this thread; (0, -1) selects all.
    unsafe {
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
    true
}

/// Cleans up after the message loop: destroys what is left and frees the font.
fn destroy_all() {
    let controls = CONTROLS
        .try_with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()))
        .ok()
        .flatten();
    if let Some(controls) = controls {
        // SAFETY: the window belongs to this thread; destroying an already
        // destroyed window just fails.
        let _ = unsafe { DestroyWindow(controls.main) };
        if let Some(font) = controls.font {
            // SAFETY: the font was created by this module and no window
            // uses it any more.
            let _ = unsafe { DeleteObject(font.into()) };
        }
    }
}

fn create() -> Result<Controls> {
    // SAFETY: IDC_ARROW is a system cursor; no instance is needed.
    let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|error| os::os_error("LoadCursorW", &error))?;
    // The system brush constant is the color index plus one.
    let background = HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void);
    os::register_class(CLASS, Some(procedure), arrow, background)?;
    let instance = os::module_instance()?;

    // Centered on the primary display's work area, sized for its DPI.
    let (area, dpi) = os::primary_work_area();
    let mut outer = RECT {
        left: 0,
        top: 0,
        right: scaled(CLIENT_WIDTH, dpi),
        bottom: scaled(CLIENT_HEIGHT, dpi),
    };
    // SAFETY: `outer` is a valid in/out RECT for the call.
    unsafe { AdjustWindowRectExForDpi(&mut outer, MAIN_STYLE, false, MAIN_EX_STYLE, dpi) }
        .map_err(|error| os::os_error("AdjustWindowRectExForDpi", &error))?;
    let width = outer.right - outer.left;
    let height = outer.bottom - outer.top;
    let x = area.left + (area.width() - width).max(0) / 2;
    let y = area.top + (area.height() - height).max(0) / 2;

    // SAFETY: the class was just registered with a 'static procedure; the
    // window belongs to this thread, which runs its message loop.
    let main = unsafe {
        CreateWindowExW(
            MAIN_EX_STYLE,
            CLASS,
            w!("Hop self-test"),
            MAIN_STYLE,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| os::os_error("CreateWindowExW (self-test window)", &error))?;

    let child = |ex_style: WINDOW_EX_STYLE, class: PCWSTR, text: PCWSTR, style: u32, id: usize| {
        // SAFETY: system control classes, with `main` (this thread's window)
        // as parent; the id travels in the menu parameter, as Win32 does
        // for child windows.
        unsafe {
            CreateWindowExW(
                ex_style,
                class,
                text,
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
                0,
                0,
                0,
                0,
                Some(main),
                Some(HMENU(id as *mut c_void)),
                Some(instance),
                None,
            )
        }
    };
    let children = (|| {
        // SS_LEFT (style 0): left-aligned text, wrapped at word breaks.
        let label = child(WINDOW_EX_STYLE(0), w!("STATIC"), w!(""), SS_NOPREFIX, 1)?;
        let edit_style =
            WS_TABSTOP.0 | WS_VSCROLL.0 | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32;
        let edit = child(WS_EX_CLIENTEDGE, w!("EDIT"), w!(""), edit_style, 2)?;
        let button_style = WS_TABSTOP.0 | BS_PUSHBUTTON as u32;
        let button = child(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Click me"),
            button_style,
            BUTTON_ID,
        )?;
        Ok::<_, windows::core::Error>((label, edit, button))
    })();
    let (label, edit, button) = match children {
        Ok(children) => children,
        Err(error) => {
            // SAFETY: `main` is this thread's window; this also destroys any
            // children created so far.
            let _ = unsafe { DestroyWindow(main) };
            return Err(os::os_error("CreateWindowExW (self-test controls)", &error));
        }
    };
    let mut controls = Controls {
        main,
        label,
        edit,
        button,
        font: None,
    };
    // SAFETY: `main` is this thread's window.
    let dpi = unsafe { GetDpiForWindow(main) };
    lay_out(&mut controls, if dpi == 0 { BASE_DPI } else { dpi });
    Ok(controls)
}

/// `value` 96-DPI pixels at `dpi`.
fn scaled(value: i32, dpi: u32) -> i32 {
    let dpi = i64::from(dpi);
    i32::try_from(i64::from(value) * dpi / i64::from(BASE_DPI)).unwrap_or(value)
}

/// Places the controls and gives them a font for `dpi`.
fn lay_out(controls: &mut Controls, dpi: u32) {
    let s = |value| scaled(value, dpi);
    let inner_width = CLIENT_WIDTH - 2 * MARGIN;
    let places = [
        (controls.label, LABEL_TOP, inner_width, LABEL_HEIGHT),
        (controls.edit, EDIT_TOP, inner_width, EDIT_HEIGHT),
        (controls.button, BUTTON_TOP, BUTTON_WIDTH, BUTTON_HEIGHT),
    ];
    for (hwnd, top, width, height) in places {
        // SAFETY: the controls belong to this thread.
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                s(MARGIN),
                s(top),
                s(width),
                s(height),
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }
    let Some(font) = message_font(dpi) else {
        return;
    };
    for hwnd in [controls.label, controls.edit, controls.button] {
        // SAFETY: WM_SETFONT takes the font handle in wParam and "redraw" in
        // lParam; the font outlives the controls (freed in destroy_all).
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
    }
    if let Some(old) = controls.font.replace(font) {
        // SAFETY: no control uses the old font any more.
        let _ = unsafe { DeleteObject(old.into()) };
    }
}

/// The system's message font at `dpi`, the font dialogs use.
fn message_font(dpi: u32) -> Option<HFONT> {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `metrics` is a NONCLIENTMETRICSW with cbSize set, and the
    // size passed is its size.
    unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast::<c_void>()),
            0,
            dpi,
        )
    }
    .ok()?;
    // SAFETY: the LOGFONTW was just filled in by Windows.
    let font = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
    (!font.is_invalid()).then_some(font)
}

fn set_text(hwnd: HWND, text: &HSTRING) -> Result<()> {
    // SAFETY: the control belongs to this thread; the string outlives the
    // call.
    unsafe { SetWindowTextW(hwnd, text) }.map_err(|error| os::os_error("SetWindowTextW", &error))
}

/// The text field's contents, with Windows line breaks as `\n`.
fn read_text(edit: HWND) -> String {
    // SAFETY: the control belongs to this thread.
    let len = usize::try_from(unsafe { GetWindowTextLengthW(edit) }).unwrap_or(0);
    let mut buffer = vec![0_u16; len + 1];
    // SAFETY: `buffer` holds len + 1 units, room for the text and its NUL;
    // GetWindowTextW writes no more than the slice's length.
    let copied = usize::try_from(unsafe { GetWindowTextW(edit, &mut buffer) }).unwrap_or(0);
    let text = buffer.get(..copied.min(len)).unwrap_or_default();
    String::from_utf16_lossy(text).replace("\r\n", "\n")
}

/// Brings the window to the front and gives the text field keyboard focus.
///
/// Windows lets a background process take the foreground only in some
/// cases. When `SetForegroundWindow` is refused, attaching this thread's
/// input to the foreground window's thread for a moment makes the two
/// share input state, and the request is then allowed.
fn focus_text_field(controls: &Controls) -> Result<()> {
    let main = controls.main;
    // SAFETY (all calls in this function): they act on this thread's own
    // windows, or attach to the thread that owns the foreground window,
    // which Windows validates; AttachThreadInput is undone before returning.
    unsafe {
        if GetForegroundWindow() != main && !SetForegroundWindow(main).as_bool() {
            let foreground = GetForegroundWindow();
            let theirs = GetWindowThreadProcessId(foreground, None);
            let ours = GetCurrentThreadId();
            let attached =
                theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
            let _ = BringWindowToTop(main);
            let _ = SetForegroundWindow(main);
            if attached {
                let _ = AttachThreadInput(ours, theirs, false);
            }
        }
        // SetFocus returns the previous focus, which may be none; success is
        // checked below instead.
        let _ = SetFocus(Some(controls.edit));
        if GetForegroundWindow() != main {
            return Err(PlatformError::Os(
                "Windows did not let the self-test window come to the front; click it, then retry"
                    .into(),
            ));
        }
        if GetFocus() != controls.edit {
            return Err(PlatformError::Os(
                "the self-test window's text field did not get keyboard focus".into(),
            ));
        }
    }
    Ok(())
}

/// The main window's procedure.
unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = os::guarded(
        None,
        || {},
        || match message {
            WM_APP_WAKE => {
                worker::run_pending_jobs(|| {});
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                // Low word: control id; high word: notification code.
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                if id == BUTTON_ID && code == BN_CLICKED as usize {
                    CLICKS.with(|clicks| {
                        if let Some(clicks) = clicks.borrow().as_ref() {
                            clicks.fetch_add(1, Ordering::AcqRel);
                        }
                    });
                }
                None
            }
            WM_DPICHANGED => {
                // Moved to a monitor with another scale: take Windows' suggested
                // size and position, then re-lay out for the new DPI (low word).
                if lparam.0 != 0 {
                    // SAFETY: for WM_DPICHANGED, lparam points to a RECT valid
                    // for the duration of the message; it is copied.
                    let suggested = unsafe { *(lparam.0 as *const RECT) };
                    // SAFETY: `hwnd` is this thread's window.
                    let _ = unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            suggested.left,
                            suggested.top,
                            suggested.right - suggested.left,
                            suggested.bottom - suggested.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        )
                    };
                }
                let dpi = u32::try_from(wparam.0 & 0xFFFF).unwrap_or(BASE_DPI);
                CONTROLS.with(|controls| {
                    if let Ok(mut controls) = controls.try_borrow_mut()
                        && let Some(controls) = controls.as_mut()
                    {
                        lay_out(controls, dpi);
                    }
                });
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                // SAFETY: plain call; ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ => None,
        },
    );
    match handled {
        Some(result) => result,
        // SAFETY: the arguments are passed on unchanged.
        None => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
