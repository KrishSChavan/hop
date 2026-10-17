//! Running code on a thread that owns windows.
//!
//! A Win32 window belongs to the thread that created it, and that thread
//! must keep pumping messages. Hop's input thread and the test window's
//! thread each run a message loop; other threads hand them work as boxed
//! jobs through a channel and post `WM_APP_WAKE` so the loop runs them. The
//! reply comes back on a channel of its own, with a timeout, so a caller can
//! never hang on a stuck thread. Nothing is shared by pointer across
//! threads, so a reply that comes too late is simply dropped.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use hop_platform::{PlatformError, Result};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostMessageW, TranslateMessage, WM_APP,
};

use crate::os::{self, guarded};

/// Work for a window-owning thread.
pub(crate) type Job = Box<dyn FnOnce() + Send>;

/// Posted to a worker's window when jobs are waiting.
pub(crate) const WM_APP_WAKE: u32 = WM_APP + 0x10;

thread_local! {
    /// The jobs waiting for this thread, if it is a worker.
    static INBOX: RefCell<Option<Receiver<Job>>> = const { RefCell::new(None) };
}

/// A handle for giving work to a window-owning thread.
#[derive(Clone, Debug)]
pub(crate) struct Worker {
    /// The thread's window, as a number (see [`os::hwnd_to_raw`]).
    hwnd: usize,
    thread_id: u32,
    jobs: Sender<Job>,
    /// For error messages: "Hop's input thread".
    name: &'static str,
}

impl Worker {
    /// Makes the calling thread a worker that receives jobs through `hwnd`,
    /// one of its windows, and returns the handle for other threads.
    pub(crate) fn install(hwnd: HWND, name: &'static str) -> Worker {
        let (jobs, inbox) = mpsc::channel();
        INBOX.with(|slot| *slot.borrow_mut() = Some(inbox));
        Worker {
            hwnd: os::hwnd_to_raw(hwnd),
            // SAFETY: plain call.
            thread_id: unsafe { GetCurrentThreadId() },
            jobs,
            name,
        }
    }

    /// Runs `job` on the worker thread and returns its result, waiting at
    /// most `timeout`. Called on the worker thread itself, it runs `job`
    /// right away (waiting for itself would deadlock).
    pub(crate) fn run<R: Send + 'static>(
        &self,
        timeout: Duration,
        job: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R> {
        // SAFETY: plain call.
        if unsafe { GetCurrentThreadId() } == self.thread_id {
            return Ok(job());
        }
        let (reply, answer) = mpsc::sync_channel(1);
        self.jobs
            .send(Box::new(move || {
                // The caller may have given up waiting; then nobody listens.
                let _ = reply.send(job());
            }))
            .map_err(|_| self.gone())?;
        // SAFETY: posting is allowed from any thread, and a stale handle
        // only makes the call fail.
        unsafe {
            PostMessageW(
                Some(os::raw_to_hwnd(self.hwnd)),
                WM_APP_WAKE,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .map_err(|error| os::os_error("PostMessageW", &error))?;
        answer.recv_timeout(timeout).map_err(|error| match error {
            RecvTimeoutError::Timeout => PlatformError::Os(format!(
                "{} did not answer within {} ms",
                self.name,
                timeout.as_millis()
            )),
            // The job was dropped without running, or panicked.
            RecvTimeoutError::Disconnected => self.gone(),
        })
    }

    fn gone(&self) -> PlatformError {
        PlatformError::Os(format!("{} has stopped", self.name))
    }
}

/// Runs every job waiting for this thread. Window procedures call this on
/// `WM_APP_WAKE`. No borrow is held while a job runs, so a job may do
/// anything, including things that send messages back to this thread. If a
/// job panics, `on_panic` runs (the input thread passes its fail-open).
pub(crate) fn run_pending_jobs(on_panic: fn()) {
    loop {
        let job = INBOX
            .try_with(|slot| {
                let slot = slot.try_borrow().ok()?;
                slot.as_ref()?.try_recv().ok()
            })
            .ok()
            .flatten();
        let Some(job) = job else {
            break;
        };
        // A job that panics drops its reply channel, so its caller gets an
        // error instead of waiting forever.
        guarded((), on_panic, job);
    }
}

/// Pumps this thread's messages until `WM_QUIT` (or an error).
pub(crate) fn message_loop() {
    message_loop_with(|_| false);
}

/// Like [`message_loop`], but `handled` sees each message first; when it
/// returns `true`, the message is neither translated nor dispatched (the
/// way a keyboard accelerator works).
pub(crate) fn message_loop_with(mut handled: impl FnMut(&MSG) -> bool) {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid out-pointer; `None` asks for messages of
        // every window of this thread and for thread messages.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // 0 is WM_QUIT, -1 an error.
        if result.0 <= 0 {
            break;
        }
        // Runs outside any window procedure, so a panic here would not
        // cross into Windows; it is still caught, so a bad filter can only
        // lose its own message.
        if guarded(false, || {}, || handled(&msg)) {
            continue;
        }
        // SAFETY: `msg` was just filled in by GetMessageW. TranslateMessage
        // turns key messages into WM_CHAR, which the test window's text
        // field needs.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
