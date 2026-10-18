//! The settings, their defaults and first-run setup.

use std::fmt;
use std::path::PathBuf;

use hop_types::{DeviceId, Edge, Os};
use serde::{Deserialize, Serialize};

use crate::{ConfigError, ValidationError, validate};

/// Everything in Hop's config file.
///
/// Every setting has a default, so a missing file, section or key falls back
/// to the value documented on its field. Peers are the exception: each
/// `[[peers]]` entry must give its `name`, `id`, `os` and `side`.
///
/// Read the file with [`Config::load`] and write it with [`Config::save`];
/// [`Config::default_path`] says where it lives.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// This computer.
    pub device: DeviceConfig,
    /// The computers this one shares its keyboard and mouse with. Empty by
    /// default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerConfig>,
    /// Moving the cursor and keyboard between computers.
    pub input: InputConfig,
    /// Shortcut translation.
    pub keymap: KeymapConfig,
    /// Text clipboard sync.
    pub clipboard: ClipboardConfig,
    /// Files sent from other computers.
    pub transfer: TransferConfig,
    /// Networking.
    pub network: NetworkConfig,
}

impl Config {
    /// The config for a computer running Hop for the first time: every
    /// setting at its default, `device.name` set to `device_name`, and a new
    /// `device.id` made of 16 random bytes written as 32 lowercase hex
    /// characters.
    ///
    /// The random id is only a placeholder. From M2 on, a device's id comes
    /// from its long-term key, and the agent puts that id in place with
    /// [`Config::set_device_id`] at every start, before anything else uses
    /// it. That is also how the first M2 run replaces the random id an M0
    /// build saved.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] if `device_name` is empty or only spaces, and
    /// [`ConfigError::Random`] if the system's random number source fails.
    pub fn first_run(device_name: impl Into<String>) -> Result<Config, ConfigError> {
        let name = device_name.into();
        if is_blank(&name) {
            return Err(ConfigError::Invalid {
                path: None,
                errors: vec![ValidationError::Empty {
                    key: "device.name".to_owned(),
                }],
            });
        }
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(ConfigError::Random)?;
        Ok(Config {
            device: DeviceConfig {
                name,
                id: DeviceId::new(lowercase_hex(&bytes)),
            },
            ..Config::default()
        })
    }

    /// Sets this device's id, `device.id`.
    ///
    /// The agent calls this at every start with the id derived from the
    /// device's key: [`device_id_from_fingerprint`](crate::device_id_from_fingerprint)
    /// of the key's fingerprint. The first M2 run replaces M0's random id
    /// this way, and a key replaced on purpose gets its new id.
    ///
    /// Returns whether the id changed, which is when the file needs saving.
    /// The new id is checked like every other setting when the config is
    /// saved: it must not be a peer's id, and the device needs a name (a
    /// config that has none yet can take its `device` from
    /// [`Config::first_run`] first).
    pub fn set_device_id(&mut self, id: DeviceId) -> bool {
        if self.device.id == id {
            return false;
        }
        self.device.id = id;
        true
    }
}

/// This computer's identity.
///
/// Both fields are empty until first-run setup ([`Config::first_run`]) fills
/// them in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    /// The name other computers show for this one, such as
    /// "Krish's MacBook". Required once the device has an id.
    pub name: String,
    /// This computer's stable id. Other computers list it as the `id` of
    /// their peer entry for this one. Empty until first-run setup.
    pub id: DeviceId,
}

impl DeviceConfig {
    /// Whether this computer has an id yet. A loaded config without one (no
    /// file yet, or a file without `device.id`) needs first-run setup: take
    /// the `device` section from [`Config::first_run`] and save.
    pub fn has_id(&self) -> bool {
        !is_blank(self.id.as_str())
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            id: DeviceId::new(String::new()),
        }
    }
}

/// Another computer, and where its screens sit relative to this one's.
///
/// `name`, `id`, `os` and `side` have no defaults: there is no safe guess for
/// who a peer is or where it sits, so a missing one is reported with its line
/// number.
// Fields added here later must have defaults, so older files keep loading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerConfig {
    /// The name shown for the peer, such as "Windows PC".
    pub name: String,
    /// The peer's id: the `device.id` in the peer's own config file. Once
    /// the peer is paired, it is the first 32 characters of `fingerprint`.
    pub id: DeviceId,
    /// The peer's operating system: `macos`, `windows` or `linux`.
    pub os: Os,
    /// Which side of this computer's screens the peer is on.
    pub side: Side,
    /// A fixed network address for the peer, such as `192.168.1.20` or
    /// `krish-pc.local`. When unset, Hop looks for the peer on the local
    /// network (from M2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The fingerprint of the peer's key, written by pairing: the SHA-256 of
    /// its public key as 64 lowercase hex characters (see
    /// [`is_fingerprint`](crate::is_fingerprint)). Hop connects only to a
    /// computer that proves it holds that key, so a peer without one cannot
    /// connect until the two computers are paired. Unset by default, which
    /// is how files from before pairing existed load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl PeerConfig {
    /// The fingerprint Hop pins for this peer: `fingerprint`, when it is set,
    /// well formed, and `id` is its first 32 characters.
    ///
    /// `None` means Hop must not connect to the peer: it is not paired yet,
    /// or its id and fingerprint disagree. Pairing always writes the two
    /// together, so a disagreement means the entry was edited, and Hop
    /// cannot tell which computer it names. [`Config::load`] warns about that
    /// case ([`Warning::PeerIdMismatch`](crate::Warning::PeerIdMismatch)).
    pub fn pinned_fingerprint(&self) -> Option<&str> {
        let fingerprint = self.fingerprint.as_deref()?;
        (crate::device_id_from_fingerprint(fingerprint)? == self.id).then_some(fingerprint)
    }
}

