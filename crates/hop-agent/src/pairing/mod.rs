//! Pairing this computer with the other one, once (docs/M2-DESIGN.md,
//! "Pairing"): the flow of `hop pair`, without a terminal.
//!
//! One computer shows a code ([`PairRole::Show`]) and waits; the other
//! types it ([`PairRole::Join`]). After the code checks out, the user on the
//! showing computer says yes or no, and both computers save each other's
//! key. The code itself never crosses the network.
//!
//! One async flow does the work for both front ends: the terminal (`hop
//! pair`, in `cli`) and the app ([`PairSession`]). The flow reports progress
//! as [`PairEvent`]s and asks the front end the one question it needs
//! answered; each front end words the events its own way.
//!
//! The code is shown on screen only. It is never logged, it lives in a
//! [`Zeroizing`] string that is wiped when dropped, and nothing here prints
//! it in `Debug` output.

mod save;
mod session;

#[cfg(test)]
mod tests;

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use hop_net::{
    PairError, PairInfo, PairingCode, PairingHost, PairingTiming, find_pairing_hosts, pair_join,
    resolve_address,
};
use hop_types::Os;
use zeroize::Zeroizing;

pub use self::save::{Replaced, Saved};
use self::save::{SaveError, replaced_old_key, save_peer};
pub use self::session::PairSession;
use crate::startup::Startup;

/// How long the joiner listens for a computer showing a code.
const FIND_WAIT: Duration = Duration::from_secs(3);

/// How long a host name given as the address may take to look up.
const RESOLVE_WAIT: Duration = Duration::from_secs(5);

/// Which side of pairing this computer takes.
pub enum PairRole {
    /// Show a new code and wait for the other computer to type it.
    Show,
    /// Type the code the other computer shows.
    Join {
        /// The code as the user typed it: six digits, spaces and dashes
        /// allowed. Wiped from memory when dropped.
        code: Zeroizing<String>,
        /// The other computer's address (an IP address or host name, with an
        /// optional `:port`), for when it is not found on the network by
        /// itself. `None` looks for it.
        address: Option<String>,
    },
}

impl fmt::Debug for PairRole {
    /// Never shows the code.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairRole::Show => f.write_str("Show"),
            PairRole::Join { address, .. } => f
                .debug_struct("Join")
                .field("code", &"(redacted)")
                .field("address", address)
                .finish(),
        }
    }
}

/// What happens while pairing, in order. Text in here is plain words for
/// the user, except the code, which is for the screen only.
pub enum PairEvent {
    /// Showing: the code to show, such as "482 913". Show it large, on this
    /// screen only; never log it. Wiped from memory when dropped.
    Code(Zeroizing<String>),
    /// Showing: this computer could not be announced on the local network
    /// (`reason`), so the other computer needs this computer's address. The
    /// code comes next, as usual.
    NotAnnounced {
        /// Why, from the mDNS responder.
        reason: String,
    },
    /// Showing: someone typed a wrong code; after 3 the code dies.
    WrongCode {
        /// Wrong tries so far.
        failures: u32,
    },
    /// Showing: the other computer typed the right code. Ask the user
    /// whether to pair, and answer with [`PairSession::answer`]; no answer
    /// within `within` counts as no.
    Confirm {
        /// The other computer's name.
        name: String,
        /// Its OS.
        os: Os,
        /// A sentence to show with the question when a yes would replace
        /// the pairing with a computer that used another key: "This
        /// replaces the pairing with …, whose old key will stop working."
        replaces: Option<String>,
        /// How long the user has to answer.
        within: Duration,
    },
    /// Joining: looking for the computer showing the code on the local
    /// network.
    Searching,
    /// Joining: no computer is showing a code on the local network; the
    /// user can enter its address instead. [`PairEvent::Finished`] follows.
    NoHostFound,
    /// Joining: more than one computer is showing a code: their names and
    /// addresses. Hop never picks one, because it could be an impostor;
    /// the user enters the address of the one they mean.
    /// [`PairEvent::Finished`] follows.
    SeveralHosts(Vec<(String, Vec<IpAddr>)>),
    /// Joining: connecting to the computer showing the code.
    Connecting,
    /// Joining: the code checked out; the user must now say yes on `host`.
    WaitingForConfirmation {
        /// The name of the computer showing the code.
        host: String,
    },
    /// Showing: this computer saved the pairing, but the other computer
    /// may not have heard the yes (`reason` says what happened). If it
    /// reports a failure, pair again on both computers.
    /// [`PairEvent::Finished`] with the saved pairing follows.
    Unconfirmed {
        /// What went wrong after the save.
        reason: String,
    },
    /// The end: what this computer saved, or why it is not paired (a plain
    /// sentence fragment in the app's words, such as "the code did not
    /// match. Check it…", to show as "Not paired: {reason}."). Always the
    /// last event. The reason never holds the code, but it may quote the
    /// address the user typed (in case that was the code by mistake, show
    /// it rather than log it).
    Finished(Result<Saved, String>),
}

