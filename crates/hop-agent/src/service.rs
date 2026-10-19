//! The agent on a thread of its own, for an app (docs/M6-APP.md, "APIs in
//! hop-agent").
//!
//! [`AgentHandle::start`] runs exactly what `hop run` runs, on a thread
//! named `hop-agent`, and [`AgentHandle::stop`] ends it the way Ctrl+C ends
//! `hop run`: input comes back first, then the network says goodbye. What
//! happens on the way arrives as [`AgentUpdate`]s, ending with
//! [`AgentUpdate::Stopped`].
//!
//! ```no_run
//! use std::sync::mpsc;
//! use std::time::Duration;
//! use hop_agent::service::{AgentHandle, AgentUpdate};
//!
//! # fn main() -> std::io::Result<()> {
//! hop_agent::service::install_panic_hook(None);
//! let (tx, updates) = mpsc::channel();
//! let agent = AgentHandle::start(tx)?;
//! // A bridge thread drains `updates` into the app's state and repaints.
//! std::thread::spawn(move || {
//!     for update in updates {
//!         if let AgentUpdate::Stopped { error } = update {
//!             println!("stopped: {error:?}");
//!         }
//!     }
//! });
//! // On Stop sharing, or when the window closes:
//! agent.stop();
//! agent.wait(Duration::from_secs(3));
//! # Ok(())
//! # }
//! ```
//!
//! The guarantees:
//!
//! - The core thread only ever `send`s on the unbounded channel the app
//!   gives it, so it never waits for the app.
//! - [`AgentUpdate::Stopped`] is always the last update, even when the
//!   agent could not start or its thread panicked.
//! - File names reach the app only in [`AgentUpdate::FilesReceived`], to
//!   show; the agent never logs them, and the app must not either.
//! - A panic anywhere in the app asks the running agent to quit failing
//!   open, once [`install_panic_hook`] is in place. A panic on the main
//!   thread also waits (at most 3 seconds) for it to have stopped.
//! - [`on_quit_signal`] lets the app close its window the usual way when
//!   the OS asks it to quit (SIGTERM, Ctrl+C, SIGHUP on a Mac).

use std::io::{self, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hop_config::Side;
use hop_types::Os;

use crate::agent::Signals;
use crate::agent::start::{self, Host};
use crate::logging::Log;
use crate::startup;

/// What [`AgentUpdate::Stopped`] says when the agent ended because some
/// thread panicked. Fixed on purpose: a panic's own message could quote
/// input.
pub const PANICKED: &str = "Hop hit an internal error, so it gave the keyboard and mouse back \
                            and stopped sharing (the log says where)";

/// What the app's panic hook says happens next.
const AFTER_PANIC: &str = "Giving the keyboard and mouse back and stopping sharing.";

/// How often [`AgentHandle::wait`] looks whether the thread has ended.
const WAIT_POLL: Duration = Duration::from_millis(5);

/// How long a panic on the app's main thread waits for the agents to stop,
/// failing open, before the panic goes on (the same 3 seconds closing the
/// window waits).
const MAIN_THREAD_WAIT: Duration = Duration::from_secs(3);

/// How long the log then gets to write the report and the agents' last
/// lines.
const MAIN_THREAD_FLUSH: Duration = Duration::from_secs(1);

/// What a session looks like from this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    /// This computer's keyboard and mouse work here, and the other
    /// computer's work there.
    Idle,
    /// This computer's keyboard and mouse control the other computer (from
    /// the moment the cursor crosses).
    Controlling,
    /// The other computer's keyboard and mouse control this one.
    Controlled,
}

/// What the agent tells the app, in order. Text in here is plain sentences
/// for the user; none of it is ever a key, clipboard text or a code.
///
/// One update carries file names, [`AgentUpdate::FilesReceived`], for the
/// app to show. Names never go in the log, so never log an update (its
/// `Debug` included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentUpdate {
    /// The agent is running: it has the network and watches the keyboard
    /// and mouse. Always the first update of a run that got this far.
    Started {
        /// The paired computer it shares with.
        peer: String,
        /// Where that computer sits.
        side: Side,
        /// How to end a session at once from this keyboard, as one
        /// sentence ("If the keyboard and mouse ever seem stuck…").
        emergency_keys: String,
    },
    /// Connected to the other computer.
    Connected {
        /// The name it gave.
        name: String,
        /// Its OS.
        os: Os,
    },
    /// The connection closed; the agent keeps trying to reconnect.
    Disconnected,
    /// The session kind changed. Sent only on a change.
    Session(Session),
    /// Files from the other computer arrived complete in the Received
    /// folder, which opens with them. Separate from the session: it says
    /// nothing about who uses which keyboard and mouse.
    ///
    /// This is the one update with file names in it. The app shows them;
    /// they must never be logged (the status line about the same files only
    /// counts them).
    FilesReceived {
        /// The computer that sent them, as it calls itself.
        from: String,
        /// The name each file was saved under, in the order they were sent.
        names: Vec<String>,
    },
    /// The agent stopped, failing open: input is back on both computers.
    /// Always the last update.
    Stopped {
        /// Why it stopped, when not asked to: why it could not start (not
        /// paired, the port in use, no Accessibility permission…), with the
        /// whole chain of causes, or [`PANICKED`].
        error: Option<String>,
    },
}

