//! Reading and changing the config file, for the app: where the other
//! computer sits, its address, and forgetting it.
//!
//! Every change re-reads the file first, so a newer version (another
//! pairing, an edit by hand) is never overwritten; then the config's rules
//! are checked, and the file is saved atomically. A running agent picks a
//! change up when it starts again: the app restarts sharing after one.
//!
//! The `_at` versions take the files to use, for tests; the others use the
//! usual locations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hop_config::{Config, PeerConfig, Side};
use hop_types::{DeviceId, Os};

use crate::startup::{self, Startup};

/// This computer and the computers in its config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overview {
    /// This computer's name on the network (`device.name`).
    pub name: String,
    /// This computer's OS.
    pub os: Os,
    /// Where the config file lives.
    pub config_path: PathBuf,
    /// The other computers, in the file's order.
    pub peers: Vec<PeerView>,
    /// Sentences worth telling the user from loading: a new key, the name
    /// this computer got on a first run, config warnings. They name files
    /// and settings, never secrets.
    pub notes: Vec<String>,
}

/// One other computer, as the config has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerView {
    /// Its id: what the changes below take.
    pub id: DeviceId,
    /// Its name.
    pub name: String,
    /// Its OS.
    pub os: Os,
    /// Where it sits: the side of this computer's screens that leads to it.
    pub side: Side,
    /// A fixed address for it, if one was set.
    pub address: Option<String>,
    /// Whether it is paired: Hop has its key and can connect to it.
    pub paired: bool,
}

/// Loads the config and this computer's key from their usual places,
/// making both on a first run (as `hop run` and `hop pair` do), and
/// describes them.
///
/// # Errors
///
/// The config cannot be read, parsed or saved, or the key file exists but
/// cannot be read (Hop never replaces it: that would unpair every
/// computer).
pub fn overview() -> Result<Overview> {
    Ok(describe(startup::load()?))
}

/// [`overview`] with the config file and the key file given.
///
/// # Errors
///
/// As [`overview`].
pub fn overview_at(config_path: &Path, identity_path: &Path) -> Result<Overview> {
    Ok(describe(startup::load_from(
        config_path.to_owned(),
        identity_path,
    )?))
}

fn describe(startup: Startup) -> Overview {
    let peers = startup
        .config
        .peers
        .iter()
        .map(|peer| PeerView {
            id: peer.id.clone(),
            name: peer.name.clone(),
            os: peer.os,
            side: peer.side,
            address: peer.address.clone(),
            paired: peer.pinned_fingerprint().is_some(),
        })
        .collect();
    Overview {
        name: startup.config.device.name,
        os: Os::current(),
        config_path: startup.config_path,
        peers,
        notes: startup.notes,
    }
}

/// Puts the computer `id` on `side` of this computer's screens.
///
/// # Errors
///
/// Another computer already sits there (move it first), `id` is no longer
/// in the config, or the file cannot be read or saved.
pub fn set_side(id: &DeviceId, side: Side) -> Result<()> {
    set_side_at(&config_path()?, id, side)
}

/// [`set_side`] in the config at `path`.
///
/// # Errors
///
/// As [`set_side`].
pub fn set_side_at(path: &Path, id: &DeviceId, side: Side) -> Result<()> {
    change(path, |config| {
        if let Some(other) = config
            .peers
            .iter()
            .find(|peer| peer.side == side && peer.id != *id)
        {
            bail!(
                "{} already sits {} this computer; move it to another side first",
                other.name,
                placement(side)
            );
        }
        let peer = peer_mut(config, id, path)?;
        if peer.side == side {
            return Ok(false);
        }
        peer.side = side;
        Ok(true)
    })
}

/// Sets the fixed address of the computer `id`: an IPv4 address or a host
/// name, with an optional `:port`, for when it is not found on the network
/// by itself. Spaces around it are dropped; `None` or an empty address
/// removes it.
///
/// # Errors
///
/// The address cannot be an address (the message says why without
/// repeating it), `id` is no longer in the config, or the file cannot be
/// read or saved.
pub fn set_address(id: &DeviceId, address: Option<&str>) -> Result<()> {
    set_address_at(&config_path()?, id, address)
}

/// [`set_address`] in the config at `path`.
///
/// # Errors
///
/// As [`set_address`].
pub fn set_address_at(path: &Path, id: &DeviceId, address: Option<&str>) -> Result<()> {
    let address = address.map(str::trim).filter(|text| !text.is_empty());
    if let Some(text) = address {
        check_address(text)?;
    }
    change(path, |config| {
        let peer = peer_mut(config, id, path)?;
        let address = address.map(str::to_owned);
        if peer.address == address {
            return Ok(false);
        }
        peer.address = address;
        Ok(true)
    })
}