impl fmt::Debug for PairEvent {
    /// Never shows the code.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairEvent::Code(_) => f.write_str("Code(redacted)"),
            PairEvent::NotAnnounced { reason } => f
                .debug_struct("NotAnnounced")
                .field("reason", reason)
                .finish(),
            PairEvent::WrongCode { failures } => f
                .debug_struct("WrongCode")
                .field("failures", failures)
                .finish(),
            PairEvent::Confirm {
                name,
                os,
                replaces,
                within,
            } => f
                .debug_struct("Confirm")
                .field("name", name)
                .field("os", os)
                .field("replaces", replaces)
                .field("within", within)
                .finish(),
            PairEvent::Searching => f.write_str("Searching"),
            PairEvent::NoHostFound => f.write_str("NoHostFound"),
            PairEvent::SeveralHosts(hosts) => f.debug_tuple("SeveralHosts").field(hosts).finish(),
            PairEvent::Connecting => f.write_str("Connecting"),
            PairEvent::WaitingForConfirmation { host } => f
                .debug_struct("WaitingForConfirmation")
                .field("host", host)
                .finish(),
            PairEvent::Unconfirmed { reason } => f
                .debug_struct("Unconfirmed")
                .field("reason", reason)
                .finish(),
            PairEvent::Finished(result) => f.debug_tuple("Finished").field(result).finish(),
        }
    }
}

/// The question the flow asks its front end on the showing computer.
pub(crate) struct Question {
    /// The computer that typed the code.
    pub(crate) name: String,
    pub(crate) os: Os,
    /// See [`PairEvent::Confirm`].
    pub(crate) replaces: Option<String>,
    /// No answer within this counts as no.
    pub(crate) within: Duration,
}

/// A front end of the pairing flow: the terminal or the app.
pub(crate) trait Front {
    /// Says what is happening. Never gets [`PairEvent::Confirm`],
    /// [`PairEvent::Unconfirmed`] or [`PairEvent::Finished`]: the question
    /// goes to [`Front::confirm`], and the outcome is the flow's result.
    fn event(&mut self, event: PairEvent);

    /// Asks whether to pair with the computer that typed the code. No answer
    /// within `question.within` must count as no.
    fn confirm(&mut self, question: Question) -> impl Future<Output = bool>;

    /// The pairing was saved in this computer's config, before the other
    /// computer has heard the yes. Only the app needs this, in case the
    /// user cancels in that moment.
    fn saved(&mut self, _saved: &Saved) {}
}

/// How the flow pairs: the files it uses and the network it listens on.
pub(crate) struct Setup {
    /// The config and this computer's key.
    pub(crate) startup: Startup,
    /// The pairing port: `network.port + 1`. The joiner's default port for
    /// an address without one, and the port the showing computer listens
    /// on.
    pub(crate) port: u16,
    /// Where the showing computer listens: every interface, or loopback in
    /// tests.
    pub(crate) listen_ip: Ipv4Addr,
    /// Whether the showing computer announces itself over mDNS (multicast;
    /// tests turn it off).
    pub(crate) advertise: bool,
    pub(crate) timing: PairingTiming,
    /// Tests learn the listening address here.
    pub(crate) listening: Option<mpsc::Sender<SocketAddr>>,
}