/// The agent running on its own thread. Dropping the handle asks the agent
/// to quit, as [`AgentHandle::stop`] does, without waiting.
#[derive(Debug)]
pub struct AgentHandle {
    signals: Arc<Signals>,
    thread: Option<JoinHandle<()>>,
}

impl AgentHandle {
    /// Starts the agent on a new thread, `hop-agent`, running the same
    /// sequence as `hop run`: load the config and the key (making them on a
    /// first run), start the network, check the Accessibility permission
    /// (asking for it on a Mac if it is missing), start capture, and share
    /// until stopped. Updates go to `updates`, and the last one is always
    /// [`AgentUpdate::Stopped`].
    ///
    /// The app keeps its own log and panic hook ([`install_panic_hook`]),
    /// and on a Mac holds [`crate::system::stay_responsive`] on the main
    /// thread while sharing.
    ///
    /// # Errors
    ///
    /// The thread could not start. Nothing ran then, and no update is sent.
    pub fn start(updates: Sender<AgentUpdate>) -> io::Result<AgentHandle> {
        AgentHandle::spawn(updates, |signals, updates| {
            start::run(signals, Host::App(updates), startup::load).map(drop)
        })
    }

    /// The `hop-agent` thread around `run`, which gets the agent's signals
    /// and its updates: tests run fakes in it.
    pub(crate) fn spawn(
        updates: Sender<AgentUpdate>,
        run: impl FnOnce(&Arc<Signals>, Sender<AgentUpdate>) -> anyhow::Result<()> + Send + 'static,
    ) -> io::Result<AgentHandle> {
        let signals = Arc::new(Signals::default());
        RUNNING.add(&signals);
        let thread_signals = signals.clone();
        let spawned = thread::Builder::new()
            .name("hop-agent".into())
            .spawn(move || {
                // This thread is the core thread: everything that rings,
                // rings it.
                thread_signals.attach();
                let mut stopped = StoppedOnDrop {
                    updates: updates.clone(),
                    signals: thread_signals.clone(),
                    error: None,
                };
                // A panic has given input back by the time it is caught:
                // unwinding dropped the agent and its platform. `stopped`
                // then reports it.
                let outcome =
                    panic::catch_unwind(AssertUnwindSafe(|| run(&thread_signals, updates)));
                stopped.error = match outcome {
                    Ok(Ok(())) if thread_signals.panicked() => Some(PANICKED.to_owned()),
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => {
                        tracing::error!(error = %format!("{error:#}"), "Hop stopped");
                        Some(format!("{error:#}"))
                    }
                    Err(_) => Some(PANICKED.to_owned()),
                };
            });
        match spawned {
            Ok(thread) => Ok(AgentHandle {
                signals,
                thread: Some(thread),
            }),
            Err(error) => {
                RUNNING.remove(&signals);
                Err(error)
            }
        }
    }

    /// Asks the agent to quit, failing open: input comes back first, then
    /// the network says goodbye, then [`AgentUpdate::Stopped`] follows.
    /// Returns at once. Asked while the agent is still starting, it quits
    /// before capture starts.
    pub fn stop(&self) {
        self.signals.request_quit();
    }

    /// Whether the agent's thread has ended.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits up to `timeout` for the agent's thread to end (call
    /// [`AgentHandle::stop`] first). Returns true when it ended in time;
    /// otherwise the thread finishes on its own.
    pub fn wait(mut self, timeout: Duration) -> bool {
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(WAIT_POLL);
        }
        // A panic on the thread was reported as Stopped already.
        let _ = thread.join();
        true
    }

    /// The agent's signals, for tests.
    #[cfg(test)]
    pub(crate) fn signals(&self) -> &Arc<Signals> {
        &self.signals
    }
}

impl Drop for AgentHandle {
    /// A handle nobody holds can never stop the agent, so dropping it does.
    fn drop(&mut self) {
        self.signals.request_quit();
    }
}

/// Sends [`AgentUpdate::Stopped`] when the agent's thread ends, however it
/// ends, and takes the agent off the panic hook's list.
struct StoppedOnDrop {
    updates: Sender<AgentUpdate>,
    signals: Arc<Signals>,
    error: Option<String>,
}

