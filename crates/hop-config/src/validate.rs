//! The rules a config must follow, and the messages for breaking them.

use std::collections::BTreeMap;
use std::fmt::{self, Display};
use std::ops::RangeInclusive;
use std::path::PathBuf;

use hop_types::DeviceId;
use serde::Deserializer;
use serde::de::{self, Unexpected, Visitor};

use crate::config::is_blank;
use crate::{Config, ConfigError, Side};

const EDGE_DELAY_MS: RangeInclusive<u64> = 0..=2000;
const CORNER_DEAD_ZONE: RangeInclusive<f64> = 0.0..=200.0;
const POINTER_SPEED: RangeInclusive<f64> = 0.1..=5.0;
const TAKEBACK_THRESHOLD: RangeInclusive<f64> = 0.0..=100.0;
/// Up to the wire protocol's 1 MiB limit on clipboard text: anything longer
/// could never be sent. 0 would silently stop all syncing; `enabled = false`
/// says that plainly.
const MAX_TEXT_BYTES: RangeInclusive<usize> = 1..=1_048_576;
/// Ports below 1024 need administrator rights on some systems.
const PORT: RangeInclusive<u16> = 1024..=65535;

/// One rule a config breaks. [`ConfigError::Invalid`] lists every one found.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// A number is outside its allowed range.
    #[error("{key} is {value}, but it must be between {min} and {max}")]
    OutOfRange {
        /// The setting, such as `input.pointer_speed`.
        key: &'static str,
        /// The value found.
        value: String,
        /// The smallest allowed value.
        min: String,
        /// The largest allowed value.
        max: String,
    },

    /// A name, id, address or key name is empty or only spaces.
    #[error("{key} must not be empty")]
    Empty {
        /// The setting, such as `device.name`, `peers[1].id` or
        /// `input.emergency_chord[0]`.
        key: String,
    },

    /// The emergency chord lists no keys, which would leave no keyboard way
    /// to end a session.
    #[error("input.emergency_chord must list at least one key")]
    NoChordKeys,

    /// Two peers are on the same side of this computer.
    #[error(
        "peers[{first}] and peers[{second}] are both {}; only one peer can be on each side",
        .side.placement()
    )]
    SideTaken {
        /// The side they share.
        side: Side,
        /// The position of the first peer on that side in the peer list.
        first: usize,
        /// The position of the later one.
        second: usize,
    },

    /// Two peers have the same id.
    #[error(
        "peers[{first}] and peers[{second}] both have the id \"{id}\"; each peer needs its own id"
    )]
    DuplicatePeerId {
        /// The shared id.
        id: DeviceId,
        /// The position of the first peer with that id in the peer list.
        first: usize,
        /// The position of the later one.
        second: usize,
    },

    /// A peer has this computer's own id.
    #[error(
        "peers[{index}] has this computer's own id \"{id}\"; use the other computer's device.id"
    )]
    PeerIsThisDevice {
        /// The peer's position in the peer list.
        index: usize,
        /// The id.
        id: DeviceId,
    },

    /// The folder for received files is not a full path.
    #[error(
        "transfer.received_dir is \"{}\", but it must be a full path (\"~\" is not expanded)",
        .path.display()
    )]
    RelativeReceivedDir {
        /// The path found.
        path: PathBuf,
    },
}

