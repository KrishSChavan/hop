//! Pairing: the Pair section's state, and how the pairing flow's events
//! ([`PairEvent`]) change it.
//!
//! The code this computer shows lives in a [`Zeroizing`] string, so it is
//! wiped from memory when the session ends. The code typed into the Code
//! field is kept the same way: it stays for another try after a failure,
//! and is wiped once pairing succeeds, when the form is cancelled, and when
//! this computer shows a code instead. Neither is ever written to the log
//! or to the Activity list.

use std::time::{Duration, Instant};

use hop_agent::pairing::{PairEvent, PairRole, Replaced, Saved};
use hop_agent::{Os, Zeroizing};

use super::{Activity, words};

/// The most characters the Code field takes: six digits with spaces or
/// dashes between them fit easily.
pub const CODE_FIELD_CHARS: usize = 20;

/// Bytes reserved for the Code field, so typing into it never moves the
/// text to a new buffer (which would leave the old one unwiped): room for
/// [`CODE_FIELD_CHARS`] characters of up to 4 bytes each.
const CODE_FIELD_BYTES: usize = CODE_FIELD_CHARS * 4;

/// The Pair section.
pub struct Pairing {
    /// What the section shows.
    pub stage: Stage,
    /// A pairing session runs: from its start until its `Finished` event.
    pub busy: bool,
    /// The user clicked Cancel; the session's end is on its way.
    pub cancelling: bool,
    /// How the last session ended, shown under the section's buttons.
    pub outcome: Option<Outcome>,
    /// The Code field ("Enter a code"). Wiped when dropped.
    pub code: Zeroizing<String>,
    /// The Address field ("If the other computer is not found").
    pub address: String,
    /// `Unconfirmed`'s reason, until `Finished` comes.
    unconfirmed: Option<String>,
}

/// What the Pair section shows.
pub enum Stage {
    /// The two buttons only.
    Choose,
    /// "Show a pairing code".
    Show(Showing),
    /// "Enter a code": the form.
    Join(Joining),
}

/// Showing a code, while the session runs.
#[derive(Default)]
pub struct Showing {
    /// The code, once the listener runs. Wiped when dropped.
    pub code: Option<Zeroizing<String>>,
    /// When the code stops working.
    pub expires: Option<Instant>,
    /// Wrong tries so far.
    pub wrong: u32,
    /// Why this computer could not be announced on the network, if so.
    pub not_announced: Option<String>,
    /// The question to the user, once the other computer typed the code.
    pub question: Option<Question>,
    /// The name of the computer the user said yes to, until pairing ends.
    pub accepted: Option<String>,
}

/// "{name} ({OS}) entered the code. Pair with it?"
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The computer that typed the code.
    pub name: String,
    /// Its OS.
    pub os: Os,
    /// The sentence saying which pairing a yes replaces, if any.
    pub replaces: Option<String>,
    /// No answer by then counts as no.
    pub deadline: Instant,
}

/// The "Enter a code" form's progress.
#[derive(Default)]
pub struct Joining {
    /// The current progress line, while a session runs.
    pub progress: Option<String>,
    /// Nothing or several computers were found: what to do, pointing to the
    /// Address field.
    pub problem: Option<String>,
}

/// How a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Paired (green), or not (red).
    pub paired: bool,
    /// What to say, first line first.
    pub lines: Vec<String>,
}

impl Default for Pairing {
    fn default() -> Self {
        Pairing {
            stage: Stage::Choose,
            busy: false,
            cancelling: false,
            outcome: None,
            code: empty_code(),
            address: String::new(),
            unconfirmed: None,
        }
    }
}

/// An empty Code field with room for anything it accepts.
fn empty_code() -> Zeroizing<String> {
    Zeroizing::new(String::with_capacity(CODE_FIELD_BYTES))
}

impl Pairing {
    /// The user clicked "Show a pairing code". True when the app must start
    /// a session showing one; not while another runs. A code typed into the
    /// form is wiped: this computer shows one instead.
    pub fn begin_show(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.stage = Stage::Show(Showing::default());
        self.code = empty_code();
        self.started();
        true
    }

    /// The user clicked "Enter a code": the form opens.
    pub fn open_join(&mut self) {
        if !self.busy && !matches!(self.stage, Stage::Join(_)) {
            self.stage = Stage::Join(Joining::default());
            self.outcome = None;
        }
    }

    /// The user clicked Cancel on the form while nothing runs: it closes,
    /// and the code typed into it is wiped (it may still work on the other
    /// computer for a while).
    pub fn close_join(&mut self) {
        if !self.busy {
            self.stage = Stage::Choose;
            self.outcome = None;
            self.code = empty_code();
        }
    }

    /// Whether the form's Pair button can be clicked.
    pub fn can_join(&self) -> bool {
        !self.busy && !self.code.trim().is_empty()
    }

    /// The user clicked Pair on the form: the role to start the session
    /// with, holding a copy of the code (wiped when the session drops it).
    /// `None` while a session runs or with no code typed.
    pub fn begin_join(&mut self) -> Option<PairRole> {
        if !self.can_join() {
            return None;
        }
        self.stage = Stage::Join(Joining::default());
        self.started();
        Some(PairRole::Join {
            code: Zeroizing::new(self.code.as_str().to_owned()),
            address: Some(self.address.clone()),
        })
    }

    fn started(&mut self) {
        self.busy = true;
        self.cancelling = false;
        self.outcome = None;
        self.unconfirmed = None;
    }

