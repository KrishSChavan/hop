//! Shared vocabulary for Hop: device identity, operating systems, geometry,
//! input events and the session messages exchanged between devices.
//!
//! Every other Hop crate depends on this one, so it stays small, stable and
//! free of platform code.
//!
//! Coordinates are logical units (macOS points, Windows device-independent
//! pixels) with the origin at the top-left of the device's primary display and
//! y growing downward.

#![forbid(unsafe_code)]

mod geometry;
mod input;
mod msg;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use geometry::{DisplayInfo, Edge, Point, Rect, Size};
pub use input::{InputEvent, KeyCode, MouseButton, ScrollUnit};
pub use msg::{CoreMessage, EntryPoint, MotionMark, RejectReason};

/// Stable identifier for a device.
///
/// From M2 on this is the fingerprint of the device's long-term public key;
/// until then it comes from the config file. Ordering is used to break ties
/// when two devices try to take control of each other at the same moment.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Operating system family of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    MacOs,
    Windows,
    Linux,
}

impl Os {
    /// The OS this binary was compiled for.
    pub const fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Os::MacOs => "macOS",
            Os::Windows => "Windows",
            Os::Linux => "Linux",
        })
    }
}
