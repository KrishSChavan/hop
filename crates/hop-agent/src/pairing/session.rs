//! [`PairSession`]: the pairing flow on a thread of its own, for the app.

use std::future::Future;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use hop_net::PairError;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use zeroize::Zeroizing;

use super::{Front, Outcome, PairEvent, PairRole, Question, Saved, Setup, pair, runtime};
use crate::{startup, status};

/// The network runtime's threads get this long to finish after the flow.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// [`PairEvent::Finished`] after [`PairSession::cancel`].
pub(crate) const CANCELLED: &str = "cancelled";

/// [`PairEvent::Finished`] after the user said no on this computer.
pub(crate) const DECLINED: &str = "nothing was saved on either computer";

/// [`PairEvent::Finished`] when the question went unanswered.
pub(crate) const NO_ANSWER: &str = "no answer in time, so nothing was saved on either computer";

/// [`PairEvent::Finished`] after a panic. Fixed on purpose: a panic's own
/// message could quote input.
pub(crate) const PANICKED: &str = "Hop hit an internal error while pairing (the log says where)";

/// [`PairEvent::Unconfirmed`] when the session ended between saving here
/// and the other computer hearing the yes.
const ENDED_AFTER_SAVING: &str = "this computer saved the pairing, but pairing stopped before \
                                  the other computer heard the yes; if it reports a failure, pair \
                                  again on both computers";

/// [`PairEvent::Unconfirmed`] when this computer saved the pairing but could
/// not make sure the other computer heard the yes.
const NOT_HEARD: &str = "this computer saved the pairing, but the other computer may not have \
                         heard the yes; if it reports a failure, pair again on both computers";

/// A pairing run on its own thread, `hop-pair`, with a tokio runtime of two
/// workers: what `hop pair` does, without a terminal. Events go to the
/// channel given to [`PairSession::start`], and [`PairEvent::Finished`] is
/// always the last one.
///
/// Dropping the session cancels it, as [`PairSession::cancel`] does.
#[derive(Debug)]
pub struct PairSession {
    thread: Option<JoinHandle<()>>,
    cancel: Arc<Notify>,
    answers: UnboundedSender<bool>,
}

impl PairSession {
    /// Starts pairing as `role` says, on a new thread. It loads the config
    /// and the key first (making them on a first run), then:
    ///
    /// - **Show:** listens on the pairing port (`network.port + 1`),
    ///   announces this computer on the local network, and sends
    ///   [`PairEvent::Code`] (after [`PairEvent::NotAnnounced`] if the
    ///   announcement failed); then [`PairEvent::WrongCode`] for each wrong
    ///   try, and [`PairEvent::Confirm`] when the other computer typed the
    ///   right code. Answer that with [`PairSession::answer`].
    /// - **Join:** looks for the computer showing the code
    ///   ([`PairEvent::Searching`]), or uses the address given, then
    ///   [`PairEvent::Connecting`] and
    ///   [`PairEvent::WaitingForConfirmation`]. The code and the address may
    ///   have spaces around them; an empty address means "look for it".
    ///
    /// Either way, [`PairEvent::Finished`] comes last, with what was saved
    /// or why nothing was.
    ///
    /// # Errors
    ///
    /// The thread could not start. Nothing ran then, and no event is sent.
    pub fn start(role: PairRole, events: Sender<PairEvent>) -> io::Result<PairSession> {
        PairSession::spawn(role, events, || Setup::new(startup::load()?))
    }

    /// [`PairSession::start`] with the setup made by `setup` on the
    /// session's thread: tests pair over loopback with files of their own.
    pub(crate) fn spawn(
        role: PairRole,
        events: Sender<PairEvent>,
        setup: impl FnOnce() -> Result<Setup> + Send + 'static,
    ) -> io::Result<PairSession> {
        let role = tidy(role);
        let cancel = Arc::new(Notify::new());
        let (answers, answered) = unbounded_channel();
        let thread_cancel = cancel.clone();
        let thread = thread::Builder::new()
            .name("hop-pair".into())
            .spawn(move || run(role, events, answered, &thread_cancel, setup))?;
        Ok(PairSession {
            thread: Some(thread),
            cancel,
            answers,
        })
    }

    /// Answers [`PairEvent::Confirm`]: true pairs, false declines. An answer
    /// given before the question is asked does not count.
    pub fn answer(&self, yes: bool) {
        let _ = self.answers.send(yes);
    }

    /// Stops pairing at once: the listener or the connection closes, nothing
    /// more is saved, and [`PairEvent::Finished`] follows. Returns at once.
    pub fn cancel(&self) {
        self.cancel.notify_one();
    }

