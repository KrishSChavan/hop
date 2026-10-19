//! Loading and saving real files through the public API.

use std::fs;
use std::path::{Path, PathBuf};

use hop_config::{
    CmdCtrl, Config, ConfigError, PeerConfig, Side, ValidationError, Warning,
    device_id_from_fingerprint,
};
use hop_types::{DeviceId, Os};

/// A fresh, empty folder for one test inside Cargo's scratch folder for
/// this package's tests.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("hop-config")
        .join(test);
    // Left over from an earlier run, if anything.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The fingerprint of the PC's key, as pairing writes it.
const PC_FINGERPRINT: &str = "c41d8e2b9a7f4e06b3d5a1f8e9c2b7d05a3f19c7e2b8d4a60f1e9c3b7d2a8e45";

/// A paired PC: its id is the start of its fingerprint.
fn windows_pc() -> PeerConfig {
    PeerConfig {
        name: "Windows PC".to_owned(),
        id: DeviceId::new("c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0"),
        os: Os::Windows,
        side: Side::Right,
        address: Some("192.168.1.42".to_owned()),
        fingerprint: Some(PC_FINGERPRINT.to_owned()),
    }
}

#[test]
fn a_missing_file_gives_the_defaults() {
    let dir = scratch("missing");
    let (config, warnings) = Config::load(dir.join("config.toml")).unwrap();
    assert_eq!(config, Config::default());
    assert_eq!(warnings, []);
}

#[test]
fn defaults_round_trip() {
    let path = scratch("defaults").join("config.toml");
    Config::default().save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), (Config::default(), vec![]));
}

#[test]
fn a_first_run_config_round_trips() {
    let path = scratch("first-run").join("config.toml");
    let config = Config::first_run("Krish's MacBook").unwrap();
    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), (config, vec![]));
}

#[test]
fn every_setting_round_trips() {
    let dir = scratch("everything");
    let path = dir.join("config.toml");
    let mut config = Config::first_run("Krish's \"MacBook\" \u{1F4BB}").unwrap();
    config.peers.push(windows_pc());
    config.peers.push(PeerConfig {
        name: "Old Mac".to_owned(),
        id: DeviceId::new("0123"),
        os: Os::MacOs,
        side: Side::Above,
        address: None,
        fingerprint: None,
    });
    config.input.edge_delay_ms = 150;
    config.input.corner_dead_zone = 12.5;
    config.input.pointer_speed = 0.1;
    config.input.takeback_threshold = 0.0;
    config.input.emergency_chord = vec!["MetaLeft".to_owned(), "KeyH".to_owned()];
    config.keymap.cmd_ctrl = CmdCtrl::Swap;
    config.clipboard.enabled = false;
    config.clipboard.max_text_bytes = 4096;
    config.transfer.enabled = false;
    config.transfer.received_dir = Some(dir.join("Hop Received"));
    config.network.port = 50000;

    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), (config, vec![]));
}

/// A file saved by M0, before pairing existed: peers without fingerprints
/// and a random device id.
const M0_FILE: &str = "\
[device]
name = \"Krish's MacBook\"
id = \"5f0c9a1e7b3d4c2a8e6f1b9d0a7c3e52\"

[[peers]]
name = \"Windows PC\"
id = \"0c41d8e2b9a7f4e06b3d5a1f8e9c2b7d\"
os = \"windows\"
side = \"right\"
";

#[test]
fn an_m0_file_still_loads_with_its_peers_unpaired() {
    let path = scratch("m0").join("config.toml");
    fs::write(&path, M0_FILE).unwrap();
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, []);
    assert_eq!(config.peers[0].fingerprint, None);
    assert_eq!(config.peers[0].pinned_fingerprint(), None);
}

#[test]
fn the_first_m2_start_replaces_the_random_id_with_the_one_from_the_key() {
    let path = scratch("m2-id").join("config.toml");
    fs::write(&path, M0_FILE).unwrap();

    // What the agent does at every start, with this Mac's own fingerprint.
    let mac_fingerprint = "9e1f6a3c5b7d2e4f8a0c1b3d5e7f9a2b4c6d8e0f1a3b5c7d9e2f4a6b8c0d1e3f";
    let id = device_id_from_fingerprint(mac_fingerprint).unwrap();
    let (mut config, _) = Config::load(&path).unwrap();
    assert!(config.set_device_id(id.clone()));
    config.save(&path).unwrap();

    let (mut config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, []);
    assert_eq!(
        config.device.id,
        DeviceId::new("9e1f6a3c5b7d2e4f8a0c1b3d5e7f9a2b")
    );
    assert_eq!(config.device.name, "Krish's MacBook");
    assert_eq!(config.peers.len(), 1, "the peers are kept");
    // The next start finds the id already right and has nothing to save.
    assert!(!config.set_device_id(id));
}

