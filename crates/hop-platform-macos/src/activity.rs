//! Keeps macOS from napping Hop while it runs.
//!
//! Hop.app has no window while the agent runs, so macOS treats it as a
//! background app and may put it in App Nap, which throttles its timers.
//! Hop's 10 ms tick renews the suppression lease and sends heartbeats, so a
//! napping Hop would end sessions by itself.

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, NSString};

/// An `NSProcessInfo` activity that exempts Hop from App Nap for as long as
/// it is held. The Mac may still sleep when the user leaves it idle.
///
/// Hold it on the thread that created it (the main thread in `hop run`).
pub struct AppNapExemption {
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl AppNapExemption {
    /// Starts the exemption. `reason` shows in macOS's diagnostics.
    pub fn begin(reason: &str) -> Self {
        let activity = autoreleasepool(|_| {
            NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
                &NSString::from_str(reason),
            )
        });
        Self { activity }
    }
}

impl Drop for AppNapExemption {
    fn drop(&mut self) {
        autoreleasepool(|_| {
            // SAFETY: `activity` is the token beginActivityWithOptions:reason:
            // returned, which is exactly what endActivity: takes, and it is
            // ended only once, here.
            unsafe { NSProcessInfo::processInfo().endActivity(&self.activity) };
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exemption_can_begin_and_end() {
        // Starting and ending an activity touches nothing on screen and asks
        // for no permission.
        let exemption = AppNapExemption::begin("Hop test");
        drop(exemption);
    }
}
