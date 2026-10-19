//! Hop's OS-independent core: the control hand-off state machine, screen
//! layout and edge mapping. Sans-IO: events in, actions out, time passed in
//! explicitly. See docs/ARCHITECTURE.md ("Coordinates and layout" and
//! "Control hand-off").
//!
//! The runtime feeds every [`Event`] to [`Core::handle`] together with the
//! current time and carries out the [`Action`]s it returns, in order. The
//! core never reads a clock, touches the OS or the network, or uses
//! randomness, so the same inputs always give the same outputs.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod chord;
mod config;
mod layout;
mod machine;
mod motion;

pub use chord::Chord;
pub use config::CoreConfig;
pub use layout::{Crossing, DeviceLayout, Layout, LayoutError, SharedEdge};
pub use machine::{Action, CaptureMode, Core, EndReason, Event, Notice, State};

/// Milliseconds on a monotonic clock chosen by the runtime. Only differences
/// matter.
pub type Millis = u64;