    /// Whether the session's thread has ended (after
    /// [`PairEvent::Finished`]).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for PairSession {
    /// Nobody could answer or cancel any more, so the session ends.
    fn drop(&mut self) {
        self.cancel.notify_one();
    }
}

/// What the app's fields hold, as the flow reads it: the code and the
/// address without the spaces around them (a pasted code may end in a line
/// break), and no address when it is empty, which means "look for it".
pub(crate) fn tidy(role: PairRole) -> PairRole {
    match role {
        PairRole::Show => PairRole::Show,
        PairRole::Join { code, address } => PairRole::Join {
            // The untrimmed copy is wiped as it is dropped here.
            code: Zeroizing::new(code.trim().to_owned()),
            address: address
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty()),
        },
    }
}

/// How a session ended.
enum Ending {
    /// The flow ran to its end, or could not start.
    Done(Result<Outcome>),
    Cancelled,
    Panicked,
}

/// The session's thread: the flow, then the last events.
fn run(
    role: PairRole,
    events: Sender<PairEvent>,
    answers: UnboundedReceiver<bool>,
    cancel: &Notify,
    setup: impl FnOnce() -> Result<Setup>,
) {
    let joining = matches!(role, PairRole::Join { .. });
    let mut front = AppFront {
        events: events.clone(),
        answers,
        saved: None,
        unanswered: false,
    };
    let ending = panic::catch_unwind(AssertUnwindSafe(|| {
        session(role, &mut front, cancel, setup)
    }))
    .unwrap_or(Ending::Panicked);
    for event in last_events(ending, &mut front, joining) {
        let _ = events.send(event);
    }
}

/// Sets up and runs the flow until it ends or is cancelled.
fn session(
    role: PairRole,
    front: &mut AppFront,
    cancel: &Notify,
    setup: impl FnOnce() -> Result<Setup>,
) -> Ending {
    let setup = match setup() {
        Ok(setup) => setup,
        Err(error) => return Ending::Done(Err(error)),
    };
    for note in &setup.startup.notes {
        status!("{note}");
    }
    let runtime = match runtime() {
        Ok(runtime) => Bounded(Some(runtime)),
        Err(error) => return Ending::Done(Err(error)),
    };
    let ending = runtime.block_on(async {
        tokio::select! {
            // Cancel first: a cancel that comes before the flow starts stops
            // it before it shows a code or connects.
            biased;
            () = cancel.notified() => Ending::Cancelled,
            outcome = pair(&setup, role, &mut *front) => Ending::Done(outcome),
        }
    });
    // The listener, any connection and the announcement went with the flow;
    // stopping the runtime also lets go of the port, so a new session can
    // listen on it as soon as this one has finished.
    drop(runtime);
    ending
}

/// The session's runtime, stopped within [`RUNTIME_SHUTDOWN`] however the
/// session ends, a panic included: a name lookup that hangs on one of its
/// threads must not hold up [`PairEvent::Finished`].
struct Bounded(Option<tokio::runtime::Runtime>);

impl Bounded {
    fn block_on(&self, future: impl Future<Output = Ending>) -> Ending {
        match &self.0 {
            Some(runtime) => runtime.block_on(future),
            None => Ending::Done(Err(anyhow::anyhow!("the network runtime is gone"))),
        }
    }
}

impl Drop for Bounded {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        }
    }
}

