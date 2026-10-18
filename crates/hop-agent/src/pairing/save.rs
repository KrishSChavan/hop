//! Saving a pairing in the config (docs/M2-DESIGN.md, "Saving"): which
//! `[[peers]]` entry the other computer goes into, and on which side.

use std::path::{Path, PathBuf};

use hop_config::{Config, ConfigError, PeerConfig, Side};
use hop_net::PairedPeer;
use hop_types::{DeviceId, Os};

use crate::startup::default_device_name;

/// Why the pairing could not be saved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SaveError {
    /// The config could not be read or written.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Every side already has a computer.
    #[error(
        "every side of this computer already has a computer on it; remove one from {} first",
        .0.display()
    )]
    NoFreeSide(PathBuf),
}

/// What pairing saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The other computer's name, as it gave it (or a plain one, such as
    /// "Windows PC", if it gave none).
    pub name: String,
    /// Its OS.
    pub os: Os,
    /// Where it sits: the side of this computer's screens that leads to it.
    pub side: Side,
    /// Which entry it went into.
    pub replaced: Replaced,
    /// The config file it was saved in.
    pub path: PathBuf,
}

/// Which `[[peers]]` entry a pairing went into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replaced {
    /// None: a new entry was added.
    Nothing,
    /// The entry of the same computer (same key), or one that was never
    /// paired.
    Entry,
    /// The entry of the one computer with the same OS that was paired with
    /// a different key: the same computer with a new key. The old key, of
    /// the computer named here, can no longer connect.
    OldKey(String),
}

/// Saves `peer` in the config at `path`, re-reading the file first so a
/// newer version is never overwritten.
pub(crate) fn save_peer(
    path: &Path,
    own_id: &DeviceId,
    peer: &PairedPeer,
) -> Result<Saved, SaveError> {
    let (mut config, _) = Config::load(path)?;
    // The file was set up at startup; this only matters if it was removed
    // in the meantime.
    if !config.device.has_id() {
        config.device = Config::first_run(default_device_name(Os::current()))?.device;
    }
    config.set_device_id(own_id.clone());
    let (side, replaced) = add_peer(&mut config, peer, Os::current())
        .ok_or_else(|| SaveError::NoFreeSide(path.to_owned()))?;
    config.save(path)?;
    Ok(Saved {
        name: display_name(peer),
        os: peer.os,
        side,
        replaced,
        path: path.to_owned(),
    })
}

/// Puts `peer` in `config.peers` (docs/M2-DESIGN.md, "Saving"): it replaces
/// the entry with the same id, or else an unpaired entry with the same OS,
/// or else the only paired entry with the same OS, keeping that entry's
/// side and address. Otherwise it adds an entry on the first free side,
/// starting with the natural one: a Mac puts a PC on its right, and a PC
/// puts a Mac on its left. Returns the side and which entry it used; `None`
/// when every side is taken.
///
/// The third rule is for a computer that got a new key (its key file was
/// lost or deleted) and was paired again, as Hop tells the user to do.
/// Adding it as a second computer would leave the old pin first in the
/// list, where `hop run` (which shares with one computer for now) would
/// keep using it and never connect, and the old key would stay trusted.
pub(crate) fn add_peer(
    config: &mut Config,
    peer: &PairedPeer,
    local_os: Os,
) -> Option<(Side, Replaced)> {
    let name = display_name(peer);
    let fingerprint = peer.fingerprint.to_hex();
    let same_id = config.peers.iter().position(|entry| entry.id == peer.id);
    let unpaired = || {
        config
            .peers
            .iter()
            .position(|entry| entry.fingerprint.is_none() && entry.os == peer.os)
    };
    let existing = match same_id.or_else(unpaired) {
        Some(index) => Some((index, Replaced::Entry)),
        None => only_paired_with_os(config, peer.os).map(|index| {
            let old = config
                .peers
                .get(index)
                .map(|entry| entry.name.clone())
                .unwrap_or_default();
            (index, Replaced::OldKey(old))
        }),
    };
    if let Some((index, replaced)) = existing
        && let Some(entry) = config.peers.get_mut(index)
    {
        entry.name = name;
        entry.id = peer.id.clone();
        entry.os = peer.os;
        entry.fingerprint = Some(fingerprint);
        return Some((entry.side, replaced));
    }
    let side = preferred_sides(local_os, peer.os)
        .into_iter()
        .find(|side| config.peers.iter().all(|entry| entry.side != *side))?;
    config.peers.push(PeerConfig {
        name,
        id: peer.id.clone(),
        os: peer.os,
        side,
        address: None,
        fingerprint: Some(fingerprint),
    });
    Some((side, Replaced::Nothing))
}

/// The computer whose pairing saving `peer` would replace because it used
/// another key (see [`add_peer`]), as the config is now.
pub(crate) fn replaced_old_key(path: &Path, peer: &PairedPeer) -> Option<String> {
    let (mut config, _) = Config::load(path).ok()?;
    match add_peer(&mut config, peer, Os::current())? {
        (_, Replaced::OldKey(old)) => Some(old),
        _ => None,
    }
}

