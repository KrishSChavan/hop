//! The window's state, and the rules for how what happens changes it.
//!
//! Nothing here touches the network, the disk, the clock or the screen: the
//! app (`app.rs`) carries out what the user asks for and feeds the results
//! back in, and the view (`view.rs`) draws whatever is here. So every rule
//! below has a test that needs no window and no network.

pub mod pairing;
pub mod sharing;
pub mod words;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};

use hop_agent::settings::{Overview, PeerView};
use hop_agent::{DeviceId, Os, PermissionState};

pub use self::pairing::Pairing;
pub use self::sharing::Sharing;

/// How many lines the Activity list keeps.
pub const ACTIVITY_LINES: usize = 100;

/// Everything the window shows.
pub struct Model {
    /// This computer's OS.
    pub os: Os,
    /// This computer and the computers it is paired with, from the config.
    pub settings: Settings,
    /// The Sharing section.
    pub sharing: Sharing,
    /// The Pair section.
    pub pairing: Pairing,
    /// The fields and dialogs of "Your other computer".
    pub peers: PeerForms,
    /// The Accessibility permission on a Mac; `None` elsewhere, where the
    /// Permissions section is not shown.
    pub permission: Option<PermissionState>,
    /// The Activity list.
    pub activity: Activity,
    /// The window is closing: sharing and pairing are being stopped, and
    /// nothing new may start.
    pub quitting: bool,
}

/// The config, as far as the window is concerned.
pub enum Settings {
    /// Read: this computer's name and the other computers.
    Loaded(Overview),
    /// Could not be read: why, shown at the top of the window.
    Failed(String),
}

/// The fields and dialogs of "Your other computer".
#[derive(Default)]
pub struct PeerForms {
    /// Each computer's Address field, as typed.
    pub addresses: HashMap<DeviceId, String>,
    /// The computer the Forget dialog asks about.
    pub forgetting: Option<DeviceId>,
    /// The last change that could not be saved: the computer, and why.
    pub problem: Option<(DeviceId, String)>,
}

/// The Activity list: the last [`ACTIVITY_LINES`] lines, oldest first.
#[derive(Default)]
pub struct Activity(VecDeque<String>);

impl Activity {
    /// Adds a line at the bottom, dropping the oldest beyond the limit.
    pub fn push(&mut self, line: impl Into<String>) {
        self.0.push_back(line.into());
        while self.0.len() > ACTIVITY_LINES {
            self.0.pop_front();
        }
    }

    /// The lines, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl Model {
    /// The window at startup.
    pub fn new(os: Os, settings: Settings, permission: Option<PermissionState>) -> Model {
        let mut model = Model {
            os,
            settings: Settings::Failed(String::new()),
            sharing: Sharing::default(),
            pairing: Pairing::default(),
            peers: PeerForms::default(),
            permission,
            activity: Activity::default(),
            quitting: false,
        };
        model.set_settings(settings);
        model
    }

    /// The computers that are paired (those Hop has a key for), in the
    /// config's order. The first one is the one Hop shares with.
    pub fn paired_peers(&self) -> impl Iterator<Item = &PeerView> {
        let peers = match &self.settings {
            Settings::Loaded(overview) => overview.peers.as_slice(),
            Settings::Failed(_) => &[],
        };
        peers.iter().filter(|peer| peer.paired)
    }

    /// Whether anything is paired, so sharing can start.
    pub fn has_paired_peer(&self) -> bool {
        self.paired_peers().next().is_some()
    }

    /// Whether the settings could be read.
    pub fn settings_loaded(&self) -> bool {
        matches!(self.settings, Settings::Loaded(_))
    }

    /// Takes newly read settings. Returns whether the computers changed (a
    /// side, an address, a pairing), so a running agent must restart.
    ///
    /// Each Address field follows the config, unless the user has typed
    /// something else there that is not saved yet.
    pub fn set_settings(&mut self, settings: Settings) -> bool {
        let old_peers = match &self.settings {
            Settings::Loaded(overview) => overview.peers.clone(),
            Settings::Failed(_) => Vec::new(),
        };
        let changed = match &settings {
            Settings::Loaded(overview) => overview.peers != old_peers,
            Settings::Failed(_) => false,
        };
        if let Settings::Loaded(overview) = &settings {
            let mut fields = std::mem::take(&mut self.peers.addresses);
            for peer in &overview.peers {
                let saved = peer.address.clone().unwrap_or_default();
                let before = old_peers
                    .iter()
                    .find(|old| old.id == peer.id)
                    .map(|old| old.address.clone().unwrap_or_default());
                let field = match fields.remove(&peer.id) {
                    // Still showing what was saved before: show the new one.
                    Some(typed) if before.as_ref() == Some(&typed) => saved,
                    Some(typed) => typed,
                    None => saved,
                };
                self.peers.addresses.insert(peer.id.clone(), field);
            }
            let listed = |id: &DeviceId| overview.peers.iter().any(|peer| peer.id == *id);
            if self.peers.forgetting.as_ref().is_some_and(|id| !listed(id)) {
                self.peers.forgetting = None;
            }
            if self
                .peers
                .problem
                .as_ref()
                .is_some_and(|(id, _)| !listed(id))
            {
                self.peers.problem = None;
            }
        }
        self.settings = settings;
        changed
    }

    /// After an address was saved: its field shows the address as the
    /// config now has it (trimmed, or empty once removed).
    pub fn show_saved_address(&mut self, id: &DeviceId) {
        let Settings::Loaded(overview) = &self.settings else {
            return;
        };
        if let Some(peer) = overview.peers.iter().find(|peer| peer.id == *id) {
            self.peers
                .addresses
                .insert(id.clone(), peer.address.clone().unwrap_or_default());
        }
    }

    /// The computer the Forget dialog asks about, while it is open.
    pub fn forgetting(&self) -> Option<&PeerView> {
        let id = self.peers.forgetting.as_ref()?;
        self.paired_peers().find(|peer| peer.id == *id)
    }

    /// The lines under the sharing button: why it is greyed out, where to
    /// push the cursor and the emergency keys while sharing, and (on
    /// Windows) the firewall's question.
    pub fn sharing_hints(&self) -> Vec<String> {
        let mut hints = Vec::new();
        if !self.sharing.is_active() {
            if !self.settings_loaded() {
                hints.push(words::SETTINGS_FIRST.to_owned());
            } else if !self.has_paired_peer() {
                hints.push(words::PAIR_FIRST.to_owned());
            }
        }
        if let Some(running) = self.sharing.running() {
            hints.push(words::push_past(running.side, &running.peer));
            hints.push(running.emergency_keys.clone());
        }
        if self.os == Os::Windows {
            hints.push(words::FIREWALL.to_owned());
        }
        hints
    }

    /// The sharing button: its label, and whether it can be clicked.
    pub fn sharing_button(&self) -> (&'static str, bool) {
        let (label, enabled) = self.sharing.button(self.has_paired_peer());
        (label, enabled && !self.quitting)
    }

    /// Whether a pairing session may start now.
    pub fn can_pair(&self) -> bool {
        !self.quitting && self.settings_loaded() && !self.pairing.busy
    }
}
