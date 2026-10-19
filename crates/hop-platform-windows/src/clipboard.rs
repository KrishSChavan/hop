//! The contract's text clipboard (milestone M4), on a thread of its own
//! (docs/M4-M5-DESIGN.md, section 4).
//!
//! Reading the clipboard can wait on the app that copied: an app that hands
//! over its data only when asked (delayed rendering) gets up to 30 seconds
//! to answer. So reads and writes run on the "hop-clipboard" thread, which
//! owns a message-only window (the clipboard's owner while Hop's text is on
//! it) and pumps its messages, and the caller waits at most
//! `CLIPBOARD_TIMEOUT` for the answer. A job whose caller gave up before it
//! started is skipped; one already running finishes, and its answer is
//! dropped. A late write therefore still puts the text on the clipboard,
//! which is what the caller asked for; the caller only misses the change
//! count, so at worst the text is later sent back once.
//!
//! The change count is `GetClipboardSequenceNumber`, which any thread may
//! call at any time. The rules (which items are never read, UTF-16 and line
//! endings, the size limit) are in `logic::clipboard`. Text is never logged,
//! and errors never contain it.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hop_platform::{CLIPBOARD_TIMEOUT, PlatformError, Result, TextClipboard};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, HCURSOR, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::logic::clipboard::{
    CAN_INCLUDE_IN_HISTORY, CAN_UPLOAD_TO_CLOUD, EXCLUDE_FROM_MONITORING, Flag, Marks,
    OPEN_RETRY_MS, OPEN_TRIES, VIEWER_IGNORE, decode_text, dword, encode_text, text_units,
};
use crate::os;
use crate::worker::{self, WM_APP_WAKE, Worker};

const CLASS: PCWSTR = w!("HopClipboard");

/// `CF_UNICODETEXT`, as the clipboard functions take it.
const UNICODE_TEXT: u32 = CF_UNICODETEXT.0 as u32;

/// The clipboard thread, once started.
static CLIPBOARD: Mutex<Option<Worker>> = Mutex::new(None);

thread_local! {
    /// On the clipboard thread: its message-only window. Null elsewhere.
    static OWNER: Cell<usize> = const { Cell::new(0) };
}

/// The contract's text clipboard. Every handle shares the one clipboard
/// thread, which starts on the first read or write.
#[derive(Debug, Default)]
pub(crate) struct WinClipboard;

impl TextClipboard for WinClipboard {
    fn change_count(&self) -> Result<u64> {
        // SAFETY: plain call; it only reads the window station's counter.
        Ok(u64::from(unsafe { GetClipboardSequenceNumber() }))
    }

    fn read_text(&self, max_bytes: usize) -> Result<Option<String>> {
        on_clipboard_thread(move |owner| read(owner, max_bytes))
    }

    fn write_text(&mut self, text: &str) -> Result<u64> {
        let units = encode_text(text);
        on_clipboard_thread(move |owner| write(owner, &units))
    }
}

/// Runs `job` on the clipboard thread with its window, starting the thread
/// if needed, and waits for it at most [`CLIPBOARD_TIMEOUT`] in all.
fn on_clipboard_thread<R: Send + 'static>(
    job: impl FnOnce(HWND) -> Result<R> + Send + 'static,
) -> Result<R> {
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let worker = worker(deadline)?;
    let wait = deadline.saturating_duration_since(Instant::now());
    worker.run(wait, move || {
        // The caller has given up: a read would make an app render its
        // data for nobody, and a write would come as a surprise.
        if Instant::now() >= deadline {
            return Err(PlatformError::Os(
                "the clipboard thread was busy, so a request that came too late was skipped".into(),
            ));
        }
        job(os::raw_to_hwnd(OWNER.with(Cell::get)))
    })?
}

fn worker(deadline: Instant) -> Result<Worker> {
    let mut slot = CLIPBOARD.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(worker) = slot.as_ref() {
        return Ok(worker.clone());
    }
    let worker = start(deadline)?;
    *slot = Some(worker.clone());
    Ok(worker)
}

/// Starts the clipboard thread, waiting for it until `deadline`. A thread
/// that starts later finds nobody waiting and ends; the next call starts
/// another.
fn start(deadline: Instant) -> Result<Worker> {
    let (ready, started) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("hop-clipboard".into())
        .spawn(move || main(&ready))
        .map_err(|error| {
            PlatformError::Os(format!("could not start Hop's clipboard thread: {error}"))
        })?;
    started
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| PlatformError::Os("Hop's clipboard thread did not start in time".into()))?
        .map_err(PlatformError::Os)
}

