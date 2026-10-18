//! Reading and writing the config file.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::{Config, ConfigError, is_fingerprint};

/// Written at the top of every saved file.
const HEADER: &str = "\
# Hop configuration. Hop may rewrite this file, and comments are not kept.
";

/// Every key this build understands, by section. `peers` lists the keys of
/// one `[[peers]]` entry. A test checks this against the settings types.
const KNOWN_KEYS: &[(&str, &[&str])] = &[
    ("device", &["name", "id"]),
    (
        "peers",
        &["name", "id", "os", "side", "address", "fingerprint"],
    ),
    (
        "input",
        &[
            "edge_delay_ms",
            "corner_dead_zone",
            "pointer_speed",
            "takeback_threshold",
            "emergency_chord",
        ],
    ),
    ("keymap", &["swap_cmd_ctrl"]),
    ("clipboard", &["enabled", "max_text_bytes"]),
    ("transfer", &["received_dir"]),
    ("network", &["port"]),
];

/// How many temporary names a save tries before giving up.
const TEMP_NAME_ATTEMPTS: u32 = 100;

/// Something in a config file that Hop ignored. Loading still succeeds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Warning {
    /// A setting this version of Hop does not know: a typo, or a setting
    /// from a newer version of Hop.
    UnknownKey {
        /// Where it is, such as `input.pointer_sped` or `peers[0].nickname`.
        key: String,
    },
    /// A peer whose `id` is not the first 32 characters of its
    /// `fingerprint`. Pairing always writes the two together, so the entry
    /// was edited; Hop cannot tell which computer it names and will not
    /// connect to it (see [`PeerConfig::pinned_fingerprint`]). Pairing the
    /// two computers again fixes it.
    ///
    /// [`PeerConfig::pinned_fingerprint`]: crate::PeerConfig::pinned_fingerprint
    PeerIdMismatch {
        /// The peer's position in the peer list.
        index: usize,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::UnknownKey { key } => write!(
                f,
                "ignored unknown setting {key} (a typo, or a setting from a newer version of Hop)"
            ),
            Warning::PeerIdMismatch { index } => write!(
                f,
                "ignored peers[{index}]: its id is not the first 32 characters of its \
                 fingerprint, so Hop cannot tell which computer it is and will not connect \
                 to it; pair the two computers again to fix it"
            ),
        }
    }
}

impl Config {
    /// Reads and checks the config file at `path`.
    ///
    /// A missing file gives the defaults, as on a first run. Settings the
    /// file leaves out take their defaults. Settings this version of Hop does
    /// not know are skipped and reported as warnings, so an older Hop can
    /// read a file written by a newer one. A peer whose id does not match its
    /// fingerprint is reported as a warning too ([`Warning::PeerIdMismatch`]):
    /// it stays in the file, but Hop does not connect to it.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Read`] if the file exists but cannot be read.
    /// - [`ConfigError::Parse`] if it is not valid TOML or a setting has the
    ///   wrong type; the source error gives the line and column.
    /// - [`ConfigError::Invalid`] if the settings break a rule (see
    ///   [`Config::validate`]).
    pub fn load(path: impl AsRef<Path>) -> Result<(Config, Vec<Warning>), ConfigError> {
        let path = path.as_ref();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Config::default(), Vec::new()));
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let (config, warnings) = parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        invalid_at(path, &config)?;
        Ok((config, warnings))
    }

    /// Writes the config to `path`, creating missing parent folders.
    ///
    /// The write is atomic: the text goes to a temporary file in the same
    /// folder, which is flushed to disk and then renamed over `path`, so a
    /// crash or power cut leaves either the old file or the new one, never a
    /// mix. Comments and unknown settings in an existing file are not kept.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Invalid`] if the settings break a rule; nothing is
    ///   written, so Hop never saves a file it would refuse to load.
    /// - [`ConfigError::Encode`] if the settings cannot be written as TOML.
    /// - [`ConfigError::Write`] if the folder or file cannot be written.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        invalid_at(path, self)?;
        let text = self.to_toml()?;
        write_atomically(path, text.as_bytes()).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The text [`Config::save`] writes.
    pub(crate) fn to_toml(&self) -> Result<String, ConfigError> {
        let body = toml::to_string(self).map_err(ConfigError::Encode)?;
        Ok(format!("{HEADER}\n{body}"))
    }
}