/// Forgets the computer `id`: its entry and its key go, so it can no longer
/// connect, and the two computers must pair again to share. Forgetting a
/// computer the config no longer lists does nothing.
///
/// # Errors
///
/// The file cannot be read or saved.
pub fn forget(id: &DeviceId) -> Result<()> {
    forget_at(&config_path()?, id)
}

/// [`forget`] in the config at `path`.
///
/// # Errors
///
/// As [`forget`].
pub fn forget_at(path: &Path, id: &DeviceId) -> Result<()> {
    change(path, |config| {
        let before = config.peers.len();
        config.peers.retain(|peer| peer.id != *id);
        Ok(config.peers.len() != before)
    })
}

/// The side this computer is on as seen from a computer on `side`: what
/// to choose for this computer over there.
pub fn opposite(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
        Side::Above => Side::Below,
        Side::Below => Side::Above,
    }
}

fn config_path() -> Result<PathBuf> {
    Config::default_path().context("could not work out where the configuration lives")
}

/// Re-reads the config at `path`, lets `edit` change it, and saves it if
/// `edit` says it changed something (saving checks the rules first).
fn change(path: &Path, edit: impl FnOnce(&mut Config) -> Result<bool>) -> Result<()> {
    let (mut config, _) =
        Config::load(path).with_context(|| format!("could not load {}", path.display()))?;
    if edit(&mut config)? {
        config
            .save(path)
            .with_context(|| format!("could not save {}", path.display()))?;
    }
    Ok(())
}

fn peer_mut<'a>(config: &'a mut Config, id: &DeviceId, path: &Path) -> Result<&'a mut PeerConfig> {
    config
        .peers
        .iter_mut()
        .find(|peer| peer.id == *id)
        .with_context(|| {
            format!(
                "{} no longer lists that computer; it may have been forgotten or paired again",
                path.display()
            )
        })
}

/// Where a computer on `side` is, for sentences.
fn placement(side: Side) -> &'static str {
    match side {
        Side::Left => "to the left of",
        Side::Right => "to the right of",
        Side::Above => "above",
        Side::Below => "below",
    }
}

