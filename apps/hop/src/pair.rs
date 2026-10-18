//! `hop pair`: pairs this computer with the other one, once
//! (docs/M2-DESIGN.md, "Pairing").
//!
//! Without a code, this computer shows one and waits; with a code, it joins
//! the computer showing it. Pairing runs in a terminal on both computers,
//! never through `open` on the Mac: `open` would send the output, and so
//! the code, to a log file. The code is printed on screen only and never
//! logged.

use std::io::{self, BufRead as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hop_config::{Config, ConfigError, PeerConfig, Side};
use hop_net::{
    Identity, PairError, PairInfo, PairedPeer, PairingCode, PairingHost, PairingTiming,
    find_pairing_hosts, pair_join, resolve_address,
};
use hop_types::{DeviceId, Os};
use zeroize::Zeroizing;

use crate::logging::{Destination, Filter, Log};
use crate::paths;
use crate::startup::{self, Startup, default_device_name};

/// How long the joiner listens for a computer showing a code.
const FIND_WAIT: Duration = Duration::from_secs(3);

/// How long a host name given with `--address` may take to look up.
const RESOLVE_WAIT: Duration = Duration::from_secs(5);

/// Runs `hop pair`: shows a code when `code` is empty, joins otherwise.
pub(crate) fn run(code: &[String], address: Option<&str>) -> Result<ExitCode> {
    let log = start_log();
    let startup = startup::load()?;
    for note in &startup.notes {
        println!("{note}");
    }
    let port =
        startup.config.network.port.checked_add(1).context(
            "network.port must be at most 65534, because pairing uses the port after it",
        )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("could not start the network threads")?;
    let result = if code.is_empty() {
        runtime.block_on(host(&startup, port))
    } else {
        // "482 913" may arrive as two arguments.
        let typed = Zeroizing::new(code.join(" "));
        runtime.block_on(join(&startup, &typed, address, port))
    };
    // Waiting for the y/N answer may leave a thread reading the terminal;
    // the process ends anyway.
    runtime.shutdown_timeout(Duration::from_secs(1));
    if let Some(log) = log {
        log.flush(Duration::from_secs(1));
    }
    result
}

/// Pairing writes its own log next to the agent's. Only warnings from the
/// network layer end up there (never the code).
fn start_log() -> Option<Log> {
    let path = paths::log_dir()?.join("pair.log");
    let log = Log::start(Destination::File(path)).ok()?;
    log.install(Filter::AGENT);
    Some(log)
}

/// What this computer tells the other one about itself.
fn pair_info(startup: &Startup) -> PairInfo {
    PairInfo {
        name: startup.config.device.name.clone(),
        os: Os::current(),
    }
}

/// Binds the pairing listener with a new code, and returns it with the
/// code to show.
async fn bind_host(
    startup: &Startup,
    address: SocketAddr,
    advertise: bool,
) -> Result<(PairingHost, Zeroizing<String>), PairError> {
    let code = PairingCode::generate()?;
    let shown = Zeroizing::new(code.reveal());
    let host = PairingHost::bind(
        startup.identity.clone(),
        pair_info(startup),
        address,
        code,
        PairingTiming::default(),
        advertise,
    )
    .await?;
    Ok((host, shown))
}

/// Shows a code and waits for the other computer.
async fn host(startup: &Startup, port: u16) -> Result<ExitCode> {
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let (mut host, shown) = match bind_host(startup, address, true).await {
        Ok(bound) => bound,
        Err(PairError::Discovery(reason)) => {
            // Pairing still works with --address. The first code was never
            // shown; a new one goes with the new listener.
            println!(
                "Could not announce this computer on the local network ({reason}). On the \
                 other computer, add --address with this computer's IP address."
            );
            let mut retries = 0;
            loop {
                // The first listener closes in the background; until it has,
                // the port is still taken.
                tokio::time::sleep(Duration::from_millis(200)).await;
                match bind_host(startup, address, false).await {
                    Ok(bound) => break bound,
                    Err(PairError::Bind { .. }) if retries < 5 => retries += 1,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Err(error) => return Err(error.into()),
    };
    println!();
    println!("Pairing code: {}", shown.as_str());
    println!();
    println!(
        "On the other computer, open a terminal and run hop pair with this code, for example: \
         hop pair {}",
        shown.as_str()
    );
    println!(
        "The code works once, for 2 minutes. Never show it in a screen share. Waiting for the \
         other computer..."
    );
    drop(shown);
    host.on_failed_attempt(|failures| {
        println!("Someone entered a wrong code ({failures} of 3 tries used).");
    });
    let pending = match host.wait_for_joiner().await {
        Ok(pending) => pending,
        Err(error) => {
            println!("Not paired: {error}.");
            return Ok(ExitCode::FAILURE);
        }
    };
    let joiner = pending.peer().clone();
    // Said before the answer, not only after: a yes forgets the old key.
    let replaces = match replaced_old_key(&startup.config_path, &joiner) {
        Some(old) => {
            format!(" This replaces the pairing with {old}, whose old key will stop working.")
        }
        None => String::new(),
    };
    let question = format!(
        "{name} ({os}) entered the code. Pair with it?{replaces} Answer y only if {name} says \
         it is waiting for you. [y/N] ",
        name = joiner.name,
        os = joiner.os,
    );
    if !ask_yes_no(&question, pending.remaining()).await {
        pending.decline().await;
        println!("Not paired. Nothing was saved on either computer.");
        return Ok(ExitCode::FAILURE);
    }
    let mut saved = None;
    let own_id = startup.identity.device_id();
    let result = pending
        .accept(|peer| -> Result<(), SaveError> {
            saved = Some(save_peer(&startup.config_path, &own_id, peer)?);
            Ok(())
        })
        .await;
    match (result, saved) {
        (Ok(_), Some(saved)) => {
            print_saved(&saved);
            Ok(ExitCode::SUCCESS)
        }
        (Err(error), Some(saved)) => {
            // Saved here, but the other computer may not have heard.
            print_saved(&saved);
            println!("However: {error}.");
            Ok(ExitCode::FAILURE)
        }
        (Err(error), None) => {
            println!("Not paired: {error}.");
            Ok(ExitCode::FAILURE)
        }
        (Ok(peer), None) => {
            // `accept` succeeds only after the save ran.
            println!("Paired with {}, but nothing was recorded here.", peer.name);
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Asks a y/N question, taking "no" if no answer comes within `within`.
async fn ask_yes_no(question: &str, within: Duration) -> bool {
    print!("{question}");
    let _ = io::stdout().flush();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A plain thread, not the runtime's blocking pool: a terminal read
    // cannot be cancelled, and the runtime must not wait for it on exit.
    let reader = std::thread::Builder::new()
        .name("hop-pair-answer".into())
        .spawn(move || {
            let mut line = String::new();
            let answer = io::stdin().lock().read_line(&mut line).ok().map(|_| line);
            let _ = tx.send(answer);
        });
    if reader.is_err() {
        return false;
    }
    match tokio::time::timeout(within, rx).await {
        Ok(Ok(Some(line))) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Ok(_) => false,
        Err(_) => {
            println!();
            println!("No answer in time.");
            false
        }
    }
}

/// Joins the computer showing the code.
async fn join(
    startup: &Startup,
    typed: &str,
    address: Option<&str>,
    port: u16,
) -> Result<ExitCode> {
    let code = PairingCode::parse(typed)?;
    let addresses = match address {
        Some(text) => resolve_address(text, port, RESOLVE_WAIT)
            .await
            .with_context(|| format!("could not find {text}"))?,
        None => match find_host(&startup.identity).await? {
            Some(addresses) => addresses,
            None => return Ok(ExitCode::FAILURE),
        },
    };
    println!("Connecting...");
    let result = pair_join(
        startup.identity.clone(),
        pair_info(startup),
        code,
        &addresses,
        PairingTiming::default(),
        |host| println!("Waiting for you to confirm on {}.", host.name),
    )
    .await;
    let peer = match result {
        Ok(peer) => peer,
        Err(error) => {
            println!("Not paired: {error}.");
            return Ok(ExitCode::FAILURE);
        }
    };
    match save_peer(&startup.config_path, &startup.identity.device_id(), &peer) {
        Ok(saved) => {
            print_saved(&saved);
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            println!(
                "{} said yes, but this computer could not save the pairing: {error}. Run hop \
                 pair again on both computers.",
                peer.name
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Looks for exactly one computer showing a code. `None` (after saying
/// why) when there is none, or more than one: then the user picks with
/// `--address`, because choosing for them could pick an impostor.
async fn find_host(identity: &Arc<Identity>) -> Result<Option<Vec<SocketAddr>>> {
    println!("Looking for the computer showing the code...");
    let hosts = find_pairing_hosts(identity.device_id(), FIND_WAIT)
        .await
        .context("could not look for the other computer on the local network")?;
    match hosts.as_slice() {
        [] => {
            println!(
                "Could not find a computer showing a pairing code. Check that hop pair is \
                 waiting on the other computer, or add --address with its IP address."
            );
            Ok(None)
        }
        [only] => Ok(Some(
            only.addresses.iter().copied().map(SocketAddr::V4).collect(),
        )),
        several => {
            println!("More than one computer is showing a pairing code:");
            for found in several {
                let addresses: Vec<String> =
                    found.addresses.iter().map(|a| a.ip().to_string()).collect();
                println!("  {} at {}", found.name, addresses.join(", "));
            }
            println!("Run hop pair again with --address and the address of the one you mean.");
            Ok(None)
        }
    }
}

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
pub(crate) struct Saved {
    pub(crate) name: String,
    pub(crate) os: Os,
    pub(crate) side: Side,
    /// Which entry it went into.
    pub(crate) replaced: Replaced,
    pub(crate) path: PathBuf,
}

/// Which `[[peers]]` entry a pairing went into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Replaced {
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
fn replaced_old_key(path: &Path, peer: &PairedPeer) -> Option<String> {
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

fn print_saved(saved: &Saved) {
    println!();
    println!(
        "Paired with {} ({}). {} {}.",
        saved.name,
        saved.os,
        if saved.replaced == Replaced::Nothing {
            "Saved it in"
        } else {
            "Updated its entry in"
        },
        saved.path.display()
    );
    if let Replaced::OldKey(old) = &saved.replaced {
        println!(
            "It replaces the pairing with {old}, which used a different key: that key can no \
             longer connect to this computer."
        );
    }
    let (placement, edge) = match saved.side {
        Side::Left => ("to the left of", "left"),
        Side::Right => ("to the right of", "right"),
        Side::Above => ("above", "top"),
        Side::Below => ("below", "bottom"),
    };
    println!(
        "It sits {placement} this computer's screen: push the cursor past the {edge} edge to \
         move to it."
    );
    println!(
        "To put it on another side, change side = \"{}\" in its [[peers]] entry in that file \
         to left, right, above or below.",
        saved.side
    );
    println!("If Hop is running, quit it and start it again to use the new pairing.");
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