impl Drop for StoppedOnDrop {
    fn drop(&mut self) {
        RUNNING.remove(&self.signals);
        let error = if thread::panicking() {
            Some(PANICKED.to_owned())
        } else {
            self.error.take()
        };
        let _ = self.updates.send(AgentUpdate::Stopped { error });
    }
}

/// The agents running in this process, for the panic hook. An app runs one
/// at a time; while a restart overlaps, the old one is still listed until
/// its thread ends.
pub(crate) struct Registry(Mutex<Vec<Arc<Signals>>>);

/// The agents [`AgentHandle`] started that are still running.
pub(crate) static RUNNING: Registry = Registry::new();

impl Registry {
    pub(crate) const fn new() -> Registry {
        Registry(Mutex::new(Vec::new()))
    }

    pub(crate) fn add(&self, signals: &Arc<Signals>) {
        self.lock().push(signals.clone());
    }

    pub(crate) fn remove(&self, signals: &Arc<Signals>) {
        self.lock().retain(|listed| !Arc::ptr_eq(listed, signals));
    }

    /// Whether `signals` is listed.
    #[cfg(test)]
    pub(crate) fn contains(&self, signals: &Arc<Signals>) -> bool {
        self.lock()
            .iter()
            .any(|listed| Arc::ptr_eq(listed, signals))
    }