/// Checks `text` the way the network layer reads an address: an IPv4
/// address or a host name, with an optional `:port`. The messages never
/// repeat the text: whatever was typed there by mistake should not end up
/// in a log.
fn check_address(text: &str) -> Result<()> {
    const EXAMPLE: &str = "use an IP address such as 192.168.1.20 or a name such as krish-pc.local, optionally \
         followed by :port";
    if text.starts_with('[') || text.matches(':').count() > 1 {
        bail!("IPv6 addresses are not supported yet; use an IPv4 address such as 192.168.1.20");
    }
    let host = match text.split_once(':') {
        Some((host, port)) => {
            if !port.parse::<u16>().is_ok_and(|port| port != 0) {
                bail!("the port after the colon must be a number from 1 to 65535");
            }
            host
        }
        None => text,
    };
    if host.is_empty() {
        bail!("the address has a port but no computer; {EXAMPLE}");
    }
    let name_like = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !name_like || host.starts_with(['.', '-']) || host.ends_with('-') {
        bail!("that is not an address; {EXAMPLE}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A fresh folder for one test's files; never the real ones.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hop-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn peer(name: &str, hex: char, side: Side, paired: bool) -> PeerConfig {
        let fingerprint: String = std::iter::repeat_n(hex, 64).collect();
        PeerConfig {
            name: name.to_owned(),
            id: DeviceId::new(&fingerprint[..32]),
            os: Os::Windows,
            side,
            address: None,
            fingerprint: paired.then_some(fingerprint),
        }
    }

    fn id(hex: char) -> DeviceId {
        DeviceId::new(std::iter::repeat_n(hex, 32).collect::<String>())
    }

    /// A config with a paired PC on the right and an unpaired one above.
    fn two_peers(dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        let mut config = Config::first_run("Mac").expect("config");
        config.device.id = id('f');
        config.peers.push(peer("PC", 'a', Side::Right, true));
        config.peers.push(peer("Old PC", 'b', Side::Above, false));
        config.save(&path).expect("save");
        path
    }

    fn load(path: &Path) -> Config {
        Config::load(path).expect("load").0
    }

    #[test]
    fn a_first_run_overview_makes_the_key_and_has_no_peers() {
        let dir = scratch("first");
        let overview =
            overview_at(&dir.join("config.toml"), &dir.join("identity.pem")).expect("overview");
        assert_eq!(overview.name, startup::default_device_name(Os::current()));
        assert_eq!(overview.os, Os::current());
        assert_eq!(overview.config_path, dir.join("config.toml"));
        assert!(overview.peers.is_empty());
        assert!(!overview.notes.is_empty(), "a first run says what it made");
        assert!(dir.join("identity.pem").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_overview_lists_each_peer_and_whether_it_is_paired() {
        let dir = scratch("overview");
        let path = two_peers(&dir);
        let overview = overview_at(&path, &dir.join("identity.pem")).expect("overview");
        assert_eq!(overview.name, "Mac");
        assert_eq!(
            overview.peers,
            [
                PeerView {
                    id: id('a'),
                    name: "PC".to_owned(),
                    os: Os::Windows,
                    side: Side::Right,
                    address: None,
                    paired: true,
                },
                PeerView {
                    id: id('b'),
                    name: "Old PC".to_owned(),
                    os: Os::Windows,
                    side: Side::Above,
                    address: None,
                    paired: false,
                },
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_side_changes_unless_another_computer_sits_there() {
        let dir = scratch("side");
        let path = two_peers(&dir);
        set_side_at(&path, &id('a'), Side::Left).expect("left is free");
        assert_eq!(load(&path).peers[0].side, Side::Left);

        let taken = set_side_at(&path, &id('a'), Side::Above).expect_err("Old PC is above");
        assert!(
            taken.to_string().contains("Old PC already sits above"),
            "{taken}"
        );
        assert_eq!(load(&path).peers[0].side, Side::Left, "nothing changed");

        set_side_at(&path, &id('a'), Side::Left).expect("the same side again is fine");
        let gone = set_side_at(&path, &id('c'), Side::Below).expect_err("no such computer");
        assert!(gone.to_string().contains("no longer lists"), "{gone}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_change_rereads_the_file_first() {
        let dir = scratch("reread");
        let path = two_peers(&dir);
        // Something else changed the file after the app last read it.
        let mut newer = load(&path);
        newer.input.edge_delay_ms = 150;
        newer.save(&path).expect("save");
        set_side_at(&path, &id('a'), Side::Below).expect("side");
        set_address_at(&path, &id('a'), Some("192.168.1.20")).expect("address");
        let config = load(&path);
        assert_eq!(config.input.edge_delay_ms, 150, "the newer file was kept");
        assert_eq!(config.peers[0].side, Side::Below);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_address_is_trimmed_checked_and_removed_when_empty() {
        let dir = scratch("address");
        let path = two_peers(&dir);
        set_address_at(&path, &id('a'), Some("  krish-pc.local:47800 ")).expect("address");
        assert_eq!(
            load(&path).peers[0].address.as_deref(),
            Some("krish-pc.local:47800")
        );
        set_address_at(&path, &id('a'), Some("192.168.1.20")).expect("address");
        assert_eq!(
            load(&path).peers[0].address.as_deref(),
            Some("192.168.1.20")
        );

        for wrong in [
            "fe80::1",
            "[::1]:47800",
            "192.168.1.20:0",
            "192.168.1.20:port",
            ":47800",
            "482 913",
            "http://pc",
            ".mybox",
            "mybox-",
        ] {
            let error = set_address_at(&path, &id('a'), Some(wrong)).expect_err(wrong);
            let text = format!("{error:#}");
            assert!(
                !text.contains(wrong),
                "the message repeats {wrong:?}: {text}"
            );
            assert_eq!(
                load(&path).peers[0].address.as_deref(),
                Some("192.168.1.20"),
                "{wrong:?} changed nothing"
            );
        }

        set_address_at(&path, &id('a'), Some("   ")).expect("blank");
        assert_eq!(load(&path).peers[0].address, None);
        set_address_at(&path, &id('a'), Some("pc.local")).expect("address");
        set_address_at(&path, &id('a'), None).expect("none");
        assert_eq!(load(&path).peers[0].address, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_removes_the_computer_and_its_key() {
        let dir = scratch("forget");
        let path = two_peers(&dir);
        forget_at(&path, &id('a')).expect("forget");
        let config = load(&path);
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].name, "Old PC");
        forget_at(&path, &id('a')).expect("forgetting it again does nothing");

        // Without a file there is nothing to forget, and no file is made.
        let missing = dir.join("missing.toml");
        forget_at(&missing, &id('a')).expect("nothing to forget");
        assert!(!missing.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_opposite_side_is_where_this_computer_sits_over_there() {
        for (side, other) in [
            (Side::Left, Side::Right),
            (Side::Right, Side::Left),
            (Side::Above, Side::Below),
            (Side::Below, Side::Above),
        ] {
            assert_eq!(opposite(side), other);
            assert_eq!(opposite(other), side);
        }
    }
}