impl Setup {
    /// What `hop pair` and the app use: the port after `network.port`, on
    /// every interface, announced over mDNS, with the design's timeouts.
    ///
    /// # Errors
    ///
    /// `network.port` is 65535, so there is no port after it.
    pub(crate) fn new(startup: Startup) -> Result<Setup> {
        let port = startup.config.network.port.checked_add(1).context(
            "network.port must be at most 65534, because pairing uses the port after it",
        )?;
        Ok(Setup {
            startup,
            port,
            listen_ip: Ipv4Addr::UNSPECIFIED,
            advertise: true,
            timing: PairingTiming::default(),
            listening: None,
        })
    }
}

/// How a pairing run ended, for the front end to word. hop-net's errors
/// stay [`PairError`]s here: their own sentences are the terminal's ("run
/// hop pair again"), and the app words them its own way.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Paired and saved here.
    Paired(Saved),
    /// Saved here, but the other computer may not have heard: `error`.
    SavedHereOnly { saved: Saved, error: PairError },
    /// The user said no, or did not answer in time; nothing was saved on
    /// either computer.
    Declined,
    /// Not paired: `0` says why.
    Failed(PairError),
    /// The flow said yes but nothing was recorded (should not happen:
    /// accepting succeeds only after the save ran).
    NothingRecorded { name: String },
    /// Joining: `name` said yes, but this computer could not save.
    NotSaved { name: String, error: String },
    /// Joining: nothing showing a code was found (the front end was told).
    NoHostFound,
    /// Joining: several computers show a code (the front end was told).
    SeveralHosts,
}

/// The tokio runtime pairing runs on: two workers, like the agent's.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("hop-pair-net")
        .enable_all()
        .build()
        .context("could not start the network threads")
}

/// Pairs, as `role` says, reporting to `front`.
///
/// # Errors
///
/// What stops pairing before it gets going: the pairing port is taken, the
/// code is not six digits, the address cannot be found, discovery cannot
/// start. Everything after that ends in an [`Outcome`].
pub(crate) async fn pair(setup: &Setup, role: PairRole, front: &mut impl Front) -> Result<Outcome> {
    match role {
        PairRole::Show => show(setup, front).await,
        PairRole::Join { code, address } => join(setup, &code, address.as_deref(), front).await,
    }
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
    setup: &Setup,
    address: SocketAddr,
    advertise: bool,
) -> Result<(PairingHost, Zeroizing<String>), PairError> {
    let code = PairingCode::generate()?;
    let shown = Zeroizing::new(code.reveal());
    let host = PairingHost::bind(
        setup.startup.identity.clone(),
        pair_info(&setup.startup),
        address,
        code,
        setup.timing.clone(),
        advertise,
    )
    .await?;
    Ok((host, shown))
}