    /// The user clicked Cancel while a session runs. True when the app must
    /// cancel it now.
    pub fn cancel(&mut self) -> bool {
        if self.busy && !self.cancelling {
            self.cancelling = true;
            true
        } else {
            false
        }
    }

    /// The user answered the question at `now`. True when the answer must
    /// go to the session: only once, and only while the question is open.
    /// Once its time is up, no answer counts (the flow has taken it as No),
    /// and the question stays until the session's end arrives.
    pub fn answer(&mut self, yes: bool, now: Instant) -> bool {
        let Stage::Show(showing) = &mut self.stage else {
            return false;
        };
        if showing
            .question
            .as_ref()
            .is_none_or(|question| now >= question.deadline)
        {
            return false;
        }
        let Some(question) = showing.question.take() else {
            return false;
        };
        if yes {
            showing.accepted = Some(question.name);
        }
        true
    }

    /// Whether the window needs repainting every so often: a code's time
    /// left or the question's countdown is on screen.
    pub fn is_counting_down(&self) -> bool {
        matches!(&self.stage, Stage::Show(showing) if showing.expires.is_some())
    }

    /// The session could not even be started.
    pub fn failed_to_start(&mut self, why: &str, activity: &mut Activity) {
        self.busy = false;
        self.cancelling = false;
        if let Stage::Show(_) = self.stage {
            self.stage = Stage::Choose;
        }
        let line = words::not_paired(why);
        activity.push(line.clone());
        self.outcome = Some(Outcome {
            paired: false,
            lines: vec![line],
        });
    }

    /// Applies an event of the running session, adding its progress to
    /// `activity` (never the code, and never written to the log). Returns
    /// what was saved when the session paired this computer.
    pub fn apply(
        &mut self,
        event: PairEvent,
        now: Instant,
        activity: &mut Activity,
    ) -> Option<Saved> {
        match event {
            PairEvent::Code(code) => {
                if let Stage::Show(showing) = &mut self.stage {
                    showing.code = Some(code);
                    showing.expires = Some(now + words::CODE_LIFETIME);
                    activity.push(words::SHOWING_CODE);
                }
            }
            PairEvent::NotAnnounced { reason } => {
                let line = words::not_announced(&reason);
                activity.push(line.clone());
                if let Stage::Show(showing) = &mut self.stage {
                    showing.not_announced = Some(line);
                }
            }
            PairEvent::WrongCode { failures } => {
                activity.push(words::wrong_code(failures));
                if let Stage::Show(showing) = &mut self.stage {
                    showing.wrong = failures;
                }
            }
            PairEvent::Confirm {
                name,
                os,
                replaces,
                within,
            } => {
                activity.push(words::entered_the_code(&name, os));
                if let Stage::Show(showing) = &mut self.stage {
                    showing.question = Some(Question {
                        name,
                        os,
                        replaces,
                        deadline: now + within,
                    });
                }
            }
            PairEvent::Searching => self.progress(words::LOOKING.to_owned(), activity),
            PairEvent::Connecting => self.progress(words::CONNECTING.to_owned(), activity),
            PairEvent::WaitingForConfirmation { host } => {
                self.progress(words::click_yes_on(&host), activity);
            }
            PairEvent::NoHostFound => self.problem(words::NO_HOST.to_owned(), activity),
            PairEvent::SeveralHosts(hosts) => {
                self.problem(words::several_hosts(&hosts), activity);
            }
            PairEvent::Unconfirmed { reason } => self.unconfirmed = Some(reason),
            PairEvent::Finished(result) => return self.finish(result, activity),
        }
        None
    }

    fn progress(&mut self, line: String, activity: &mut Activity) {
        activity.push(line.clone());
        if let Stage::Join(joining) = &mut self.stage {
            joining.progress = Some(line);
        }
    }

    fn problem(&mut self, line: String, activity: &mut Activity) {
        activity.push(line.clone());
        if let Stage::Join(joining) = &mut self.stage {
            joining.progress = None;
            joining.problem = Some(line);
        }
    }

    /// The session ended: say how, and drop what it showed (the code is
    /// wiped as it goes).
    fn finish(&mut self, result: Result<Saved, String>, activity: &mut Activity) -> Option<Saved> {
        let cancelled = std::mem::take(&mut self.cancelling);
        self.busy = false;
        let unconfirmed = self.unconfirmed.take();
        match result {
            Ok(saved) => {
                let mut lines = vec![words::paired(&saved.name, saved.side)];
                if let Replaced::OldKey(old) = &saved.replaced {
                    lines.push(words::replaced_old_key(old));
                }
                if let Some(reason) = unconfirmed {
                    lines.push(words::however(&reason));
                }
                for line in &lines {
                    activity.push(line.clone());
                }
                self.outcome = Some(Outcome {
                    paired: true,
                    lines,
                });
                self.stage = Stage::Choose;
                // The typed code did its job; wipe it.
                self.code = empty_code();
                Some(saved)
            }
            Err(reason) => {
                let line = if cancelled {
                    words::PAIRING_CANCELLED.to_owned()
                } else {
                    words::not_paired(&reason)
                };
                activity.push(line.clone());
                self.outcome = Some(Outcome {
                    paired: false,
                    lines: vec![line],
                });
                match &mut self.stage {
                    // The form stays, so the user can fix the address and
                    // try again with the same code.
                    Stage::Join(joining) => joining.progress = None,
                    Stage::Show(_) | Stage::Choose => self.stage = Stage::Choose,
                }
                None
            }
        }
    }
}

/// How long is left until `deadline`.
pub fn left(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}
