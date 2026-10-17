//! Hop's guided self-test of the platform layer (`hop selftest`) and a
//! network latency probe (`hop latency`).
//!
//! # The self-test
//!
//! [`run`] checks a backend of `hop-platform` on the real machine, with the
//! user watching: it captures their input, types and clicks into a small
//! test window, suppresses the mouse, hides the cursor, and feeds an edge
//! push to Hop's real core. It programs only against the `hop-platform`
//! traits, so the same code checks the macOS and the Windows backend. Each
//! step reports [`Outcome::Pass`], [`Outcome::Fail`] or
//! [`Outcome::Skipped`] with details, and the [`Report`] is written to a log
//! file and returned.
//!
//! ## Safety
//!
//! - Nothing moves without warning: every step shows its instructions in the
//!   test window and on stdout, then counts down.
//! - Fail open: however a run ends (normally, by an error, the emergency
//!   chord, the time limit, or a panic in a backend), capture goes back to
//!   passive, every injected key and button is released, and a hidden
//!   cursor is shown again where it was. Suppression is a lease the
//!   self-test keeps renewing, so even a hang gives input back.
//! - The emergency chord (Ctrl+Option+Shift+Esc on a Mac, Ctrl+Alt+Shift+Esc
//!   on a PC) stops a run at once. The capture sink watches for it, and
//!   every wait checks for it.
//! - Nothing here asks for a permission. Steps that need Accessibility are
//!   skipped, with instructions, when it is not granted.
//!
//! ## Privacy
//!
//! The capture sink forgets which key each event was before handing it on;
//! only the two keys of the copy shortcut that step 2 asks for are
//! recognized. Reports hold counts, positions and states. They never quote
//! the test window's text box (the user may have typed there), and the
//! self-test never touches the clipboard.
//!
//! # The latency probe
//!
//! [`latency_serve`] on one computer and [`latency_probe`] on the other
//! measure Wi-Fi round trips with small numbered UDP packets on
//! [`DEFAULT_LATENCY_PORT`], and [`LatencyStats`] says how cursor movement
//! would feel.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod keys;
mod latency;
mod observe;
mod options;
mod report;
mod runner;
mod session;
mod steps;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

pub use keys::{command_key, key_for_char};
pub use latency::{
    DEFAULT_LATENCY_PORT, LatencyError, LatencyStats, REPLY_TIMEOUT, latency_probe, latency_serve,
};
pub use options::{DEFAULT_TIME_LIMIT, SelfTestOptions, Timing, default_report_path};
pub use report::{Outcome, Report, StepId, StepReport, Stop, Verdict};
pub use runner::{run, run_selected};