    /// Asks every listed agent to quit, failing open, and to report the
    /// panic. Never blocks: if the list is busy at this instant (only for
    /// the moment it takes to add or remove an agent), the agent's own
    /// thread still fails open if it was the one that panicked.
    pub(crate) fn panicked(&self) {
        let listed = match self.0.try_lock() {
            Ok(listed) => listed,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        for signals in listed.iter() {
            signals.mark_panicked();
            signals.request_quit();
        }
    }

    /// Waits up to `timeout` until no agent is listed: each one leaves the
    /// list once it has given input back and said goodbye. Returns whether
    /// the list emptied in time. Only ever tries the lock, so it cannot
    /// deadlock on a thread that holds it; a busy list is just tried again.
    pub(crate) fn wait_until_empty(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let empty = match self.0.try_lock() {
                Ok(listed) => listed.is_empty(),
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().is_empty(),
                Err(TryLockError::WouldBlock) => false,
            };
            if empty {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(WAIT_POLL);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Signals>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Makes any panic, on any thread of the app, end sharing failing open, as
/// `hop run`'s hook does (docs/M2-DESIGN.md, "Panics"): the running agent
/// is asked to quit, and its [`AgentUpdate::Stopped`] says
/// [`PANICKED`]. Install it once, early, with the app's log.
///
/// The hook writes the thread and the place to `log` (or stderr), and the
/// panic's message only when it is a fixed string: a formatted message
/// could quote input. It replaces the default hook, so nothing else is
/// printed.
///
/// Install it on the app's main thread, the one that runs the window. On
/// every other thread the hook never blocks. On the main thread it waits up
/// to 3 seconds for the agents to stop, then up to 1 second for the log:
/// a panic there may never unwind. On a Mac, AppKit calls the window's code
/// through functions a panic cannot unwind out of, so the process aborts
/// right after the hook, and nothing else would stop sharing first. The
/// wait cannot deadlock: an agent never waits for the main thread.
pub fn install_panic_hook(log: Option<Log>) {
    let main = thread::current().id();
    panic::set_hook(Box::new(move |info| {
        report_panic(
            &start::panic_report(info, AFTER_PANIC),
            log.as_ref(),
            &RUNNING,
        );
        if thread::current().id() == main {
            let stopped = RUNNING.wait_until_empty(MAIN_THREAD_WAIT);
            if let Some(log) = &log {
                if !stopped {
                    log.write_status("Sharing had not fully stopped when Hop gave up waiting.");
                }
                log.flush(MAIN_THREAD_FLUSH);
            }
        }
    }));
}

/// The app's panic hook, once it has `text`: write it, then ask every agent
/// in `running` to quit.
fn report_panic(text: &str, log: Option<&Log>, running: &Registry) {
    match log {
        Some(log) => log.write_status(text),
        // An app may have no stderr; a failed write must not panic here.
        None => {
            let _ = writeln!(io::stderr(), "{text}");
        }
    }
    running.panicked();
}

/// Calls `on_quit` once, on a thread of its own, when the OS first asks the
/// app to quit: on a Mac, SIGTERM (`kill`), SIGINT (Ctrl+C in the terminal
/// that started it) or SIGHUP (that terminal closing). The app answers by
/// closing its window, which stops sharing failing open. A second request
/// exits at once, as `hop run` does: the OS then removes Hop's event tap,
/// and the other computer lets go of what it injected within 1.5 s.
///
/// Elsewhere it starts nothing: a windowed program on Windows gets none of
/// the console's events, and signing out or shutting down ends it without
/// them.
///
/// # Errors
///
/// The thread or its runtime could not start. Signals then end the app the
/// OS's way.
pub fn on_quit_signal(on_quit: impl FnOnce() + Send + 'static) -> io::Result<()> {
    #[cfg(unix)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        thread::Builder::new()
            .name("hop-app-signals".into())
            .spawn(move || {
                runtime.block_on(async {
                    let why = start::next_quit_signal().await;
                    tracing::info!(signal = why, "asked to quit");
                    on_quit();
                    let _ = start::next_quit_signal().await;
                    tracing::warn!("asked to quit again; exiting at once");
                    std::process::exit(130);
                });
            })?;
    }
    #[cfg(not(unix))]
    drop(on_quit);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::logging::Destination;

    #[test]
    fn the_panic_hook_asks_every_running_agent_to_quit_and_logs_no_formatted_text() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let log = Log::start(Destination::Memory(lines.clone())).expect("log");
        // A list of its own: the process-wide one belongs to other tests'
        // agents, which must not be stopped by this test's panics.
        let running: &'static Registry = Box::leak(Box::new(Registry::new()));
        let (first, second) = (Arc::new(Signals::default()), Arc::new(Signals::default()));
        running.add(&first);
        running.add(&second);
        running.remove(&second);

        let _one_hook_at_a_time = crate::PANIC_HOOK_TESTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = panic::take_hook();
        let hook_log = log.clone();
        panic::set_hook(Box::new(move |info| {
            report_panic(
                &start::panic_report(info, AFTER_PANIC),
                Some(&hook_log),
                running,
            );
        }));
        let secret = String::from("hunter2");
        let panicked = thread::Builder::new()
            .name("hop-app-bridge".into())
            .spawn(move || panic!("typed {secret}"))
            .expect("thread")
            .join();
        panic::set_hook(previous);
        assert!(panicked.is_err());
        assert!(
            first.quit_requested() && first.panicked(),
            "the agent quits"
        );
        assert!(
            !second.quit_requested(),
            "an agent whose thread ended is off the list"
        );

        log.flush(Duration::from_secs(5));
        let text = lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .join("\n");
        assert!(text.contains("thread hop-app-bridge"), "{text}");
        assert!(text.contains("its message is not logged"), "{text}");
        assert!(text.contains("stopping sharing"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn a_panic_on_the_main_thread_waits_until_every_agent_has_stopped() {
        let running = Arc::new(Registry::new());
        let signals = Arc::new(Signals::default());
        running.add(&signals);
        assert!(
            !running.wait_until_empty(Duration::from_millis(50)),
            "an agent is still listed"
        );
        // The agent leaves the list once it has failed open, a little later.
        let leaving = {
            let (running, signals) = (running.clone(), signals.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                running.remove(&signals);
            })
        };
        let started = Instant::now();
        assert!(running.wait_until_empty(Duration::from_secs(10)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no longer than needed"
        );
        leaving.join().expect("thread");
        // A busy list (held here by this very thread, as when a panic struck
        // while adding an agent) is only ever tried, never waited on.
        running.add(&signals);
        let held = running.lock();
        assert!(!running.wait_until_empty(Duration::from_millis(50)));
        drop(held);
    }

    #[test]
    fn a_busy_list_never_blocks_the_hook() {
        let running = Registry::new();
        let signals = Arc::new(Signals::default());
        running.add(&signals);
        let held = running.lock();
        // Would deadlock with a plain lock: the hook may run on a thread
        // that holds the list.
        running.panicked();
        drop(held);
        assert!(!signals.quit_requested());
        running.panicked();
        assert!(signals.quit_requested());
    }

    #[test]
    fn the_handle_lists_its_agent_until_the_thread_ends() {
        let (tx, updates) = mpsc::channel();
        let handle = AgentHandle::spawn(tx, |signals, _| {
            while !signals.quit_requested() {
                thread::park_timeout(Duration::from_millis(10));
            }
            Ok(())
        })
        .expect("thread");
        let signals = handle.signals().clone();
        assert!(RUNNING.contains(&signals));
        assert!(!handle.is_finished());
        handle.stop();
        assert!(handle.wait(Duration::from_secs(5)), "the thread ended");
        assert!(!RUNNING.contains(&signals));
        assert_eq!(
            updates.try_iter().collect::<Vec<_>>(),
            [AgentUpdate::Stopped { error: None }]
        );
    }

    #[test]
    fn dropping_the_handle_asks_the_agent_to_quit() {
        let (tx, updates) = mpsc::channel();
        let handle = AgentHandle::spawn(tx, |signals, _| {
            while !signals.quit_requested() {
                thread::park_timeout(Duration::from_millis(10));
            }
            Ok(())
        })
        .expect("thread");
        drop(handle);
        assert_eq!(
            updates.recv_timeout(Duration::from_secs(5)),
            Ok(AgentUpdate::Stopped { error: None })
        );
    }
}