/// Shows a code and waits for the other computer.
async fn show(setup: &Setup, front: &mut impl Front) -> Result<Outcome> {
    let address = SocketAddr::from((setup.listen_ip, setup.port));
    let (mut host, shown) = match bind_host(setup, address, setup.advertise).await {
        Ok(bound) => bound,
        Err(PairError::Discovery(reason)) => {
            // Pairing still works with the address. The first code was
            // never shown; a new one goes with the new listener.
            front.event(PairEvent::NotAnnounced { reason });
            let mut retries = 0;
            loop {
                // The first listener closes in the background; until it has,
                // the port is still taken.
                tokio::time::sleep(Duration::from_millis(200)).await;
                match bind_host(setup, address, false).await {
                    Ok(bound) => break bound,
                    Err(PairError::Bind { .. }) if retries < 5 => retries += 1,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(listening) = &setup.listening {
        let _ = listening.send(host.local_addr());
    }
    front.event(PairEvent::Code(shown));
    // The callback runs inside the wait; a channel carries its news to the
    // front end without borrowing it there.
    let (failed_tx, mut failed_rx) = tokio::sync::mpsc::unbounded_channel();
    host.on_failed_attempt(move |failures| {
        let _ = failed_tx.send(failures);
    });
    let waiting = host.wait_for_joiner();
    tokio::pin!(waiting);
    let joined = loop {
        tokio::select! {
            biased;
            Some(failures) = failed_rx.recv() => front.event(PairEvent::WrongCode { failures }),
            joined = &mut waiting => break joined,
        }
    };
    // A wrong try that ended the wait (the third) is said before the result.
    while let Ok(failures) = failed_rx.try_recv() {
        front.event(PairEvent::WrongCode { failures });
    }
    let pending = match joined {
        Ok(pending) => pending,
        Err(error) => return Ok(Outcome::Failed(error)),
    };
    let joiner = pending.peer().clone();
    // Said before the answer, not only after: a yes forgets the old key.
    let replaces = replaced_old_key(&setup.startup.config_path, &joiner).map(|old| {
        format!("This replaces the pairing with {old}, whose old key will stop working.")
    });
    let question = Question {
        name: joiner.name.clone(),
        os: joiner.os,
        replaces,
        within: pending.remaining(),
    };
    if !front.confirm(question).await {
        pending.decline().await;
        return Ok(Outcome::Declined);
    }
    let mut saved = None;
    let own_id = setup.startup.identity.device_id();
    let result = pending
        .accept(|peer| -> Result<(), SaveError> {
            let done = save_peer(&setup.startup.config_path, &own_id, peer)?;
            front.saved(&done);
            saved = Some(done);
            Ok(())
        })
        .await;
    Ok(match (result, saved) {
        (Ok(_), Some(saved)) => Outcome::Paired(saved),
        // Saved here, but the other computer may not have heard.
        (Err(error), Some(saved)) => Outcome::SavedHereOnly { saved, error },
        (Err(error), None) => Outcome::Failed(error),
        // `accept` succeeds only after the save ran.
        (Ok(peer), None) => Outcome::NothingRecorded { name: peer.name },
    })
}

/// Joins the computer showing the code.
async fn join(
    setup: &Setup,
    typed: &str,
    address: Option<&str>,
    front: &mut impl Front,
) -> Result<Outcome> {
    let code = PairingCode::parse(typed)?;
    let addresses = match address {
        Some(text) => resolve_address(text, setup.port, RESOLVE_WAIT)
            .await
            .with_context(|| format!("could not find {text}"))?,
        None => {
            // Exactly one computer showing a code, or none: with several,
            // the user picks with the address, because choosing for them
            // could pick an impostor.
            front.event(PairEvent::Searching);
            let hosts = find_pairing_hosts(setup.startup.identity.device_id(), FIND_WAIT)
                .await
                .context("could not look for the other computer on the local network")?;
            match hosts.as_slice() {
                [] => {
                    front.event(PairEvent::NoHostFound);
                    return Ok(Outcome::NoHostFound);
                }
                [only] => only.addresses.iter().copied().map(SocketAddr::V4).collect(),
                several => {
                    let found = several
                        .iter()
                        .map(|host| {
                            let ips = host
                                .addresses
                                .iter()
                                .map(|address| IpAddr::V4(*address.ip()))
                                .collect();
                            (host.name.clone(), ips)
                        })
                        .collect();
                    front.event(PairEvent::SeveralHosts(found));
                    return Ok(Outcome::SeveralHosts);
                }
            }
        }
    };
    front.event(PairEvent::Connecting);
    let identity = setup.startup.identity.clone();
    let result = pair_join(
        identity.clone(),
        pair_info(&setup.startup),
        code,
        &addresses,
        setup.timing.clone(),
        |host| {
            front.event(PairEvent::WaitingForConfirmation {
                host: host.name.clone(),
            });
        },
    )
    .await;
    let peer = match result {
        Ok(peer) => peer,
        Err(error) => return Ok(Outcome::Failed(error)),
    };
    match save_peer(&setup.startup.config_path, &identity.device_id(), &peer) {
        Ok(saved) => Ok(Outcome::Paired(saved)),
        Err(error) => Ok(Outcome::NotSaved {
            name: peer.name,
            error: error.to_string(),
        }),
    }
}