/// The clipboard thread.
fn main(ready: &SyncSender<std::result::Result<Worker, String>>) {
    let hwnd = match create_window() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    OWNER.with(|owner| owner.set(os::hwnd_to_raw(hwnd)));
    let worker = Worker::install(hwnd, "Hop's clipboard thread");
    // Nobody waiting any more: end, and the window goes with the thread.
    if ready.send(Ok(worker)).is_err() {
        return;
    }
    worker::message_loop();
}

fn create_window() -> Result<HWND> {
    os::register_class(
        CLASS,
        Some(procedure),
        HCURSOR::default(),
        HBRUSH::default(),
    )?;
    let instance = os::module_instance()?;
    // SAFETY: the class was just registered with a 'static procedure.
    // HWND_MESSAGE makes this a message-only window: never shown, never
    // enumerated, only there to own the clipboard and receive messages.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Hop clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| os::os_error("CreateWindowExW (clipboard thread)", &error))
}

/// The message-only window's procedure. Hop never promises data it has
/// not written yet (no delayed rendering), so there is nothing to render
/// when asked: everything but the jobs goes to the default procedure.
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
                // A job that panics closes the clipboard on the way out (see
                // `Opened`), and its caller gets an error.
                worker::run_pending_jobs(|| {});
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

/// The clipboard's text, read on the clipboard thread (see
/// `TextClipboard::read_text` and `logic::clipboard`).
fn read(owner: HWND, max_bytes: usize) -> Result<Option<String>> {
    let formats = private_formats()?;
    let units = {
        let _open = Opened::open(owner)?;
        if !available(UNICODE_TEXT) {
            return Ok(None);
        }
        let marks = Marks {
            exclude_from_monitoring: available(formats.exclude_from_monitoring),
            viewer_ignore: available(formats.viewer_ignore),
            history: flag(formats.history),
            cloud: flag(formats.cloud),
        };
        if marks.private() {
            return Ok(None);
        }
        // SAFETY: the clipboard is open; the handle stays valid until it
        // is closed, and Windows owns it.
        let text = unsafe { GetClipboardData(UNICODE_TEXT) }
            .map_err(|error| os::os_error("GetClipboardData (text)", &error))?;
        copy_text(HGLOBAL(text.0), max_bytes)?
        // The clipboard closes here, right after copying.
    };
    Ok(units.and_then(|units| decode_text(&units, max_bytes)))
}

/// Puts `units` (NUL-terminated UTF-16 with CRLF line endings, see
/// `logic::clipboard::encode_text`) on the clipboard as `CF_UNICODETEXT`,
/// owned by `owner`, and returns the change count right after. On the
/// clipboard thread.
fn write(owner: HWND, units: &[u16]) -> Result<u64> {
    // Filled in before the clipboard is opened, so it is open only for a
    // moment.
    // SAFETY: plain call.
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, size_of_val(units)) }
        .map_err(|error| os::os_error("GlobalAlloc", &error))?;
    let result = (|| {
        {
            let locked = Locked::new(memory).ok_or_else(|| os::last_error("GlobalLock"))?;
            // SAFETY: the block holds size_of_val(units) bytes, room for
            // every unit, and global memory is aligned for any type; the
            // two do not overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    units.as_ptr(),
                    locked.ptr.cast::<u16>(),
                    units.len(),
                );
            }
        }
        let _open = Opened::open(owner)?;
        // SAFETY: this thread has the clipboard open; emptying it makes
        // `owner` its owner.
        unsafe { EmptyClipboard() }.map_err(|error| os::os_error("EmptyClipboard", &error))?;
        // SAFETY: a moveable, unlocked block holding NUL-terminated UTF-16,
        // which is what CF_UNICODETEXT takes. On success it belongs to the
        // system.
        unsafe { SetClipboardData(UNICODE_TEXT, Some(HANDLE(memory.0))) }
            .map_err(|error| os::os_error("SetClipboardData", &error))?;
        Ok(())
        // The clipboard closes here.
    })();
    match result {
        // Read after closing, so it counts every change the write made.
        // SAFETY: plain call.
        Ok(()) => Ok(u64::from(unsafe { GetClipboardSequenceNumber() })),
        Err(error) => {
            // SAFETY: the clipboard did not take the block, so it is still
            // Hop's, and nothing else refers to it.
            let _ = unsafe { GlobalFree(Some(memory)) };
            Err(error)
        }
    }
}

/// The clipboard, opened by Hop's window. Closed when dropped, so an early
/// return or a panic never leaves it open, which would keep every other app
/// from using it.
struct Opened;

