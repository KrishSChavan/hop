//! What `hop run` and `hop pair` both do first: load the config, load (or,
//! on a first run, create) this computer's key, and put the device id that
//! belongs to the key in the config (docs/M2-DESIGN.md, "Identity").

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hop_config::{Config, Warning};
use hop_net::Identity;
use hop_types::Os;

use crate::paths;

/// The loaded config and key.
pub(crate) struct Startup {
    /// Where the config file lives.
    pub(crate) config_path: PathBuf,
    /// The config, with `device.id` already matching the key.
    pub(crate) config: Config,
    /// This computer's key.
    pub(crate) identity: Arc<Identity>,
    /// Sentences worth telling the user (a new key, a corrected id, config
    /// warnings). They name files and settings, never secrets.
    pub(crate) notes: Vec<String>,
}

/// Loads the config and the key, creating the key on a first run, and
/// saves the config if `device.id` (or, on a first run, `device.name`)
/// had to be set.
///
/// # Errors
///
/// The config cannot be read, parsed or saved, or a key file exists but
/// cannot be read: Hop never replaces an unreadable key, since that would
/// silently unpair every computer.
pub(crate) fn load() -> Result<Startup> {
    let config_path =
        Config::default_path().context("could not work out where the configuration lives")?;
    let identity_path =
        paths::identity_path().context("could not work out where this computer's key lives")?;
    load_from(config_path, &identity_path)
}

/// [`load`] with the two files given.
pub(crate) fn load_from(config_path: PathBuf, identity_path: &Path) -> Result<Startup> {
    let (mut config, warnings) = Config::load(&config_path)
        .with_context(|| format!("could not load {}", config_path.display()))?;
    let mut notes: Vec<String> = warnings.iter().map(warning_note).collect();

    let loaded = Identity::load_or_create(identity_path).with_context(|| {
        format!(
            "could not load this computer's key from {}. If it is damaged, delete it to make a \
             new one, then pair the two computers again",
            identity_path.display()
        )
    })?;
    if loaded.created {
        notes.push(format!(
            "Made a new key for this computer in {}.",
            identity_path.display()
        ));
        if config.peers.iter().any(|peer| peer.fingerprint.is_some()) {
            notes.push(
                "This computer has a new key, so the computers it was paired with must pair \
                 with it again (hop pair)."
                    .to_owned(),
            );
        }
    }
    if loaded.tightened {
        notes.push(format!(
            "{} could be read by other accounts on this computer; Hop made it private again. \
             To be safe, pair the two computers again.",
            identity_path.display()
        ));
    }
    let identity = Arc::new(loaded.identity);

    let mut changed = false;
    if !config.device.has_id() {
        // A first run: the random id first_run makes is replaced right
        // below by the one that belongs to the key.
        let name = if config.device.name.trim().is_empty() {
            default_device_name(Os::current()).to_owned()
        } else {
            config.device.name.clone()
        };
        config.device = Config::first_run(name)?.device;
        notes.push(format!(
            "This computer is called \"{}\" on the network. To rename it, change device.name \
             in {}.",
            config.device.name,
            config_path.display()
        ));
        changed = true;
    }
    if config.set_device_id(identity.device_id()) {
        changed = true;
    }
    if changed {
        config
            .save(&config_path)
            .with_context(|| format!("could not save {}", config_path.display()))?;
    }
    Ok(Startup {
        config_path,
        config,
        identity,
        notes,
    })
}

/// The name a computer gets on its first run. Plain on purpose: the name
/// is announced on every network the computer joins, so it should not carry
/// the user's name unless they put it there.
pub(crate) fn default_device_name(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Mac",
        Os::Windows => "Windows PC",
        Os::Linux => "Linux PC",
    }
}

fn warning_note(warning: &Warning) -> String {
    format!("Config: {warning}.")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hop_types::DeviceId;

    use super::*;

    /// A fresh folder for one test's files; never the real ones.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hop-startup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_first_run_makes_a_key_and_puts_its_id_in_the_config() {
        let dir = scratch("first");
        let config_path = dir.join("config.toml");
        let key_path = dir.join("identity.pem");
        let first = load_from(config_path.clone(), &key_path).expect("first run");
        assert!(key_path.exists());
        assert_eq!(first.config.device.id, first.identity.device_id());
        assert_eq!(first.config.device.name, default_device_name(Os::current()));
        let (saved, _) = Config::load(&config_path).expect("saved");
        assert_eq!(saved.device.id, first.identity.device_id());

        // The next start finds the same key and changes nothing.
        let before = fs::read_to_string(&config_path).expect("config");
        let second = load_from(config_path.clone(), &key_path).expect("second run");
        assert_eq!(second.identity.fingerprint(), first.identity.fingerprint());
        assert_eq!(fs::read_to_string(&config_path).expect("config"), before);
        assert!(second.notes.is_empty(), "{:?}", second.notes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_random_id_is_replaced_by_the_keys_and_the_name_is_kept() {
        let dir = scratch("m0");
        let config_path = dir.join("config.toml");
        let mut m0 = Config::first_run("Krish's MacBook").expect("config");
        m0.device.id = DeviceId::new("0123456789abcdef0123456789abcdef");
        m0.save(&config_path).expect("save");
        let started = load_from(config_path.clone(), &dir.join("identity.pem")).expect("start");
        assert_eq!(started.config.device.name, "Krish's MacBook");
        assert_eq!(started.config.device.id, started.identity.device_id());
        let (saved, _) = Config::load(&config_path).expect("saved");
        assert_eq!(saved.device.id, started.identity.device_id());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_key_is_an_error_and_is_never_replaced() {
        let dir = scratch("broken");
        fs::create_dir_all(&dir).expect("dir");
        let key_path = dir.join("identity.pem");
        fs::write(&key_path, "not a key").expect("write");
        assert!(load_from(dir.join("config.toml"), &key_path).is_err());
        assert_eq!(fs::read_to_string(&key_path).expect("key"), "not a key");
        let _ = fs::remove_dir_all(&dir);
    }
}