/// Reads config text into settings, plus a warning for every key ignored
/// and every peer Hop will not connect to because its id and fingerprint
/// disagree.
pub(crate) fn parse(text: &str) -> Result<(Config, Vec<Warning>), toml::de::Error> {
    // Reading the text twice keeps the typed read's precise error messages,
    // while the untyped read shows which keys the typed one skipped.
    let config: Config = toml::from_str(text)?;
    let table: Table = toml::from_str(text)?;
    let mut warnings = unknown_keys(&table);
    warnings.extend(mismatched_peers(&config));
    warnings.sort();
    Ok((config, warnings))
}

/// A warning for every peer whose well-formed fingerprint does not give its
/// id. A malformed fingerprint is left to validation, which rejects it.
fn mismatched_peers(config: &Config) -> impl Iterator<Item = Warning> + '_ {
    config
        .peers
        .iter()
        .enumerate()
        .filter(|(_, peer)| {
            peer.fingerprint.as_deref().is_some_and(is_fingerprint)
                && peer.pinned_fingerprint().is_none()
        })
        .map(|(index, _)| Warning::PeerIdMismatch { index })
}

fn invalid_at(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let errors = config.problems();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            path: Some(path.to_path_buf()),
            errors,
        })
    }
}

/// A warning for every key in `file` that is not in [`KNOWN_KEYS`], sorted.
/// An unknown section gives one warning, not one per key inside it.
fn unknown_keys(file: &Table) -> Vec<Warning> {
    let mut warnings = Vec::new();
    for (section, value) in file {
        let Some(&(_, known)) = KNOWN_KEYS.iter().find(|(name, _)| name == section) else {
            warnings.push(unknown(display_key(section).into_owned()));
            continue;
        };
        let section = display_key(section);
        match value {
            Value::Table(table) => unknown_in(table, known, &section, &mut warnings),
            Value::Array(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    if let Value::Table(table) = entry {
                        unknown_in(table, known, &format!("{section}[{index}]"), &mut warnings);
                    }
                }
            }
            // Any other shape was already rejected by the typed read.
            _ => {}
        }
    }
    // Sorting makes the order independent of how the TOML library stores
    // tables, which a feature flag elsewhere in the build could change.
    warnings.sort();
    warnings
}

fn unknown_in(table: &Table, known: &[&str], prefix: &str, warnings: &mut Vec<Warning>) {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            warnings.push(unknown(format!("{prefix}.{}", display_key(key))));
        }
    }
}

fn unknown(key: String) -> Warning {
    Warning::UnknownKey { key }
}

/// A key as it would be written in TOML: quoted unless it is a bare key, so
/// a key containing a dot cannot be mistaken for a nested one.
fn display_key(key: &str) -> Cow<'_, str> {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        Cow::Borrowed(key)
    } else {
        Cow::Owned(format!("{key:?}"))
    }
}

