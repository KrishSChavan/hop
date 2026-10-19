//! Sharing: whether the agent runs, and what it last said.
//!
//! The app asks for a start or a stop here first, and only then touches the
//! agent, so the window always says what is about to happen. The agent's
//! updates ([`AgentUpdate`]) then move the state on; its last one,
//! `Stopped`, always comes, however a run ends.

use hop_agent::Side;
use hop_agent::service::{AgentUpdate, Session};

use super::words;

/// Whether this computer shares its keyboard and mouse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// No agent runs.
    Off {
        /// Why the last run stopped, when nobody asked it to: shown as
        /// "Stopped: {reason}".
        stopped: Option<String>,
    },
    /// The agent is starting: loading the settings, binding the network,
    /// checking Accessibility.
    Starting,
    /// The agent runs.
    Running(Running),
    /// The agent was asked to stop, and has not said `Stopped` yet.
    Stopping {
        /// Start again once it has stopped: a setting changed, or a new
        /// pairing arrived, while it ran.
        restart: bool,
    },
}

/// What a running agent said about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    /// The paired computer it shares with, as the settings name it.
    pub peer: String,
    /// Where that computer sits.
    pub side: Side,
    /// How to end a session at once from this keyboard.
    pub emergency_keys: String,
    /// Connected to the other computer.
    pub connected: bool,
    /// Who uses which keyboard and mouse right now.
    pub session: Session,
}

/// The colour of the dot next to the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Not sharing: grey.
    Idle,
    /// Starting, stopping, looking for the other computer: amber.
    Busy,
    /// Connected: green.
    Ready,
    /// One keyboard and mouse controls the other computer: blue.
    Active,
    /// Stopped by an error: red.
    Problem,
}

/// What the app must do after an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing.
    Nothing,
    /// Start the agent again, once the old agent's thread has ended.
    StartAgain,
}

impl Default for Sharing {
    fn default() -> Self {
        Sharing::Off { stopped: None }
    }
}

impl Sharing {
    /// Whether an agent runs or is about to: the App Nap guard is held, and
    /// the button says Stop sharing.
    pub fn is_active(&self) -> bool {
        !matches!(self, Sharing::Off { .. })
    }

    /// The agent's own words while it runs.
    pub fn running(&self) -> Option<&Running> {
        match self {
            Sharing::Running(running) => Some(running),
            _ => None,
        }
    }

    /// The user clicked Start sharing. True when the app must start the
    /// agent; nothing changes while one is still active.
    pub fn start(&mut self) -> bool {
        match self {
            Sharing::Off { .. } => {
                *self = Sharing::Starting;
                true
            }
            _ => false,
        }
    }

    /// The user clicked Stop sharing (or the window is closing). True when
    /// the app must ask the agent to stop; false when it was asked already
    /// or none runs. A restart that was waiting is called off.
    pub fn stop(&mut self) -> bool {
        match self {
            Sharing::Starting | Sharing::Running(_) => {
                *self = Sharing::Stopping { restart: false };
                true
            }
            Sharing::Stopping { restart } => {
                *restart = false;
                false
            }
            Sharing::Off { .. } => false,
        }
    }

    /// A setting changed, or a new pairing arrived: a running agent must
    /// stop and start again to pick it up. True when the app must ask it to
    /// stop now. An agent already stopping because the user asked stays
    /// stopped.
    pub fn restart(&mut self) -> bool {
        match self {
            Sharing::Starting | Sharing::Running(_) => {
                *self = Sharing::Stopping { restart: true };
                true
            }
            Sharing::Stopping { .. } | Sharing::Off { .. } => false,
        }
    }

    /// The agent could not even be started (its thread did not start).
    pub fn failed_to_start(&mut self, why: String) {
        *self = Sharing::Off { stopped: Some(why) };
    }

    /// Applies what the agent said.
    pub fn apply(&mut self, update: AgentUpdate) -> Next {
        match update {
            AgentUpdate::Started {
                peer,
                side,
                emergency_keys,
            } => {
                // While stopping, the agent is on its way out regardless.
                if matches!(self, Sharing::Starting | Sharing::Running(_)) {
                    *self = Sharing::Running(Running {
                        peer,
                        side,
                        emergency_keys,
                        connected: false,
                        session: Session::Idle,
                    });
                }
            }
            AgentUpdate::Connected { .. } => {
                if let Sharing::Running(running) = self {
                    running.connected = true;
                }
            }
            AgentUpdate::Disconnected => {
                if let Sharing::Running(running) = self {
                    running.connected = false;
                    running.session = Session::Idle;
                }
            }
            AgentUpdate::Session(session) => {
                if let Sharing::Running(running) = self {
                    running.session = session;
                }
            }
            // Files say nothing about who uses which keyboard and mouse. The
            // model lists their names in Activity (`Model::agent_update`).
            AgentUpdate::FilesReceived { .. } => {}
            AgentUpdate::Stopped { error } => {
                if let Sharing::Stopping { restart: true } = self {
                    *self = Sharing::Starting;
                    return Next::StartAgain;
                }
                *self = Sharing::Off { stopped: error };
            }
        }
        Next::Nothing
    }

    /// The dot's colour and the status line.
    pub fn status(&self) -> (Tone, String) {
        match self {
            Sharing::Off { stopped: None } => (Tone::Idle, words::NOT_SHARING.to_owned()),
            Sharing::Off {
                stopped: Some(reason),
            } => (Tone::Problem, words::stopped(reason)),
            // A restart reads as a start: sharing comes back by itself.
            Sharing::Starting | Sharing::Stopping { restart: true } => {
                (Tone::Busy, words::STARTING.to_owned())
            }
            Sharing::Stopping { restart: false } => (Tone::Busy, words::STOPPING.to_owned()),
            Sharing::Running(running) => match running.session {
                Session::Controlling => (Tone::Active, words::using(&running.peer)),
                Session::Controlled => (Tone::Active, words::used_by(&running.peer)),
                Session::Idle if running.connected => {
                    (Tone::Ready, words::connected_to(&running.peer))
                }
                Session::Idle => (Tone::Busy, words::looking_for(&running.peer)),
            },
        }
    }

    /// The sharing button: its label, and whether it can be clicked (given
    /// whether anything is paired). A stop in progress cannot be hurried.
    pub fn button(&self, paired: bool) -> (&'static str, bool) {
        match self {
            Sharing::Off { .. } => (words::START_SHARING, paired),
            Sharing::Starting | Sharing::Running(_) | Sharing::Stopping { restart: true } => {
                (words::STOP_SHARING, true)
            }
            Sharing::Stopping { restart: false } => (words::STOP_SHARING, false),
        }
    }
}