impl Config {
    /// Checks every rule: number ranges, at most one peer per side, unique
    /// peer ids that differ from this device's, and no empty names. Key
    /// names in the emergency chord are only checked for being non-empty;
    /// `hop-keymap` decides whether it knows them.
    ///
    /// A device without an id (one not set up yet) passes, so the defaults
    /// are valid.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`], listing every broken rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let errors = self.problems();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid { path: None, errors })
        }
    }

    /// Every broken rule, in file order.
    pub(crate) fn problems(&self) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        self.check_device(&mut errors);
        self.check_peers(&mut errors);
        self.check_input(&mut errors);
        check_range(
            "clipboard.max_text_bytes",
            self.clipboard.max_text_bytes,
            MAX_TEXT_BYTES,
            &mut errors,
        );
        if let Some(dir) = &self.transfer.received_dir {
            // A relative folder would land wherever the app happens to run.
            if !dir.is_absolute() {
                errors.push(ValidationError::RelativeReceivedDir { path: dir.clone() });
            }
        }
        check_range("network.port", self.network.port, PORT, &mut errors);
        errors
    }

    fn check_device(&self, errors: &mut Vec<ValidationError>) {
        // Without an id the device is not set up yet, and first-run setup
        // supplies the name too.
        if self.device.has_id() && is_blank(&self.device.name) {
            errors.push(empty("device.name".to_owned()));
        }
    }

    fn check_peers(&self, errors: &mut Vec<ValidationError>) {
        // At most four sides, so a list beats a map.
        let mut sides: Vec<(Side, usize)> = Vec::new();
        let mut ids: BTreeMap<&DeviceId, usize> = BTreeMap::new();
        for (index, peer) in self.peers.iter().enumerate() {
            if is_blank(&peer.name) {
                errors.push(empty(format!("peers[{index}].name")));
            }
            if is_blank(peer.id.as_str()) {
                errors.push(empty(format!("peers[{index}].id")));
            } else if self.device.has_id() && peer.id == self.device.id {
                errors.push(ValidationError::PeerIsThisDevice {
                    index,
                    id: peer.id.clone(),
                });
            } else if let Some(&first) = ids.get(&peer.id) {
                errors.push(ValidationError::DuplicatePeerId {
                    id: peer.id.clone(),
                    first,
                    second: index,
                });
            } else {
                ids.insert(&peer.id, index);
            }
            if peer.address.as_deref().is_some_and(is_blank) {
                errors.push(empty(format!("peers[{index}].address")));
            }
            match sides.iter().find(|(side, _)| *side == peer.side) {
                Some(&(side, first)) => errors.push(ValidationError::SideTaken {
                    side,
                    first,
                    second: index,
                }),
                None => sides.push((peer.side, index)),
            }
        }
    }

    fn check_input(&self, errors: &mut Vec<ValidationError>) {
        let input = &self.input;
        check_range(
            "input.edge_delay_ms",
            input.edge_delay_ms,
            EDGE_DELAY_MS,
            errors,
        );
        check_range(
            "input.corner_dead_zone",
            input.corner_dead_zone,
            CORNER_DEAD_ZONE,
            errors,
        );
        check_range(
            "input.pointer_speed",
            input.pointer_speed,
            POINTER_SPEED,
            errors,
        );
        check_range(
            "input.takeback_threshold",
            input.takeback_threshold,
            TAKEBACK_THRESHOLD,
            errors,
        );
        if input.emergency_chord.is_empty() {
            errors.push(ValidationError::NoChordKeys);
        }
        for (index, key) in input.emergency_chord.iter().enumerate() {
            if is_blank(key) {
                errors.push(empty(format!("input.emergency_chord[{index}]")));
            }
        }
    }
}

/// Records an error unless `value` is in `range`. NaN is never in range.
fn check_range<T: PartialOrd + Display>(
    key: &'static str,
    value: T,
    range: RangeInclusive<T>,
    errors: &mut Vec<ValidationError>,
) {
    if !range.contains(&value) {
        errors.push(ValidationError::OutOfRange {
            key,
            value: value.to_string(),
            min: range.start().to_string(),
            max: range.end().to_string(),
        });
    }
}

fn empty(key: String) -> ValidationError {
    ValidationError::Empty { key }
}

// Readers for the numeric settings (`#[serde(deserialize_with)]`). Without
// them, a number its setting's type cannot hold (`port = 70000`, a negative
// delay) or a value of the wrong kind is reported as "expected u16" or
// "expected f64". These name the allowed range instead. A number the type
// can hold is always accepted here, even outside the range, so validation
// reports it together with every other problem.

pub(crate) fn read_edge_delay_ms<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    d.deserialize_u64(WholeNumber(EDGE_DELAY_MS))
}

pub(crate) fn read_corner_dead_zone<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    d.deserialize_f64(Number(CORNER_DEAD_ZONE))
}

pub(crate) fn read_pointer_speed<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    d.deserialize_f64(Number(POINTER_SPEED))
}

pub(crate) fn read_takeback_threshold<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    d.deserialize_f64(Number(TAKEBACK_THRESHOLD))
}

pub(crate) fn read_max_text_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    d.deserialize_u64(WholeNumber(MAX_TEXT_BYTES))
}

pub(crate) fn read_port<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    d.deserialize_u16(WholeNumber(PORT))
}

/// Reads a whole number of type `T`; the range is only for the message.
struct WholeNumber<T>(RangeInclusive<T>);

impl<T> Visitor<'_> for WholeNumber<T>
where
    T: TryFrom<i64> + TryFrom<u64> + Display,
{
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a whole number from {} to {}",
            self.0.start(),
            self.0.end()
        )
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<T, E> {
        T::try_from(value).map_err(|_| E::invalid_value(Unexpected::Signed(value), &self))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<T, E> {
        T::try_from(value).map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
    }
}

/// Reads a number, whole or not; the range is only for the message.
struct Number(RangeInclusive<f64>);

impl Visitor<'_> for Number {
    type Value = f64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a number from {} to {}", self.0.start(), self.0.end())
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<f64, E> {
        Ok(value)
    }

    // TOML writes `10` and `10.0` differently; both mean ten here.
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<f64, E> {
        Ok(value as f64)
    }
}