impl Opened {
    /// Opens the clipboard for `owner`, trying [`OPEN_TRIES`] times
    /// [`OPEN_RETRY_MS`] apart: another app may hold it for a moment.
    fn open(owner: HWND) -> Result<Opened> {
        let mut tries = 1;
        loop {
            // SAFETY: `owner` is this thread's window.
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => return Ok(Opened),
                Err(error) if tries >= OPEN_TRIES => {
                    return Err(PlatformError::Os(format!(
                        "another app is holding the clipboard open ({error})"
                    )));
                }
                Err(_) => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(OPEN_RETRY_MS));
                }
            }
        }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard and has not closed it.
        let _ = unsafe { CloseClipboard() };
    }
}

/// A locked global memory block, unlocked when dropped.
struct Locked {
    memory: HGLOBAL,
    /// The block's first byte.
    ptr: *mut c_void,
}

impl Locked {
    fn new(memory: HGLOBAL) -> Option<Locked> {
        // SAFETY: a global memory handle from GlobalAlloc or the clipboard;
        // an invalid one makes the call return null.
        let ptr = unsafe { GlobalLock(memory) };
        (!ptr.is_null()).then_some(Locked { memory, ptr })
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: locked once by `new`. GlobalUnlock reports "failure" (with
        // NO_ERROR) when the lock count reaches zero, which is fine.
        let _ = unsafe { GlobalUnlock(self.memory) };
    }
}

/// The text units of a `CF_UNICODETEXT` block on the open clipboard, copied
/// out: up to the first NUL and never past the block's end, or `None` when
/// the text is certainly longer than `max_bytes` (see
/// `logic::clipboard::text_units`; a huge block is never read through).
fn copy_text(memory: HGLOBAL, max_bytes: usize) -> Result<Option<Vec<u16>>> {
    // SAFETY: a valid handle from GetClipboardData; 0 for a bad one.
    let size = unsafe { GlobalSize(memory) };
    let locked = Locked::new(memory).ok_or_else(|| os::last_error("GlobalLock (text)"))?;
    // SAFETY: the locked block's `size` bytes stay valid and unchanged
    // while it is locked and the clipboard is open, which outlasts `block`;
    // global memory is aligned for any type, u16 included.
    let block = unsafe { std::slice::from_raw_parts(locked.ptr.cast::<u16>(), size / 2) };
    Ok(text_units(block, max_bytes).and_then(|len| block.get(..len).map(<[u16]>::to_vec)))
}

/// What a DWORD mark on the open clipboard holds (see
/// `logic::clipboard::Flag`).
fn flag(format: u32) -> Flag {
    if !available(format) {
        return Flag::Absent;
    }
    // SAFETY: the clipboard is open; Windows owns the handle.
    let Ok(data) = (unsafe { GetClipboardData(format) }) else {
        return Flag::Unreadable;
    };
    let memory = HGLOBAL(data.0);
    // SAFETY: a valid handle from GetClipboardData; 0 for a bad one.
    let size = unsafe { GlobalSize(memory) };
    let Some(locked) = Locked::new(memory) else {
        return Flag::Unreadable;
    };
    // SAFETY: at most the locked block's `size` bytes, valid while it is
    // locked and the clipboard is open.
    let bytes = unsafe { std::slice::from_raw_parts(locked.ptr.cast::<u8>(), size.min(4)) };
    dword(bytes)
}

/// Whether the open clipboard holds `format`.
fn available(format: u32) -> bool {
    // SAFETY: plain call.
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// The registered formats that can mark an item as private.
#[derive(Clone, Copy, Debug)]
struct PrivateFormats {
    exclude_from_monitoring: u32,
    viewer_ignore: u32,
    history: u32,
    cloud: u32,
}

/// The private marks' format numbers, registered once per process. Without
/// them nothing is read: an item could not be checked.
fn private_formats() -> Result<PrivateFormats> {
    static FORMATS: OnceLock<PrivateFormats> = OnceLock::new();
    if let Some(formats) = FORMATS.get() {
        return Ok(*formats);
    }
    let register = |name: &str| {
        let name = HSTRING::from(name);
        // SAFETY: a NUL-terminated name that outlives the call.
        match unsafe { RegisterClipboardFormatW(&name) } {
            0 => Err(os::last_error("RegisterClipboardFormatW")),
            format => Ok(format),
        }
    };
    let formats = PrivateFormats {
        exclude_from_monitoring: register(EXCLUDE_FROM_MONITORING)?,
        viewer_ignore: register(VIEWER_IGNORE)?,
        history: register(CAN_INCLUDE_IN_HISTORY)?,
        cloud: register(CAN_UPLOAD_TO_CLOUD)?,
    };
    Ok(*FORMATS.get_or_init(|| formats))
}