/// Where a peer's screens sit relative to this computer's screens. The
/// cursor moves to the peer across that edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// To the left (`left` in the file).
    Left,
    /// To the right (`right` in the file).
    Right,
    /// Above (`above` in the file).
    Above,
    /// Below (`below` in the file).
    Below,
}

impl Side {
    /// The edge of this computer's screens that leads to the peer.
    pub const fn edge(self) -> Edge {
        match self {
            Side::Left => Edge::Left,
            Side::Right => Edge::Right,
            Side::Above => Edge::Top,
            Side::Below => Edge::Bottom,
        }
    }

    /// Where a peer on this side is, for use in sentences.
    pub(crate) const fn placement(self) -> &'static str {
        match self {
            Side::Left => "on the left",
            Side::Right => "on the right",
            Side::Above => "above",
            Side::Below => "below",
        }
    }
}

impl fmt::Display for Side {
    /// The spelling used in the config file.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Above => "above",
            Side::Below => "below",
        })
    }
}

/// Moving the cursor and keyboard between computers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    /// How long, in milliseconds, the cursor must push against a screen edge
    /// before it crosses to the peer. Default 0 (cross at once); 0 to 2000.
    #[serde(deserialize_with = "validate::read_edge_delay_ms")]
    pub edge_delay_ms: u64,
    /// Pushing within this many units of either end of an edge never
    /// crosses, so hot corners and the Start button stay usable. Default 10;
    /// 0 to 200, where 0 turns the dead zone off.
    #[serde(deserialize_with = "validate::read_corner_dead_zone")]
    pub corner_dead_zone: f64,
    /// Multiplier for cursor motion arriving from another computer. Default
    /// 1.0; 0.1 to 5.0.
    #[serde(deserialize_with = "validate::read_pointer_speed")]
    pub pointer_speed: f64,
    /// How far, in units, this computer's own mouse must move while another
    /// computer controls it to take control back. Default 4; 0 to 100.
    #[serde(deserialize_with = "validate::read_takeback_threshold")]
    pub takeback_threshold: f64,
    /// The keys that, pressed together on the controlling computer, end a
    /// session at once. Names follow the W3C `KeyboardEvent.code` list, such
    /// as `ControlLeft` or `Escape`; `hop-keymap` turns them into key codes.
    /// Default Left Control + Left Alt/Option + Left Shift + Escape.
    pub emergency_chord: Vec<String>,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            edge_delay_ms: 0,
            corner_dead_zone: 10.0,
            pointer_speed: 1.0,
            takeback_threshold: 4.0,
            emergency_chord: ["ControlLeft", "AltLeft", "ShiftLeft", "Escape"]
                .map(String::from)
                .to_vec(),
        }
    }
}

/// Shortcut translation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeymapConfig {
    /// Between a Mac and a PC, swap Command (or the Windows key) with
    /// Control, so Cmd+C typed on the Mac arrives on Windows as Ctrl+C and
    /// Ctrl+C typed on the PC arrives on the Mac as Cmd+C. Default true.
    pub swap_cmd_ctrl: bool,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            swap_cmd_ctrl: true,
        }
    }
}

/// Text clipboard sync.
///
/// Text that a password manager marks as concealed or transient is never
/// sent, and there is deliberately no setting to change that (requirement 18
/// in docs/DECISIONS.md).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Send copied text to the other computer when the cursor moves there.
    /// Default true.
    pub enabled: bool,
    /// The longest text, in bytes of UTF-8, that is sent; longer text is not
    /// sent at all rather than cut short. Default 1048576 (1 MiB), which is
    /// also the most the wire protocol carries; 1 to 1048576.
    #[serde(deserialize_with = "validate::read_max_text_bytes")]
    pub max_text_bytes: usize,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_text_bytes: 1_048_576,
        }
    }
}

/// Files sent from other computers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferConfig {
    /// The folder received files are saved in. It must be a full path ("~"
    /// is not expanded). When unset (the default), files go to "Hop
    /// Received" in the Downloads folder; see
    /// [`TransferConfig::received_dir_or_default`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_dir: Option<PathBuf>,
}

