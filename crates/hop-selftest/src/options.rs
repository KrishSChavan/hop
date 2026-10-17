//! Settings for a self-test run.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use hop_platform::SUPPRESS_LEASE;

/// The default hard limit on a whole self-test run. The steps themselves
/// take about a minute, countdowns included.
pub const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(120);

/// How long each part of the self-test takes.
///
/// [`Timing::default`] gives the durations the user sees. Tests use much
/// shorter ones so they finish in seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    /// The countdown shown before a step starts, so nothing moves by surprise.
    pub countdown: Duration,
    /// How long step 2 watches the mouse and keyboard.
    pub capture: Duration,
    /// The pause between injected key events.
    pub key_interval: Duration,
    /// How long to wait for typed text, a click or captured events to show up
    /// before deciding they never will.
    pub settle_timeout: Duration,
    /// The pause after moving the cursor before clicking, and how long an
    /// injected mouse button stays down.
    pub click_pause: Duration,
    /// How long to wait after bringing the test window to the front before
    /// typing or clicking into it: the OS switches windows asynchronously.
    pub focus_pause: Duration,
    /// How long step 5 suppresses the mouse.
    pub suppress: Duration,
    /// How often the suppression lease is renewed while Hop suppresses input.
    /// Never more than half of [`SUPPRESS_LEASE`], whatever is set here.
    pub renew_interval: Duration,
    /// How long after the last renewal step 5 checks that capture went back
    /// to passive by itself. Must be longer than the backend's lease.
    pub lapse_wait: Duration,
    /// How long step 6 keeps the cursor hidden.
    pub park: Duration,
    /// How long step 7 waits for a push against the right edge.
    pub edge_push: Duration,
    /// How often waits check for the emergency chord, the time limit and new
    /// events. At least 1 ms.
    pub poll_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            countdown: Duration::from_secs(3),
            capture: Duration::from_secs(8),
            key_interval: Duration::from_millis(15),
            settle_timeout: Duration::from_millis(1500),
            click_pause: Duration::from_millis(60),
            focus_pause: Duration::from_millis(250),
            suppress: Duration::from_secs(3),
            renew_interval: Duration::from_millis(200),
            lapse_wait: Duration::from_millis(1500),
            park: Duration::from_secs(2),
            edge_push: Duration::from_secs(10),
            poll_interval: Duration::from_millis(10),
        }
    }
}

impl Timing {
    /// These timings with values that would make the run unsafe or spin
    /// replaced: a renewal slower than half the lease could let suppression
    /// lapse in the middle of a step, and a zero poll interval would busy-wait.
    /// Every duration is capped at an hour, so adding one to a clock reading
    /// can never overflow.
    pub(crate) fn sanitized(mut self) -> Self {
        const LONGEST: Duration = Duration::from_secs(3600);
        for duration in [
            &mut self.countdown,
            &mut self.capture,
            &mut self.key_interval,
            &mut self.settle_timeout,
            &mut self.click_pause,
            &mut self.focus_pause,
            &mut self.suppress,
            &mut self.renew_interval,
            &mut self.lapse_wait,
            &mut self.park,
            &mut self.edge_push,
            &mut self.poll_interval,
        ] {
            *duration = (*duration).min(LONGEST);
        }
        self.renew_interval = self.renew_interval.min(SUPPRESS_LEASE / 2);
        self.poll_interval = self.poll_interval.max(Duration::from_millis(1));
        self
    }
}

/// Settings for [`run`](crate::run).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTestOptions {
    /// Where the text report is written. `None` skips writing it. The
    /// default is [`default_report_path`].
    pub report_path: Option<PathBuf>,
    /// The hard limit on the whole run. When it runs out, the step in
    /// progress stops, Hop gives back the mouse and keyboard, and the
    /// remaining steps are skipped.
    pub time_limit: Duration,
    /// How long each part takes.
    pub timing: Timing,
    /// Print each step's instructions and result to stdout as well as
    /// showing them in the test window.
    pub print_to_stdout: bool,
}

impl Default for SelfTestOptions {
    fn default() -> Self {
        Self {
            report_path: default_report_path(),
            time_limit: DEFAULT_TIME_LIMIT,
            timing: Timing::default(),
            print_to_stdout: true,
        }
    }
}

/// Where the report goes by default: `~/Library/Logs/Hop/selftest.txt` on
/// macOS, `%LOCALAPPDATA%\Hop\Logs\selftest.txt` on Windows, and
/// `$XDG_STATE_HOME/hop/selftest.txt` (or `~/.local/state/hop/selftest.txt`)
/// elsewhere. `None` when the environment variable it needs is not set.
pub fn default_report_path() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home().map(|home| {
            home.join("Library")
                .join("Logs")
                .join("Hop")
                .join("selftest.txt")
        })
    } else if cfg!(windows) {
        env_dir("LOCALAPPDATA").map(|dir| dir.join("Hop").join("Logs").join("selftest.txt"))
    } else {
        env_dir("XDG_STATE_HOME")
            .or_else(|| home().map(|home| home.join(".local").join("state")))
            .map(|dir| dir.join("hop").join("selftest.txt"))
    }
}

fn home() -> Option<PathBuf> {
    env_dir("HOME")
}

/// The directory an environment variable names, ignoring an empty value.
fn env_dir(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_keeps_the_lease_renewed_and_waits_from_spinning() {
        let timing = Timing {
            renew_interval: Duration::from_secs(5),
            poll_interval: Duration::ZERO,
            ..Timing::default()
        }
        .sanitized();
        assert_eq!(timing.renew_interval, SUPPRESS_LEASE / 2);
        assert_eq!(timing.poll_interval, Duration::from_millis(1));
        assert_eq!(Timing::default().sanitized(), Timing::default());
        let huge = Timing {
            capture: Duration::MAX,
            click_pause: Duration::MAX,
            ..Timing::default()
        }
        .sanitized();
        assert_eq!(huge.capture, Duration::from_secs(3600));
        assert_eq!(huge.click_pause, Duration::from_secs(3600));
    }

    #[test]
    fn defaults_match_the_spec() {
        let timing = Timing::default();
        assert_eq!(timing.capture, Duration::from_secs(8));
        assert_eq!(timing.suppress, Duration::from_secs(3));
        assert_eq!(timing.renew_interval, Duration::from_millis(200));
        assert_eq!(timing.lapse_wait, Duration::from_millis(1500));
        assert_eq!(timing.park, Duration::from_secs(2));
        assert_eq!(timing.edge_push, Duration::from_secs(10));
        // The lapse check must wait longer than the lease it checks.
        assert!(timing.lapse_wait > SUPPRESS_LEASE);
    }

    #[test]
    fn the_default_report_path_is_a_selftest_file() {
        // HOME (or LOCALAPPDATA) is set wherever these tests run.
        let path = default_report_path().unwrap();
        assert!(path.ends_with("selftest.txt"));
        if cfg!(target_os = "macos") {
            assert!(path.ends_with("Library/Logs/Hop/selftest.txt"));
        }
        if cfg!(windows) {
            assert!(path.ends_with("Hop\\Logs\\selftest.txt"));
        }
    }
}