/// A file saved before the Shortcuts setting existed. Every such file says
/// `swap_cmd_ctrl = true`: the app never offered the choice.
const SWAP_FILE: &str = "\
[device]
name = \"Krish's MacBook\"
id = \"5f0c9a1e7b3d4c2a8e6f1b9d0a7c3e52\"

[keymap]
swap_cmd_ctrl = true

[network]
port = 50000
";

#[test]
fn the_retired_swap_setting_is_ignored_and_left_out_of_the_next_save() {
    let path = scratch("retired-swap").join("config.toml");
    fs::write(&path, SWAP_FILE).unwrap();
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, [], "no warning about a key Hop wrote itself");
    assert_eq!(
        config.keymap.cmd_ctrl,
        CmdCtrl::Keep,
        "the old value is ignored"
    );
    assert_eq!(config.network.port, 50000, "the rest still loads");

    config.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[keymap]\ncmd_ctrl = \"keep\"\n"), "{text}");
    assert!(!text.contains("swap_cmd_ctrl"), "{text}");
    assert_eq!(Config::load(&path).unwrap(), (config, vec![]));
}

#[test]
fn the_shortcuts_setting_round_trips() {
    let path = scratch("cmd-ctrl").join("config.toml");
    fs::write(&path, "[keymap]\ncmd_ctrl = \"swap\"\n").unwrap();
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, []);
    assert_eq!(config.keymap.cmd_ctrl, CmdCtrl::Swap);

    config.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[keymap]\ncmd_ctrl = \"swap\"\n"), "{text}");
    assert_eq!(Config::load(&path).unwrap(), (config, vec![]));
}

#[test]
fn receiving_files_can_be_turned_off() {
    let path = scratch("transfer-off").join("config.toml");
    let mut config = Config::default();
    config.transfer.enabled = false;
    config.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[transfer]\nenabled = false\n"), "{text}");
    assert_eq!(Config::load(&path).unwrap(), (config, vec![]));
}

#[test]
fn a_malformed_fingerprint_fails_to_load_with_the_path() {
    let path = scratch("bad-fingerprint").join("config.toml");
    let text = format!("{M0_FILE}fingerprint = \"not a fingerprint\"\n");
    fs::write(&path, text).unwrap();
    let error = Config::load(&path).unwrap_err();
    let ConfigError::Invalid {
        path: Some(reported),
        errors,
    } = &error
    else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(reported, &path);
    assert_eq!(errors, &[ValidationError::InvalidFingerprint { index: 0 }]);
    // The bad value is not repeated: it could be anything pasted by mistake.
    assert!(!error.to_string().contains("not a fingerprint"), "{error}");
}

#[test]
fn a_peer_whose_id_does_not_match_its_fingerprint_loads_with_a_warning() {
    let path = scratch("mismatch").join("config.toml");
    // The M0 peer id is not the start of this fingerprint.
    let text = format!("{M0_FILE}fingerprint = \"{PC_FINGERPRINT}\"\n");
    fs::write(&path, text).unwrap();
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, [Warning::PeerIdMismatch { index: 0 }]);
    assert_eq!(config.peers[0].pinned_fingerprint(), None);
}

#[test]
fn save_creates_folders_and_leaves_no_temp_files() {
    let dir = scratch("atomic");
    let folder = dir.join("Application Support").join("Hop");
    let path = folder.join("config.toml");

    let mut config = Config::first_run("Krish's MacBook").unwrap();
    config.save(&path).unwrap();
    assert_eq!(names_in(&folder), ["config.toml"]);

    // Saving again replaces the file in place.
    config.network.port = 50000;
    config.save(&path).unwrap();
    assert_eq!(names_in(&folder), ["config.toml"]);
    assert_eq!(Config::load(&path).unwrap().0.network.port, 50000);
}

