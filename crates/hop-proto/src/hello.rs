//! The `Hello` handshake message and version and capability negotiation.

use std::collections::BTreeMap;

use hop_types::{DeviceId, DisplayInfo, Os};
use serde::{Deserialize, Serialize};

use crate::ProtoError;
use crate::message::{KIND_HELLO, decode_body};

/// This build's major protocol version. Peers with different majors refuse
/// to talk.
pub const PROTO_MAJOR: u16 = 1;

/// This build's minor protocol version. Peers settle on the lower of their
/// two minors.
pub const PROTO_MINOR: u16 = 0;

/// Names of the capabilities this build knows about.
pub mod caps {
    /// Keyboard and mouse control across devices.
    pub const INPUT: &str = "input";
    /// Swapping shortcut modifiers (Cmd and Ctrl) between macOS and Windows.
    pub const KEYMAP: &str = "keymap";
    /// Syncing plain-text clipboard contents.
    pub const CLIPBOARD_TEXT: &str = "clipboard.text";
    /// Sending files to the other computer's Received folder.
    pub const FILES: &str = "files";
}

/// A named feature and the highest version of it a device supports.
///
/// Supporting version N means supporting every version below it as well,
/// because [`negotiate`] settles on the lower of the two sides' versions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Capability {
    /// The capability's name, such as [`caps::INPUT`].
    pub name: String,
    /// The highest version supported.
    pub version: u16,
}

impl Capability {
    /// Creates a capability.
    pub fn new(name: impl Into<String>, version: u16) -> Self {
        Self {
            name: name.into(),
            version,
        }
    }
}

/// The capabilities this build supports: `input`, `keymap` and
/// `clipboard.text`, each at version 1.
pub fn default_capabilities() -> Vec<Capability> {
    vec![
        Capability::new(caps::INPUT, 1),
        Capability::new(caps::KEYMAP, 1),
        Capability::new(caps::CLIPBOARD_TEXT, 1),
    ]
}

/// The first message on every connection, sent by both sides: who the device
/// is, what its screens look like and what it can do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// The sender's major protocol version.
    pub proto_major: u16,
    /// The sender's minor protocol version.
    pub proto_minor: u16,
    /// The sender's Hop version, for logs and error messages.
    pub app_version: String,
    /// The sender's stable identifier.
    pub device_id: DeviceId,
    /// A human-readable name for the sender, such as "Krish's MacBook".
    pub device_name: String,
    /// The sender's operating system.
    pub os: Os,
    /// The sender's displays, in its own logical coordinates.
    pub displays: Vec<DisplayInfo>,
    /// What the sender supports.
    pub capabilities: Vec<Capability>,
}

impl Hello {
    /// A `Hello` for this build: the current protocol version, the OS this
    /// binary was compiled for and [`default_capabilities`].
    pub fn new(
        app_version: impl Into<String>,
        device_id: DeviceId,
        device_name: impl Into<String>,
        displays: Vec<DisplayInfo>,
    ) -> Self {
        Self {
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
            app_version: app_version.into(),
            device_id,
            device_name: device_name.into(),
            os: Os::current(),
            displays,
            capabilities: default_capabilities(),
        }
    }
}

/// What two devices agreed on: the protocol version and capabilities both
/// support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Negotiated {
    /// The shared major version.
    pub proto_major: u16,
    /// The lower of the two minor versions.
    pub proto_minor: u16,
    /// The capabilities both sides listed, each at the lower of the two
    /// versions, sorted by name.
    pub capabilities: Vec<Capability>,
}

impl Negotiated {
    /// The agreed version of the capability `name`, or `None` if the two
    /// devices do not share it.
    pub fn capability_version(&self, name: &str) -> Option<u16> {
        self.capabilities
            .iter()
            .find(|capability| capability.name == name)
            .map(|capability| capability.version)
    }
}

/// Works out what this device (`local`) and its peer (`remote`) can use
/// together.
///
/// The major versions must match, otherwise the result is
/// [`ProtoError::Incompatible`] naming both versions. The minor version is
/// the lower of the two. A capability is kept only if both sides list it, at
/// the lower of the two versions. If one side lists the same name more than
/// once, its highest version counts. The result is sorted by name and is the
/// same whichever side runs it.
pub fn negotiate(local: &Hello, remote: &Hello) -> Result<Negotiated, ProtoError> {
    if local.proto_major != remote.proto_major {
        return Err(ProtoError::Incompatible {
            local_major: local.proto_major,
            local_minor: local.proto_minor,
            remote_major: remote.proto_major,
            remote_minor: remote.proto_minor,
        });
    }
    let local_caps = highest_versions(&local.capabilities);
    let remote_caps = highest_versions(&remote.capabilities);
    let capabilities = local_caps
        .into_iter()
        .filter_map(|(name, local_version)| {
            let remote_version = remote_caps.get(name)?;
            Some(Capability::new(name, local_version.min(*remote_version)))
        })
        .collect();
    Ok(Negotiated {
        proto_major: local.proto_major,
        proto_minor: local.proto_minor.min(remote.proto_minor),
        capabilities,
    })
}

/// One entry per capability name with its highest listed version. A
/// `BTreeMap` keeps the negotiated list in a stable order.
fn highest_versions(capabilities: &[Capability]) -> BTreeMap<&str, u16> {
    let mut highest = BTreeMap::new();
    for capability in capabilities {
        highest
            .entry(capability.name.as_str())
            .and_modify(|version: &mut u16| *version = (*version).max(capability.version))
            .or_insert(capability.version);
    }
    highest
}

/// Only the version fields of a `Hello`, which keep their names and types
/// in every protocol version.
#[derive(Deserialize)]
struct HelloVersion {
    proto_major: u16,
    #[serde(default)]
    proto_minor: u16,
}

/// Decodes a `Hello` body.
///
/// A peer with a different major version may have changed `Hello` itself, so
/// its body may not decode. In that case the version fields alone are read,
/// so the caller learns the real problem ([`ProtoError::Incompatible`])
/// rather than a confusing decode error.
pub(crate) fn decode_hello(body: &[u8]) -> Result<Hello, ProtoError> {
    decode_body::<Hello>(KIND_HELLO, body).map_err(|err| {
        match decode_body::<HelloVersion>(KIND_HELLO, body) {
            Ok(version) if version.proto_major != PROTO_MAJOR => ProtoError::Incompatible {
                local_major: PROTO_MAJOR,
                local_minor: PROTO_MINOR,
                remote_major: version.proto_major,
                remote_minor: version.proto_minor,
            },
            _ => err,
        }
    })
}