/// Replaces the file at `path` with `contents` so that readers see either
/// the old file or the new one, never a partial write.
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the path does not name a file")
    })?;
    // A bare file name has an empty parent, meaning the current folder.
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;

    // The temporary file must be in the same folder: a rename is only atomic
    // within one file system.
    let (temp_path, mut file) = create_temp_file(dir, file_name)?;
    // Flushing before the rename keeps a crash from leaving an empty file
    // under the real name.
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    // Windows cannot rename a file that is still open.
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp_path, path));
    if result.is_err() {
        // Best effort: the error worth reporting is the one that got us here.
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Creates a new, uniquely named file next to the one being saved.
fn create_temp_file(dir: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    let pid = std::process::id();
    for attempt in 0..TEMP_NAME_ATTEMPTS {
        let mut name = OsString::from(".");
        name.push(file_name);
        name.push(format!(".{pid}.{attempt}.tmp"));
        let temp_path = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            // Another save is in progress, or one crashed: try the next name.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every temporary file name is taken",
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hop_types::{DeviceId, Os};
    use proptest::prelude::*;

    use super::*;
    use crate::{PeerConfig, Side, TransferConfig};

    fn unknown_key(key: &str) -> Warning {
        unknown(key.to_owned())
    }

    const PC_FINGERPRINT: &str = "c41d8e2b9a7f4e06b3d5a1f8e9c2b7d05a3f19c7e2b8d4a60f1e9c3b7d2a8e45";

    /// A paired PC: its id is the start of its fingerprint.
    fn windows_pc() -> PeerConfig {
        PeerConfig {
            name: "Windows PC".to_owned(),
            id: DeviceId::new(&PC_FINGERPRINT[..32]),
            os: Os::Windows,
            side: Side::Right,
            address: Some("192.168.1.20".to_owned()),
            fingerprint: Some(PC_FINGERPRINT.to_owned()),
        }
    }

    #[test]
    fn empty_text_gives_the_defaults() {
        assert_eq!(parse("").unwrap(), (Config::default(), vec![]));
        assert_eq!(
            parse("# only a comment\n\n").unwrap(),
            (Config::default(), vec![])
        );
    }

    #[test]
    fn partial_sections_keep_the_other_defaults() {
        let (config, warnings) =
            parse("[input]\npointer_speed = 1.5\n\n[clipboard]\nenabled = false\n").unwrap();
        assert_eq!(warnings, []);
        let mut expected = Config::default();
        expected.input.pointer_speed = 1.5;
        expected.clipboard.enabled = false;
        assert_eq!(config, expected);
    }

    #[test]
    fn whole_numbers_work_for_distances() {
        let (config, _) = parse("[input]\ncorner_dead_zone = 20\n").unwrap();
        assert_eq!(config.input.corner_dead_zone, 20.0);
    }

    #[test]
    fn peers_need_their_identity() {
        let error =
            parse("[[peers]]\nname = \"PC\"\nid = \"bbbb\"\nos = \"windows\"\n").unwrap_err();
        assert!(error.message().contains("missing field `side`"), "{error}");
        // The address is optional.
        let (config, _) =
            parse("[[peers]]\nname = \"PC\"\nid = \"bbbb\"\nos = \"windows\"\nside = \"right\"\n")
                .unwrap();
        assert_eq!(config.peers[0].address, None);
    }

    #[test]
    fn wrong_types_and_bad_syntax_name_the_line() {
        let cases = [
            (
                "[input]\npointer_speed = \"fast\"\n",
                "line 2",
                "expected a number from 0.1 to 5",
            ),
            (
                "[network]\nport = 70000\n",
                "line 2",
                "expected a whole number from 1024 to 65535",
            ),
            ("[input\n", "line 1", "unclosed table"),
            (
                "[[peers]]\nname = \"PC\"\nid = \"b\"\nos = \"windows\"\nside = \"top\"\n",
                "line 5",
                "expected one of `left`, `right`, `above`, `below`",
            ),
            (
                "[[peers]]\nname = \"PC\"\nid = \"b\"\nos = \"macOS\"\nside = \"left\"\n",
                "line 4",
                "unknown variant `macOS`",
            ),
        ];
        for (text, line, message) in cases {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(line), "{text:?}: {error}");
            assert!(error.contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn numbers_of_the_wrong_kind_or_size_name_the_allowed_range() {
        // A value its setting's type cannot hold never reaches validation, so
        // the message comes from reading the file. It must still say what
        // the setting accepts, not "expected u16".
        let cases = [
            (
                "[network]\nport = 70000\n",
                "a whole number from 1024 to 65535",
            ),
            (
                "[network]\nport = -1\n",
                "a whole number from 1024 to 65535",
            ),
            (
                "[input]\nedge_delay_ms = -5\n",
                "a whole number from 0 to 2000",
            ),
            (
                "[input]\nedge_delay_ms = 1.5\n",
                "a whole number from 0 to 2000",
            ),
            (
                "[clipboard]\nmax_text_bytes = -1\n",
                "a whole number from 1 to 1048576",
            ),
            (
                "[input]\npointer_speed = \"fast\"\n",
                "a number from 0.1 to 5",
            ),
            (
                "[input]\ncorner_dead_zone = true\n",
                "a number from 0 to 200",
            ),
            (
                "[input]\ntakeback_threshold = \"4\"\n",
                "a number from 0 to 100",
            ),
        ];
        for (text, expected) in cases {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains("line 2"), "{text:?}: {error}");
            assert!(error.contains(expected), "{text:?}: {error}");
            for type_name in ["u16", "u64", "usize", "f64"] {
                assert!(!error.contains(type_name), "{text:?}: {error}");
            }
        }
    }

    #[test]
    fn in_type_numbers_out_of_range_are_left_to_validation() {
        // So that one load reports every problem, with the setting's name.
        let (config, _) = parse("[network]\nport = 80\n\n[input]\nedge_delay_ms = 5000\n").unwrap();
        assert_eq!(config.network.port, 80);
        assert_eq!(config.input.edge_delay_ms, 5000);
        assert_eq!(config.problems().len(), 2);
    }

    #[test]
    fn unknown_keys_warn_and_the_rest_still_loads() {
        let text = "\
theme = \"dark\"

[device]
name = \"Mac\"
id = \"aaaa\"
colour = \"blue\"

[[peers]]
name = \"PC\"
id = \"bbbb\"
os = \"windows\"
side = \"right\"
nickname = \"pc\"

[input]
pointer_speed = 2.0
acceleration = true
\"edge.delay\" = 5

[future]
anything = 1
";
        let (config, warnings) = parse(text).unwrap();
        assert_eq!(config.device.name, "Mac");
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.input.pointer_speed, 2.0);
        assert_eq!(
            warnings,
            [
                unknown_key("device.colour"),
                unknown_key("future"),
                unknown_key("input.\"edge.delay\""),
                unknown_key("input.acceleration"),
                unknown_key("peers[0].nickname"),
                unknown_key("theme"),
            ]
        );
        assert_eq!(
            warnings[0].to_string(),
            "ignored unknown setting device.colour \
             (a typo, or a setting from a newer version of Hop)"
        );
    }

    #[test]
    fn inline_peer_tables_are_checked_too() {
        let text = "peers = [{ name = \"PC\", id = \"b\", os = \"windows\", side = \"left\", extra = 1 }]\n";
        let (config, warnings) = parse(text).unwrap();
        assert_eq!(config.peers[0].side, Side::Left);
        assert_eq!(warnings, [unknown_key("peers[0].extra")]);
    }

    #[test]
    fn a_peer_without_a_fingerprint_loads_unpaired() {
        // Every file from before pairing existed looks like this.
        let text = "[[peers]]\nname = \"PC\"\nid = \"bbbb\"\nos = \"windows\"\nside = \"right\"\n";
        let (config, warnings) = parse(text).unwrap();
        assert_eq!(warnings, []);
        assert_eq!(config.peers[0].fingerprint, None);
        assert_eq!(config.peers[0].pinned_fingerprint(), None);
        assert_eq!(config.problems(), []);
    }

    #[test]
    fn a_paired_peer_loads_with_its_pin() {
        let text = format!(
            "[[peers]]\nname = \"PC\"\nid = \"{}\"\nos = \"windows\"\nside = \"right\"\n\
             fingerprint = \"{PC_FINGERPRINT}\"\n",
            &PC_FINGERPRINT[..32]
        );
        let (config, warnings) = parse(&text).unwrap();
        assert_eq!(warnings, []);
        assert_eq!(config.peers[0].pinned_fingerprint(), Some(PC_FINGERPRINT));
        assert_eq!(config.problems(), []);
    }

    #[test]
    fn a_peer_whose_id_does_not_match_its_fingerprint_is_warned_about() {
        let text = format!(
            "[[peers]]\nname = \"PC\"\nid = \"bbbb\"\nos = \"windows\"\nside = \"right\"\n\
             fingerprint = \"{PC_FINGERPRINT}\"\nnickname = \"pc\"\n"
        );
        let (config, warnings) = parse(&text).unwrap();
        // Unknown keys first, then peers, so the order never depends on
        // how the TOML library stores tables.
        assert_eq!(
            warnings,
            [
                unknown_key("peers[0].nickname"),
                Warning::PeerIdMismatch { index: 0 },
            ]
        );
        assert_eq!(
            warnings[1].to_string(),
            "ignored peers[0]: its id is not the first 32 characters of its fingerprint, \
             so Hop cannot tell which computer it is and will not connect to it; \
             pair the two computers again to fix it"
        );
        // The entry is kept, so saving does not lose it, but it is not pinned.
        assert_eq!(config.peers[0].fingerprint.as_deref(), Some(PC_FINGERPRINT));
        assert_eq!(config.peers[0].pinned_fingerprint(), None);
        assert_eq!(config.problems(), []);
    }

    #[test]
    fn a_malformed_fingerprint_is_left_to_validation() {
        // Only one message for it: the error, not a warning as well.
        let text = "[[peers]]\nname = \"PC\"\nid = \"bbbb\"\nos = \"windows\"\nside = \"right\"\n\
                    fingerprint = \"C41D\"\n";
        let (config, warnings) = parse(text).unwrap();
        assert_eq!(warnings, []);
        assert_eq!(
            config.problems(),
            [crate::ValidationError::InvalidFingerprint { index: 0 }]
        );
    }

    #[test]
    fn a_fingerprint_of_the_wrong_type_names_the_line() {
        let text = "[[peers]]\nname = \"PC\"\nid = \"bbbb\"\nos = \"windows\"\nside = \"right\"\n\
                    fingerprint = 42\n";
        let error = parse(text).unwrap_err().to_string();
        assert!(error.contains("line 6"), "{error}");
    }

    #[test]
    fn known_keys_match_the_settings_types() {
        // Every optional setting is filled in, so the TOML form has every key.
        let config = Config {
            peers: vec![windows_pc()],
            transfer: TransferConfig {
                received_dir: Some(std::env::temp_dir()),
            },
            ..Config::default()
        };
        let table = Table::try_from(&config).unwrap();
        let mut from_types: Vec<(String, Vec<String>)> = table
            .iter()
            .map(|(section, value)| {
                let keys = match value {
                    Value::Table(table) => table.keys().cloned().collect(),
                    Value::Array(entries) => match entries.first() {
                        Some(Value::Table(table)) => table.keys().cloned().collect(),
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                (section.clone(), keys)
            })
            .collect();
        let mut listed: Vec<(String, Vec<String>)> = KNOWN_KEYS
            .iter()
            .map(|(section, keys)| {
                (
                    (*section).to_owned(),
                    keys.iter().map(|key| (*key).to_owned()).collect(),
                )
            })
            .collect();
        for list in [&mut from_types, &mut listed] {
            list.sort();
            for (_, keys) in list.iter_mut() {
                keys.sort();
            }
        }
        assert_eq!(listed, from_types);
    }

    #[test]
    fn saved_text_starts_with_the_header_and_reads_back() {
        let mut config = Config::first_run("Krish's MacBook").unwrap();
        config.peers.push(windows_pc());
        let text = config.to_toml().unwrap();
        assert!(text.starts_with("# Hop configuration."), "{text}");
        assert!(text.contains("[[peers]]\nname = \"Windows PC\""), "{text}");
        assert_eq!(parse(&text).unwrap(), (config, vec![]));
    }

    #[test]
    fn unset_options_are_left_out_of_the_text() {
        let text = Config::default().to_toml().unwrap();
        assert!(!text.contains("received_dir"), "{text}");
        assert!(!text.contains("peers"), "{text}");
    }

    #[test]
    fn byte_order_mark_and_windows_line_endings_are_fine() {
        // Windows Notepad can save files this way.
        let (config, warnings) = parse("\u{feff}[network]\r\nport = 50000\r\n").unwrap();
        assert_eq!(warnings, []);
        assert_eq!(config.network.port, 50000);
    }

    #[test]
    fn keys_are_quoted_only_when_needed() {
        assert_eq!(display_key("pointer_speed"), "pointer_speed");
        assert_eq!(display_key("edge-delay2"), "edge-delay2");
        assert_eq!(display_key("a.b"), "\"a.b\"");
        assert_eq!(display_key(""), "\"\"");
        assert_eq!(display_key("caf\u{e9}"), "\"caf\u{e9}\"");
    }

    fn non_blank() -> impl Strategy<Value = String> {
        any::<String>().prop_filter("not blank", |text| !text.trim().is_empty())
    }

    fn os() -> impl Strategy<Value = Os> {
        prop_oneof![Just(Os::MacOs), Just(Os::Windows), Just(Os::Linux)]
    }

    prop_compose! {
        fn peers()(
            sides in proptest::sample::subsequence(
                vec![Side::Left, Side::Right, Side::Above, Side::Below],
                0..=4,
            ),
            details in proptest::collection::vec(
                (
                    non_blank(),
                    non_blank(),
                    os(),
                    proptest::option::of(non_blank()),
                    proptest::option::of("[0-9a-f]{63}"),
                ),
                4,
            ),
        ) -> Vec<PeerConfig> {
            sides
                .into_iter()
                .zip(details)
                .enumerate()
                .map(|(index, (side, (name, id, os, address, key)))| {
                    // A paired peer's id is the start of its fingerprint. A
                    // first character per peer keeps the ids unique, and no
                    // other id is hex.
                    let fingerprint = key.map(|rest| format!("{index:x}{rest}"));
                    let id = match &fingerprint {
                        Some(fingerprint) => DeviceId::new(&fingerprint[..32]),
                        None => DeviceId::new(format!("peer{index}:{id}")),
                    };
                    PeerConfig {
                        name,
                        id,
                        os,
                        side,
                        address,
                        fingerprint,
                    }
                })
                .collect()
        }
    }

    prop_compose! {
        fn valid_config()(
            device in (non_blank(), non_blank()),
            peers in peers(),
            edge_delay_ms in 0u64..=2000,
            corner_dead_zone in 0.0f64..=200.0,
            pointer_speed in 0.1f64..=5.0,
            takeback_threshold in 0.0f64..=100.0,
            emergency_chord in proptest::collection::vec(non_blank(), 1..6),
            swap_cmd_ctrl in any::<bool>(),
            clipboard in (any::<bool>(), 1usize..=1_048_576),
            received_dir in proptest::option::of("[A-Za-z0-9 _.-]{1,12}"),
            port in 1024u16..=65535,
        ) -> Config {
            let (name, id) = device;
            let (enabled, max_text_bytes) = clipboard;
            Config {
                device: crate::DeviceConfig {
                    name,
                    id: DeviceId::new(format!("device:{id}")),
                },
                peers,
                input: crate::InputConfig {
                    edge_delay_ms,
                    corner_dead_zone,
                    pointer_speed,
                    takeback_threshold,
                    emergency_chord,
                },
                keymap: crate::KeymapConfig { swap_cmd_ctrl },
                clipboard: crate::ClipboardConfig {
                    enabled,
                    max_text_bytes,
                },
                transfer: TransferConfig {
                    received_dir: received_dir.map(|dir| std::env::temp_dir().join(dir)),
                },
                network: crate::NetworkConfig { port },
            }
        }
    }

    proptest! {
        #[test]
        fn valid_configs_survive_a_round_trip(config in valid_config()) {
            prop_assert_eq!(config.problems(), vec![]);
            let text = config.to_toml().unwrap();
            let (read, warnings) = parse(&text).unwrap();
            prop_assert_eq!(read, config);
            prop_assert_eq!(warnings, vec![]);
        }

        #[test]
        fn any_text_is_handled_without_panicking(text in any::<String>()) {
            if let Ok((config, _)) = parse(&text) {
                let _ = config.problems();
            }
        }

        #[test]
        fn config_like_text_is_handled_without_panicking(
            text in "((\\[(device|input|keymap|clipboard|transfer|network|extra)\\]|\\[\\[peers\\]\\]|\
                (name|id|os|side|address|fingerprint|port|pointer_speed|corner_dead_zone|\
                edge_delay_ms|emergency_chord|enabled|max_text_bytes|received_dir|other) = \
                (\"[a-z ]{0,6}\"|\"[0-9a-f]{64}\"|-?[0-9]{1,6}(\\.[0-9]{1,3})?|true|false|nan|inf|\
                \\[\\]|\\[\"[A-Za-z]{0,8}\"\\]))\n){0,16}"
        ) {
            if let Ok((config, _)) = parse(&text) {
                let _ = config.problems();
            }
        }
    }

    #[test]
    fn a_bare_path_is_not_a_file_name() {
        let error = write_atomically(Path::new("/"), b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let dot_dot: PathBuf = std::env::temp_dir().join("..");
        let error = write_atomically(&dot_dot, b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