/// Networking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// The network port Hop uses. Default 47800, which stays clear of
    /// Synergy's and Deskflow's 24800; 1024 to 65535.
    #[serde(deserialize_with = "validate::read_port")]
    pub port: u16,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { port: 47800 }
    }
}

/// Whether a name or id is empty in practice: nothing but spaces.
pub(crate) fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert!(!config.device.has_id());
        assert!(config.peers.is_empty());
        assert_eq!(config.input.edge_delay_ms, 0);
        assert_eq!(config.input.corner_dead_zone, 10.0);
        assert_eq!(config.input.pointer_speed, 1.0);
        assert_eq!(config.input.takeback_threshold, 4.0);
        assert_eq!(
            config.input.emergency_chord,
            ["ControlLeft", "AltLeft", "ShiftLeft", "Escape"]
        );
        assert!(config.keymap.swap_cmd_ctrl);
        assert!(config.clipboard.enabled);
        assert_eq!(config.clipboard.max_text_bytes, 1_048_576);
        assert_eq!(config.transfer.received_dir, None);
        assert_eq!(config.network.port, 47800);
    }

    #[test]
    fn defaults_are_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn first_run_makes_a_random_lowercase_hex_id() {
        let first = Config::first_run("Krish's MacBook").unwrap();
        let second = Config::first_run("Krish's MacBook").unwrap();

        assert_eq!(first.device.name, "Krish's MacBook");
        let id = first.device.id.as_str();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_ne!(first.device.id, second.device.id);

        // Everything else is at its default.
        let mut rest = first.clone();
        rest.device = DeviceConfig::default();
        assert_eq!(rest, Config::default());
        assert!(first.validate().is_ok());
    }

    #[test]
    fn first_run_needs_a_name() {
        let error = Config::first_run("  ").unwrap_err();
        let ConfigError::Invalid { path: None, errors } = error else {
            panic!("expected Invalid, got {error:?}");
        };
        assert_eq!(
            errors,
            [ValidationError::Empty {
                key: "device.name".to_owned()
            }]
        );
    }

    #[test]
    fn the_id_from_the_key_replaces_the_random_one() {
        let fingerprint = "5f0c9a1e7b3d4c2a8e6f1b9d0a7c3e52c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0";
        let mut config = Config::first_run("Krish's MacBook").unwrap();
        let id = crate::device_id_from_fingerprint(fingerprint).unwrap();
        assert!(config.set_device_id(id.clone()), "the random id differs");
        assert_eq!(config.device.id, id);
        assert_eq!(config.device.name, "Krish's MacBook");
        assert!(config.validate().is_ok());
        // At the next start the id is already right, so nothing needs saving.
        assert!(!config.set_device_id(id));
    }

    #[test]
    fn a_peer_is_pinned_only_when_its_id_matches_its_fingerprint() {
        let fingerprint = "c41d8e2b9a7f4e06b3d5a1f8e9c2b7d05a3f19c7e2b8d4a60f1e9c3b7d2a8e45";
        let mut peer = PeerConfig {
            name: "Windows PC".to_owned(),
            id: DeviceId::new("c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0"),
            os: Os::Windows,
            side: Side::Right,
            address: None,
            fingerprint: None,
        };
        assert_eq!(peer.pinned_fingerprint(), None, "not paired yet");
        peer.fingerprint = Some(fingerprint.to_owned());
        assert_eq!(peer.pinned_fingerprint(), Some(fingerprint));
        peer.id = DeviceId::new("0123456789abcdef0123456789abcdef");
        assert_eq!(
            peer.pinned_fingerprint(),
            None,
            "id and fingerprint disagree"
        );
        peer.id = DeviceId::new("C41D8E2B9A7F4E06B3D5A1F8E9C2B7D0");
        assert_eq!(peer.pinned_fingerprint(), None, "ids are compared exactly");
        peer.id = DeviceId::new("c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0");
        peer.fingerprint = Some(fingerprint.to_uppercase());
        assert_eq!(peer.pinned_fingerprint(), None, "not a fingerprint");
    }

    #[test]
    fn hex_is_two_lowercase_digits_per_byte() {
        assert_eq!(lowercase_hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(lowercase_hex(&[]), "");
    }

    #[test]
    fn has_id_ignores_spaces() {
        let mut device = DeviceConfig::default();
        assert!(!device.has_id());
        device.id = DeviceId::new("  ");
        assert!(!device.has_id());
        device.id = DeviceId::new("3f9a");
        assert!(device.has_id());
    }

    #[test]
    fn sides_map_to_edges_and_spell_like_the_file() {
        assert_eq!(Side::Left.edge(), Edge::Left);
        assert_eq!(Side::Right.edge(), Edge::Right);
        assert_eq!(Side::Above.edge(), Edge::Top);
        assert_eq!(Side::Below.edge(), Edge::Bottom);
        assert_eq!(Side::Above.to_string(), "above");
    }
}