/// The events that end a session: [`PairEvent::Finished`], after
/// [`PairEvent::Unconfirmed`] when the other computer may not know.
/// `joining` says which side this computer took, for the wording.
fn last_events(ending: Ending, front: &mut AppFront, joining: bool) -> Vec<PairEvent> {
    let unconfirmed = |reason: String, saved: Saved| {
        vec![
            PairEvent::Unconfirmed { reason },
            PairEvent::Finished(Ok(saved)),
        ]
    };
    // Ended before the flow did: maybe right after saving here.
    let ended_early = |front: &mut AppFront, why: &str| match front.saved.take() {
        Some(saved) => unconfirmed(ENDED_AFTER_SAVING.to_owned(), saved),
        None => vec![PairEvent::Finished(Err(why.to_owned()))],
    };
    let result = match ending {
        Ending::Done(Ok(outcome)) => match outcome {
            Outcome::Paired(saved) => Ok(saved),
            // Whatever went wrong after the save, it means the same here.
            Outcome::SavedHereOnly { saved, .. } => {
                return unconfirmed(NOT_HEARD.to_owned(), saved);
            }
            Outcome::Declined if front.unanswered => Err(NO_ANSWER.to_owned()),
            Outcome::Declined => Err(DECLINED.to_owned()),
            Outcome::Failed(error) => Err(app_reason(&error, joining)),
            Outcome::NothingRecorded { name } => Err(format!(
                "{name} said yes, but nothing was saved on this computer; pair again on both \
                 computers"
            )),
            Outcome::NotSaved { name, error } => Err(format!(
                "{name} said yes, but this computer could not save the pairing: {error}. Pair \
                 again on both computers"
            )),
            Outcome::NoHostFound => Err("could not find a computer showing a pairing code. \
                                         Check that the other computer shows one, or enter its \
                                         address"
                .to_owned()),
            Outcome::SeveralHosts => Err("more than one computer is showing a pairing code. \
                                          Enter the address of the one you mean"
                .to_owned()),
        },
        // A pairing error that stopped the flow early (the pairing port is
        // taken, say) is worded like the others; anything else says what
        // went wrong, with its causes.
        Ending::Done(Err(error)) => Err(match error.downcast_ref::<PairError>() {
            Some(error) => app_reason(error, joining),
            None => format!("{error:#}"),
        }),
        Ending::Cancelled => return ended_early(front, CANCELLED),
        Ending::Panicked => return ended_early(front, PANICKED),
    };
    vec![PairEvent::Finished(result)]
}

/// Why pairing failed, in the app's words. hop-net's own sentences are the
/// terminal's: they tell the user to run `hop pair` again, and call the
/// computers laptops. `joining` says which side this computer took.
pub(crate) fn app_reason(error: &PairError, joining: bool) -> String {
    match error {
        PairError::WrongCode => "the code did not match. Check it against the code the other \
                                 computer shows, and click Pair again"
            .to_owned(),
        PairError::Expired if joining => "the code no longer works: it expired, or the other \
                                          computer stopped showing it. Show a new code there \
                                          and enter it here"
            .to_owned(),
        PairError::Expired => "the code's 2 minutes ran out. Click Show a pairing code for a new \
                               one"
        .to_owned(),
        PairError::TooManyAttempts => "3 tries used a wrong code, so this code no longer works. \
                                       Someone may be trying to pair with this computer. Click \
                                       Show a pairing code for a new one"
            .to_owned(),
        PairError::Bind { addr, source } => format!(
            "could not listen on {addr} for pairing ({source}). Is another copy of Hop, or hop \
             pair, already pairing on this computer?"
        ),
        PairError::Busy => {
            "the other computer is busy pairing with another one. Try again in a moment".to_owned()
        }
        PairError::Declined => {
            "the other computer did not say yes, so nothing was saved".to_owned()
        }
        PairError::Unreachable => "could not reach the other computer. It may refuse connections \
                                   that other computers start (a work Mac can), so try it the \
                                   other way round: Show a pairing code on this computer, and \
                                   Enter a code on the other"
            .to_owned(),
        PairError::Connection(why) => {
            format!("the connection to the other computer was lost ({why})")
        }
        PairError::SelfPairing => "that is this computer itself".to_owned(),
        // The others say nothing about the terminal or laptops.
        PairError::InvalidCode
        | PairError::Random
        | PairError::Setup(_)
        | PairError::NoAddress
        | PairError::Protocol(_)
        | PairError::Save(_)
        | PairError::Discovery(_) => error.to_string(),
    }
}

/// The app's front end: events go to the app's channel, answers come from
/// [`PairSession::answer`].
struct AppFront {
    events: Sender<PairEvent>,
    answers: UnboundedReceiver<bool>,
    /// What the flow saved here, in case the session ends before the flow
    /// says so.
    saved: Option<Saved>,
    /// The question went unanswered.
    unanswered: bool,
}

impl Front for AppFront {
    fn event(&mut self, event: PairEvent) {
        // A gone app is no error: the session runs to its end regardless.
        let _ = self.events.send(event);
    }

    async fn confirm(&mut self, question: Question) -> bool {
        // An answer given before this question (a stray click) does not
        // count.
        while self.answers.try_recv().is_ok() {}
        let within = question.within;
        self.event(PairEvent::Confirm {
            name: question.name,
            os: question.os,
            replaces: question.replaces,
            within,
        });
        match tokio::time::timeout(within, self.answers.recv()).await {
            Ok(Some(yes)) => yes,
            // The session's handle is gone, so nobody can answer.
            Ok(None) => false,
            Err(_) => {
                self.unanswered = true;
                false
            }
        }
    }

    fn saved(&mut self, saved: &Saved) {
        self.saved = Some(saved.clone());
    }
}