#[test]
fn a_failed_save_leaves_no_temp_files() {
    let dir = scratch("failed-save");
    // A folder where the file should go makes the final rename fail.
    let path = dir.join("config.toml");
    fs::create_dir(&path).unwrap();

    let error = Config::default().save(&path).unwrap_err();
    assert!(
        matches!(&error, ConfigError::Write { path: p, .. } if p == &path),
        "{error:?}"
    );
    assert_eq!(names_in(&dir), ["config.toml"]);
    assert!(path.is_dir());
}

#[test]
fn save_refuses_an_invalid_config_and_writes_nothing() {
    let dir = scratch("save-invalid");
    let path = dir.join("config.toml");
    let mut config = Config::default();
    config.network.port = 80;

    let error = config.save(&path).unwrap_err();
    let ConfigError::Invalid {
        path: Some(reported),
        errors,
    } = &error
    else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(reported, &path);
    assert_eq!(
        errors,
        &[ValidationError::OutOfRange {
            key: "network.port",
            value: "80".to_owned(),
            min: "1024".to_owned(),
            max: "65535".to_owned(),
        }]
    );
    assert_eq!(names_in(&dir), Vec::<String>::new());
}

#[test]
fn a_partial_file_takes_the_other_defaults() {
    let path = scratch("partial").join("config.toml");
    fs::write(&path, "[network]\nport = 50000\n\n[keymap]\n").unwrap();
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(warnings, []);
    let mut expected = Config::default();
    expected.network.port = 50000;
    assert_eq!(config, expected);
}

#[test]
fn unknown_settings_load_with_warnings() {
    let path = scratch("unknown").join("config.toml");
    fs::write(
        &path,
        "[input]\npointer_speed = 2.0\npointer_sped = 3.0\n\n[telemetry]\nenabled = true\n",
    )
    .unwrap();
    let (config, warnings) = Config::load(&path).unwrap();
    assert_eq!(config.input.pointer_speed, 2.0);
    assert_eq!(
        warnings,
        [
            Warning::UnknownKey {
                key: "input.pointer_sped".to_owned()
            },
            Warning::UnknownKey {
                key: "telemetry".to_owned()
            },
        ]
    );
}

#[test]
fn rule_breaking_files_fail_with_every_problem_and_the_path() {
    let path = scratch("load-invalid").join("config.toml");
    let text = "\
[device]
name = \"Mac\"
id = \"aaaa\"

[[peers]]
name = \"PC\"
id = \"aaaa\"
os = \"windows\"
side = \"right\"

[[peers]]
name = \"Laptop\"
id = \"cccc\"
os = \"windows\"
side = \"right\"

[input]
pointer_speed = 9
";
    fs::write(&path, text).unwrap();

    let error = Config::load(&path).unwrap_err();
    let ConfigError::Invalid {
        path: Some(reported),
        errors,
    } = &error
    else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(reported, &path);
    assert_eq!(
        errors,
        &[
            ValidationError::PeerIsThisDevice {
                index: 0,
                id: DeviceId::new("aaaa"),
            },
            ValidationError::SideTaken {
                side: Side::Right,
                first: 0,
                second: 1,
            },
            ValidationError::OutOfRange {
                key: "input.pointer_speed",
                value: "9".to_owned(),
                min: "0.1".to_owned(),
                max: "5".to_owned(),
            },
        ]
    );
    let message = error.to_string();
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("3 problems"), "{message}");
}

#[test]
fn bad_toml_fails_with_the_line() {
    let path = scratch("parse").join("config.toml");
    fs::write(&path, "[input]\npointer_speed = \"fast\"\n").unwrap();
    let error = Config::load(&path).unwrap_err();
    let ConfigError::Parse {
        path: reported,
        source,
    } = &error
    else {
        panic!("expected Parse, got {error:?}");
    };
    assert_eq!(reported, &path);
    let detail = source.to_string();
    assert!(detail.contains("line 2"), "{detail}");
    assert!(
        detail.contains("expected a number from 0.1 to 5"),
        "{detail}"
    );
}

#[test]
fn an_unreadable_path_fails_to_load() {
    let dir = scratch("unreadable");
    // A folder is not a file, and is not missing either.
    let error = Config::load(&dir).unwrap_err();
    assert!(
        matches!(&error, ConfigError::Read { path, .. } if path == &dir),
        "{error:?}"
    );
}
