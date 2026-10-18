//! The Hop agent as a library, shared by the `hop` command and the Hop app.
//!
//! - [`cli`]: the `hop` command line. The `hop` binary only calls
//!   [`cli::main`].
//! - [`service`]: the agent (`hop run`) on a thread of its own, for an app:
//!   start it, stop it, and hear what it is doing.
//! - [`pairing`]: pairing the two computers once with a code, the flow
//!   behind `hop pair`, without a terminal.
//! - [`settings`]: reading and changing the config file (where the other
//!   computer sits, its address, forgetting it).
//! - [`system`]: small helpers: the Accessibility permission, opening
//!   System Settings and the log folder, and keeping macOS from napping Hop.
//! - [`logging`]: the log every part writes to, and its status lines.
//! - [`native`]: this OS's platform backend.
//!
//! The rules the agent keeps (docs/M2-DESIGN.md, "The agent") hold for an
//! app too: the core thread never waits for the app or for a disk, quitting
//! gives the keyboard and mouse back before the network says goodbye, any
//! panic ends a session failing open, and nothing logs keys, clipboard
//! text, pairing codes or secrets.
//!
//! This crate has no OS code of its own: it reaches the OS through
//! `hop-platform` and its backends, plus `open` and `explorer` to show a
//! folder or a System Settings pane.

#![forbid(unsafe_code)]

mod agent;
pub mod cli;
pub mod logging;
pub mod native;
pub mod pairing;
mod paths;
pub mod service;
pub mod settings;
mod simulate;
mod startup;
pub mod system;

/// Where the other computer sits: the side of this computer's screens the
/// cursor crosses to reach it.
pub use hop_config::Side;
/// The state of an OS permission, as [`system::accessibility`] reports it.
pub use hop_platform::PermissionState;
/// A computer's id and its operating system.
pub use hop_types::{DeviceId, Os};
/// Text that is wiped from memory when dropped: the pairing code.
pub use zeroize::Zeroizing;

/// Serializes the tests that replace the process-wide panic hook, so one
/// never puts back a hook another test installed.
#[cfg(test)]
pub(crate) static PANIC_HOOK_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