#[cfg(test)]
mod tests {
    use hop_types::Os;

    use super::*;
    use crate::PeerConfig;

    /// A valid setup to break one rule at a time.
    fn mac() -> Config {
        Config {
            device: crate::DeviceConfig {
                name: "Krish's MacBook".to_owned(),
                id: DeviceId::new("aaaa"),
            },
            peers: vec![peer("Windows PC", "bbbb", Side::Right)],
            ..Config::default()
        }
    }

    fn peer(name: &str, id: &str, side: Side) -> PeerConfig {
        PeerConfig {
            name: name.to_owned(),
            id: DeviceId::new(id),
            os: Os::Windows,
            side,
            address: None,
        }
    }

    fn out_of_range(key: &'static str, value: &str, min: &str, max: &str) -> ValidationError {
        ValidationError::OutOfRange {
            key,
            value: value.to_owned(),
            min: min.to_owned(),
            max: max.to_owned(),
        }
    }

    fn empty_key(key: &str) -> ValidationError {
        empty(key.to_owned())
    }

    #[test]
    fn a_normal_setup_is_valid() {
        assert_eq!(mac().problems(), []);
        assert!(mac().validate().is_ok());
    }

    #[test]
    fn validate_returns_every_problem() {
        let mut config = mac();
        config.input.pointer_speed = 0.0;
        config.network.port = 80;
        let Err(ConfigError::Invalid { path: None, errors }) = config.validate() else {
            panic!("expected Invalid");
        };
        assert_eq!(
            errors,
            [
                out_of_range("input.pointer_speed", "0", "0.1", "5"),
                out_of_range("network.port", "80", "1024", "65535"),
            ]
        );
    }

    #[test]
    fn pointer_speed_range() {
        for (speed, ok) in [(0.1, true), (5.0, true), (0.09, false), (5.5, false)] {
            let mut config = mac();
            config.input.pointer_speed = speed;
            assert_eq!(config.problems().is_empty(), ok, "pointer_speed {speed}");
        }
        let mut config = mac();
        config.input.pointer_speed = 9.0;
        assert_eq!(
            config.problems(),
            [out_of_range("input.pointer_speed", "9", "0.1", "5")]
        );
    }

    #[test]
    fn not_a_number_is_out_of_range() {
        let mut config = mac();
        config.input.pointer_speed = f64::NAN;
        config.input.corner_dead_zone = f64::INFINITY;
        assert_eq!(
            config.problems(),
            [
                out_of_range("input.corner_dead_zone", "inf", "0", "200"),
                out_of_range("input.pointer_speed", "NaN", "0.1", "5"),
            ]
        );
    }

    #[test]
    fn corner_dead_zone_range() {
        let mut config = mac();
        config.input.corner_dead_zone = 200.0;
        assert_eq!(config.problems(), []);
        config.input.corner_dead_zone = -1.0;
        assert_eq!(
            config.problems(),
            [out_of_range("input.corner_dead_zone", "-1", "0", "200")]
        );
        config.input.corner_dead_zone = 200.5;
        assert_eq!(
            config.problems(),
            [out_of_range("input.corner_dead_zone", "200.5", "0", "200")]
        );
    }

    #[test]
    fn edge_delay_range() {
        let mut config = mac();
        config.input.edge_delay_ms = 2000;
        assert_eq!(config.problems(), []);
        config.input.edge_delay_ms = 2001;
        assert_eq!(
            config.problems(),
            [out_of_range("input.edge_delay_ms", "2001", "0", "2000")]
        );
    }

    #[test]
    fn takeback_threshold_range() {
        let mut config = mac();
        config.input.takeback_threshold = 0.0;
        assert_eq!(config.problems(), []);
        config.input.takeback_threshold = 100.1;
        assert_eq!(
            config.problems(),
            [out_of_range(
                "input.takeback_threshold",
                "100.1",
                "0",
                "100"
            )]
        );
        config.input.takeback_threshold = -0.5;
        assert_eq!(
            config.problems(),
            [out_of_range("input.takeback_threshold", "-0.5", "0", "100")]
        );
    }

    #[test]
    fn port_range() {
        let mut config = mac();
        for port in [1024, 47800, 65535] {
            config.network.port = port;
            assert_eq!(config.problems(), [], "port {port}");
        }
        config.network.port = 1023;
        assert_eq!(
            config.problems(),
            [out_of_range("network.port", "1023", "1024", "65535")]
        );
    }

