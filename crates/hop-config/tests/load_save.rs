//! Loading and saving real files through the public API.

use std::fs;
use std::path::{Path, PathBuf};

use hop_config::{Config, ConfigError, PeerConfig, Side, ValidationError, Warning};
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

fn windows_pc() -> PeerConfig {
    PeerConfig {
        name: "Windows PC".to_owned(),
        id: DeviceId::new("c41d8e2b9a7f4e06b3d5a1f8e9c2b7d0"),
        os: Os::Windows,
        side: Side::Right,
        address: Some("192.168.1.42".to_owned()),
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
    });
    config.input.edge_delay_ms = 150;
    config.input.corner_dead_zone = 12.5;
    config.input.pointer_speed = 0.1;
    config.input.takeback_threshold = 0.0;
    config.input.emergency_chord = vec!["MetaLeft".to_owned(), "KeyH".to_owned()];
    config.keymap.swap_cmd_ctrl = false;
    config.clipboard.enabled = false;
    config.clipboard.max_text_bytes = 4096;
    config.transfer.received_dir = Some(dir.join("Hop Received"));
    config.network.port = 50000;

    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), (config, vec![]));
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