/// The position of the one paired entry running `os`, if exactly one is.
/// With two or more, Hop cannot tell which computer got a new key, so it
/// replaces none of them.
fn only_paired_with_os(config: &Config, os: Os) -> Option<usize> {
    let mut paired = config
        .peers
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.fingerprint.is_some() && entry.os == os)
        .map(|(index, _)| index);
    let first = paired.next()?;
    paired.next().is_none().then_some(first)
}

/// The sides to try for a new peer, best first.
fn preferred_sides(local_os: Os, peer_os: Os) -> [Side; 4] {
    if local_os != Os::MacOs && peer_os == Os::MacOs {
        [Side::Left, Side::Right, Side::Above, Side::Below]
    } else {
        [Side::Right, Side::Left, Side::Above, Side::Below]
    }
}

/// The peer's name, or a plain one if it sent none.
fn display_name(peer: &PairedPeer) -> String {
    if peer.name.trim().is_empty() {
        default_device_name(peer.os).to_owned()
    } else {
        peer.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use hop_config::Config;
    use hop_net::Fingerprint;

    use super::*;

    fn paired(id_hex: char, os: Os, name: &str) -> PairedPeer {
        let hex: String = std::iter::repeat_n(id_hex, 64).collect();
        let fingerprint = Fingerprint::from_hex(&hex).expect("fingerprint");
        PairedPeer {
            name: name.to_owned(),
            id: fingerprint.device_id(),
            os,
            fingerprint,
        }
    }

    fn config() -> Config {
        let mut config = Config::first_run("Mac").expect("config");
        config.device.id = DeviceId::new("f".repeat(32));
        config
    }

    #[test]
    fn a_mac_puts_a_pc_on_its_right_and_a_pc_puts_a_mac_on_its_left() {
        let mut mac = config();
        let pc = paired('a', Os::Windows, "Windows PC");
        assert_eq!(
            add_peer(&mut mac, &pc, Os::MacOs),
            Some((Side::Right, Replaced::Nothing))
        );
        assert!(mac.validate().is_ok());
        assert_eq!(
            mac.peers[0].pinned_fingerprint(),
            Some("a".repeat(64).as_str())
        );

        let mut windows = config();
        let the_mac = paired('b', Os::MacOs, "Mac");
        assert_eq!(
            add_peer(&mut windows, &the_mac, Os::Windows),
            Some((Side::Left, Replaced::Nothing))
        );
        assert!(windows.validate().is_ok());
    }

    #[test]
    fn pairing_again_replaces_the_entry_and_keeps_its_side_and_address() {
        let mut mac = config();
        let pc = paired('a', Os::Windows, "Windows PC");
        add_peer(&mut mac, &pc, Os::MacOs);
        mac.peers[0].side = Side::Below;
        mac.peers[0].address = Some("192.168.1.20".to_owned());
        let renamed = paired('a', Os::Windows, "Krish's PC");
        assert_eq!(
            add_peer(&mut mac, &renamed, Os::MacOs),
            Some((Side::Below, Replaced::Entry))
        );
        assert_eq!(mac.peers.len(), 1);
        assert_eq!(mac.peers[0].name, "Krish's PC");
        assert_eq!(mac.peers[0].address.as_deref(), Some("192.168.1.20"));
    }

    #[test]
    fn an_unpaired_entry_for_the_same_os_is_taken_over() {
        let mut mac = config();
        mac.peers.push(PeerConfig {
            name: "PC".to_owned(),
            id: DeviceId::new("0123456789abcdef0123456789abcdef"),
            os: Os::Windows,
            side: Side::Above,
            address: Some("krish-pc.local".to_owned()),
            fingerprint: None,
        });
        let pc = paired('c', Os::Windows, "Windows PC");
        assert_eq!(
            add_peer(&mut mac, &pc, Os::MacOs),
            Some((Side::Above, Replaced::Entry))
        );
        assert_eq!(mac.peers.len(), 1);
        assert_eq!(mac.peers[0].id, pc.id);
        assert_eq!(mac.peers[0].address.as_deref(), Some("krish-pc.local"));
        assert!(mac.validate().is_ok());
    }

    #[test]
    fn pairing_a_computer_that_has_a_new_key_replaces_its_old_pin() {
        // The PC lost its key (or it was deleted on purpose) and made a new
        // one, and Hop told the user to pair again. The old pin must go:
        // `hop run` shares with the first paired computer, so a second entry
        // would never be used, and the old key would stay trusted.
        let mut mac = config();
        add_peer(&mut mac, &paired('a', Os::Windows, "Krish's PC"), Os::MacOs);
        mac.peers[0].address = Some("192.168.1.20".to_owned());
        let new_key = paired('b', Os::Windows, "Windows PC");
        let placed = add_peer(&mut mac, &new_key, Os::MacOs).expect("saved");
        assert_eq!(placed.0, Side::Right, "it keeps its side");
        assert_eq!(mac.peers.len(), 1, "the old key is forgotten");
        assert_eq!(mac.peers[0].id, new_key.id);
        assert_eq!(
            mac.peers[0].pinned_fingerprint(),
            Some("b".repeat(64).as_str())
        );
        assert_eq!(mac.peers[0].name, "Windows PC");
        assert_eq!(mac.peers[0].address.as_deref(), Some("192.168.1.20"));
        assert!(mac.validate().is_ok());
    }

    #[test]
    fn a_taken_side_moves_the_new_peer_to_the_next_free_one() {
        // Two Linux computers already sit right and left, so a new PC goes
        // above. With two of an OS, Hop cannot tell which one a new key
        // belongs to, so another Linux computer is added, not swapped in.
        let mut mac = config();
        for (hex, side) in [('a', Side::Right), ('b', Side::Left)] {
            let linux = paired(hex, Os::Linux, "Linux");
            mac.peers.push(PeerConfig {
                name: linux.name,
                id: linux.id,
                os: Os::Linux,
                side,
                address: None,
                fingerprint: Some(linux.fingerprint.to_hex()),
            });
        }
        assert_eq!(
            add_peer(&mut mac, &paired('c', Os::Windows, "PC"), Os::MacOs),
            Some((Side::Above, Replaced::Nothing))
        );
        assert_eq!(
            add_peer(&mut mac, &paired('d', Os::Linux, "Three"), Os::MacOs),
            Some((Side::Below, Replaced::Nothing))
        );
        assert!(mac.validate().is_ok());
        assert_eq!(
            add_peer(&mut mac, &paired('e', Os::Linux, "Four"), Os::MacOs),
            None,
            "every side is taken"
        );
        assert_eq!(mac.peers.len(), 4);
    }

    #[test]
    fn a_computer_with_another_os_never_replaces_a_pairing() {
        let mut mac = config();
        add_peer(&mut mac, &paired('a', Os::Windows, "PC"), Os::MacOs);
        assert_eq!(
            add_peer(&mut mac, &paired('b', Os::Linux, "Linux"), Os::MacOs),
            Some((Side::Left, Replaced::Nothing))
        );
        assert_eq!(mac.peers.len(), 2);
    }

    #[test]
    fn saving_a_new_key_says_which_pairing_it_replaced() {
        let dir = std::env::temp_dir().join(format!("hop-pair-rekey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let own = DeviceId::new("f".repeat(32));
        let mut mac = config();
        mac.device.id = own.clone();
        add_peer(&mut mac, &paired('a', Os::Windows, "Krish's PC"), Os::MacOs);
        mac.save(&path).expect("save");
        let new_key = paired('b', Os::Windows, "Krish's PC");
        // The computer showing the code says so before the user answers.
        assert_eq!(
            replaced_old_key(&path, &new_key).as_deref(),
            Some("Krish's PC")
        );
        assert_eq!(
            replaced_old_key(&path, &paired('a', Os::Windows, "Krish's PC")),
            None,
            "the same key again replaces nothing"
        );
        let saved = save_peer(&path, &own, &new_key).expect("save");
        assert_eq!(saved.replaced, Replaced::OldKey("Krish's PC".to_owned()));
        assert_eq!(saved.side, Side::Right);
        let (loaded, _) = Config::load(&path).expect("load");
        assert_eq!(loaded.peers.len(), 1);
        assert_eq!(
            loaded.peers[0].pinned_fingerprint(),
            Some("b".repeat(64).as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nameless_peer_gets_a_plain_name() {
        let mut mac = config();
        add_peer(&mut mac, &paired('a', Os::Windows, "  "), Os::MacOs);
        assert_eq!(mac.peers[0].name, "Windows PC");
        assert!(mac.validate().is_ok());
    }

    #[test]
    fn saving_rereads_the_file_and_pins_the_peer() {
        let dir = std::env::temp_dir().join(format!("hop-pair-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let own = DeviceId::new("f".repeat(32));
        let mut first = config();
        first.device.id = own.clone();
        first.save(&path).expect("save");
        // Something else changed the file after this process loaded it.
        let (mut newer, _) = Config::load(&path).expect("load");
        newer.input.edge_delay_ms = 150;
        newer.save(&path).expect("save");

        let pc = paired('a', Os::Windows, "Windows PC");
        let saved = save_peer(&path, &own, &pc).expect("save peer");
        assert_eq!(saved.side, Side::Right);
        let (loaded, warnings) = Config::load(&path).expect("load");
        assert!(warnings.is_empty());
        assert_eq!(loaded.input.edge_delay_ms, 150, "the newer file was kept");
        assert_eq!(
            loaded.peers[0].pinned_fingerprint(),
            Some("a".repeat(64).as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