    #[test]
    fn clipboard_limit_stays_within_the_protocol() {
        let mut config = mac();
        config.clipboard.max_text_bytes = 1;
        assert_eq!(config.problems(), []);
        config.clipboard.max_text_bytes = 1_048_577;
        assert_eq!(
            config.problems(),
            [out_of_range(
                "clipboard.max_text_bytes",
                "1048577",
                "1",
                "1048576"
            )]
        );
        config.clipboard.max_text_bytes = 0;
        assert_eq!(
            config.problems(),
            [out_of_range(
                "clipboard.max_text_bytes",
                "0",
                "1",
                "1048576"
            )]
        );
    }

    #[test]
    fn one_peer_per_side() {
        let mut config = mac();
        config.peers.push(peer("Old PC", "cccc", Side::Below));
        config.peers.push(peer("Work PC", "dddd", Side::Right));
        config.peers.push(peer("Spare PC", "eeee", Side::Right));
        let errors = config.problems();
        assert_eq!(
            errors,
            [
                ValidationError::SideTaken {
                    side: Side::Right,
                    first: 0,
                    second: 2
                },
                ValidationError::SideTaken {
                    side: Side::Right,
                    first: 0,
                    second: 3
                },
            ]
        );
        assert_eq!(
            errors[0].to_string(),
            "peers[0] and peers[2] are both on the right; only one peer can be on each side"
        );
    }

    #[test]
    fn peer_ids_are_unique() {
        let mut config = mac();
        config.peers.push(peer("Same PC again", "bbbb", Side::Left));
        let errors = config.problems();
        assert_eq!(
            errors,
            [ValidationError::DuplicatePeerId {
                id: DeviceId::new("bbbb"),
                first: 0,
                second: 1
            }]
        );
        assert_eq!(
            errors[0].to_string(),
            "peers[0] and peers[1] both have the id \"bbbb\"; each peer needs its own id"
        );
    }

    #[test]
    fn a_peer_is_not_this_device() {
        let mut config = mac();
        config.peers[0].id = DeviceId::new("aaaa");
        let errors = config.problems();
        assert_eq!(
            errors,
            [ValidationError::PeerIsThisDevice {
                index: 0,
                id: DeviceId::new("aaaa")
            }]
        );
        assert_eq!(
            errors[0].to_string(),
            "peers[0] has this computer's own id \"aaaa\"; use the other computer's device.id"
        );
    }

    #[test]
    fn names_and_ids_must_not_be_empty() {
        let mut config = mac();
        config.device.name = " ".to_owned();
        config.peers[0].name = String::new();
        config.peers[0].id = DeviceId::new("");
        config.peers[0].address = Some("  ".to_owned());
        assert_eq!(
            config.problems(),
            [
                empty_key("device.name"),
                empty_key("peers[0].name"),
                empty_key("peers[0].id"),
                empty_key("peers[0].address"),
            ]
        );
    }

    #[test]
    fn a_device_without_an_id_needs_no_name() {
        let mut config = mac();
        config.device = crate::DeviceConfig::default();
        assert_eq!(config.problems(), []);
        // A named device still waiting for its id is fine too.
        config.device.name = "Krish's MacBook".to_owned();
        assert_eq!(config.problems(), []);
    }

    #[test]
    fn chord_names_must_not_be_empty() {
        let mut config = mac();
        config.input.emergency_chord = vec!["ControlLeft".to_owned(), String::new()];
        assert_eq!(config.problems(), [empty_key("input.emergency_chord[1]")]);
        assert_eq!(
            config.problems()[0].to_string(),
            "input.emergency_chord[1] must not be empty"
        );
    }

    #[test]
    fn the_chord_needs_a_key() {
        let mut config = mac();
        config.input.emergency_chord.clear();
        assert_eq!(config.problems(), [ValidationError::NoChordKeys]);
    }

    #[test]
    fn chord_names_are_not_checked_against_a_key_table() {
        let mut config = mac();
        config.input.emergency_chord = vec!["NotARealKey".to_owned()];
        assert_eq!(config.problems(), []);
    }

    #[test]
    fn received_dir_must_be_a_full_path() {
        let mut config = mac();
        config.transfer.received_dir = Some(std::env::temp_dir().join("Hop Received"));
        assert_eq!(config.problems(), []);
        for relative in ["~/Downloads/Hop", "Downloads", ""] {
            config.transfer.received_dir = Some(PathBuf::from(relative));
            assert_eq!(
                config.problems(),
                [ValidationError::RelativeReceivedDir {
                    path: PathBuf::from(relative)
                }]
            );
        }
        assert_eq!(
            config.problems()[0].to_string(),
            "transfer.received_dir is \"\", but it must be a full path (\"~\" is not expanded)"
        );
    }

    #[test]
    fn range_message_reads_plainly() {
        let mut config = mac();
        config.input.pointer_speed = 9.0;
        assert_eq!(
            config.problems()[0].to_string(),
            "input.pointer_speed is 9, but it must be between 0.1 and 5"
        );
    }
}
