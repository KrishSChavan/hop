//! The contract's input capture: a handle for the hooks on Hop's input
//! thread (see `hooks`).

use std::time::Instant;

use hop_platform::{CaptureMode, CapturedEvent, InputCapture, PlatformError, Result};

use crate::{cursor, engine, hooks, shared};

/// Starts and stops the hooks, and sets the suppression lease they obey.
#[derive(Debug, Default)]
pub(crate) struct WinCapture {
    /// This handle owns the running hooks.
    running: bool,
}

impl InputCapture for WinCapture {
    fn start(&mut self, sink: Box<dyn FnMut(CapturedEvent) + Send>) -> Result<()> {
        if self.running {
            return Err(PlatformError::Os("input capture is already running".into()));
        }
        if !shared::claim_capture() {
            return Err(PlatformError::Os(
                "input capture is already running elsewhere in this process".into(),
            ));
        }
        match engine::run(move || hooks::install(sink)) {
            Ok(Ok(())) => {
                self.running = true;
                Ok(())
            }
            Ok(Err(error)) | Err(error) => {
                // If the input thread was only slow, it may still install the
                // hooks; take them out again so nothing runs unclaimed.
                let _ = engine::run(hooks::uninstall);
                shared::release_capture();
                Err(error)
            }
        }
    }

    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        // The hooks read the lease on every event, so this takes effect for
        // the very next one. The watchdog ends a lease that is not renewed.
        shared::set_mode(mode, Instant::now());
        Ok(())
    }

    fn mode(&self) -> CaptureMode {
        shared::mode(Instant::now())
    }

    fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        // Without hooks nothing is suppressed, and a parked cursor would no
        // longer be pinned or report motion: end the lease now and free the
        // cursor (if it is parked) instead of waiting for the lease to run
        // out, whether or not a lease is still in force.
        shared::end_suppression();
        let _ = engine::run(|| {
            hooks::uninstall();
            cursor::unpark_after_lapse();
        });
        shared::release_capture();
    }
}

impl Drop for WinCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
