//! The app behind the window: the agent and the pairing sessions it runs,
//! and carrying out what the user asks for.
//!
//! Every frame starts with [`HopApp::step`]: it applies what arrived from
//! the other threads (status lines, the agent's updates, pairing events),
//! handles the window closing, starts an agent that is waiting for the last
//! one's thread to end, holds the App Nap guard while sharing, and checks
//! the Accessibility permission. Then the view draws, and the clicks it
//! reports are carried out. Nothing here waits for the network: the agent
//! and pairing run on threads of their own, and the window only ever asks
//! them to stop.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ViewportCommand};
use hop_agent::logging::Log;
use hop_agent::pairing::{PairEvent, PairRole, PairSession};
use hop_agent::service::{AgentHandle, AgentUpdate};
use hop_agent::system::{self, Responsive};
use hop_agent::{CmdCtrl, DeviceId, PermissionState, settings, status};

use crate::bridge::{self, News, Shared};
use crate::model::sharing::Next;
use crate::model::{Model, Settings, words};
use crate::view::{self, Action};

/// Closing the window waits at most this long for sharing and pairing to
/// stop (docs/M6-APP.md, "Closing the window").
const QUIT_WAIT: Duration = Duration::from_secs(3);

/// How often the Accessibility permission is checked, until it is granted.
const PERMISSION_POLL: Duration = Duration::from_secs(2);

/// How often the window looks again while it waits for a thread to end.
const THREAD_POLL: Duration = Duration::from_millis(20);

/// How often the time left on a code is redrawn.
const COUNTDOWN_TICK: Duration = Duration::from_millis(250);

/// How long the log gets to write its last lines.
const LOG_FLUSH: Duration = Duration::from_secs(1);

/// An agent run, numbered so that its updates are never mistaken for the
/// next run's.
struct Agent {
    run: u64,
    handle: AgentHandle,
}

/// A pairing session, numbered like a run.
struct Pair {
    id: u64,
    session: PairSession,
}

/// The app: the model the window draws, and the threads behind it.
pub struct HopApp {
    model: Model,
    shared: Arc<Shared>,
    log: Option<Log>,
    /// The agent while it runs, until it says `Stopped`.
    agent: Option<Agent>,
    /// An agent that said `Stopped`, until its thread has ended: a new one
    /// cannot bind the port before then.
    ending: Option<AgentHandle>,
    /// Start an agent as soon as no other agent's thread is left.
    start_pending: bool,
    /// The pairing session while it runs, until it says `Finished`.
    pair: Option<Pair>,
    /// The number of the next run or session.
    next_id: u64,
    /// macOS's App Nap exemption, held on this (the main) thread while
    /// sharing.
    awake: Option<Responsive>,
    /// When to check the Accessibility permission next.
    next_permission_check: Option<Instant>,
    /// Once the window started closing: the latest it closes.
    quit_by: Option<Instant>,
    /// The window was told to close.
    close_sent: bool,
    /// [`HopApp::shutdown`] ran.
    shut_down: bool,
}

impl HopApp {
    /// The app for `model`, with the queue its news arrives in and the log
    /// to flush on the way out.
    pub fn new(model: Model, shared: Arc<Shared>, log: Option<Log>) -> HopApp {
        HopApp {
            model,
            shared,
            log,
            agent: None,
            ending: None,
            start_pending: false,
            pair: None,
            next_id: 1,
            awake: None,
            next_permission_check: None,
            quit_by: None,
            close_sent: false,
            shut_down: false,
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Everything that happens before a frame is drawn (and while the
    /// window is hidden, when only this runs).
    fn step(&mut self, ctx: &egui::Context, now: Instant) {
        let (lines, news) = self.shared.take();
        for line in lines {
            self.model.activity.push(line);
        }
        for item in news {
            match item {
                News::Agent(run, update) => self.agent_update(run, update),
                News::Pair(id, event) => self.pair_event(id, event, now),
            }
        }
        // Before any start: a window that closes in this frame calls off a
        // restart that was waiting, so no new agent starts only to stop.
        self.handle_close(ctx, now);
        self.start_when_free();
        self.hold_awake();
        self.check_permission(now);
        self.schedule(ctx, now);
    }

    /// An update from the agent: only the running one's count. Never logged:
    /// one kind carries the names of files that arrived.
    fn agent_update(&mut self, run: u64, update: AgentUpdate) {
        if self.agent.as_ref().is_none_or(|agent| agent.run != run) {
            return;
        }
        let stopped = matches!(update, AgentUpdate::Stopped { .. });
        if self.model.agent_update(update) == Next::StartAgain {
            self.start_pending = true;
        }
        if stopped {
            self.ending = self.agent.take().map(|agent| agent.handle);
        }
    }

    /// An event from the pairing session: only the running one's count.
    fn pair_event(&mut self, id: u64, event: PairEvent, now: Instant) {
        if self.pair.as_ref().is_none_or(|pair| pair.id != id) {
            return;
        }
        let finished = matches!(event, PairEvent::Finished(_));
        let question = matches!(event, PairEvent::Confirm { .. });
        let saved = self
            .model
            .pairing
            .apply(event, now, &mut self.model.activity);
        if question && let Some(ctx) = self.shared.window() {
            // The question times out: make sure a hidden window is noticed
            // (the Dock icon bounces, the taskbar button flashes).
            ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
        if finished {
            // Its network threads and listener are gone by now (the session
            // stops them before its last word), and the thread ends right
            // after; dropping the handle only cancels a finished session.
            self.pair = None;
            self.forget_typed_code();
        }
        if let Some(saved) = saved {
            tracing::info!(peer = %saved.name, side = %saved.side, "paired");
            if self.reload_settings() {
                self.restart_sharing();
            }
        }
    }

    /// Drops the Code field's undo history, which holds copies of what was
    /// typed, once the field has been wiped.
    fn forget_typed_code(&self) {
        if self.model.pairing.code.is_empty()
            && let Some(ctx) = self.shared.window()
        {
            egui::TextEdit::store_state(ctx, view::code_field_id(), Default::default());
        }
    }

    /// Starts the agent when one is wanted and no other agent's thread is
    /// left.
    fn start_when_free(&mut self) {
        if !self.start_pending || self.model.quitting || self.agent.is_some() {
            return;
        }
        if let Some(old) = &self.ending {
            if !old.is_finished() {
                return;
            }
            self.ending = None;
        }
        self.start_pending = false;
        let run = self.take_id();
        let (tx, rx) = mpsc::channel();
        let handle = match AgentHandle::start(tx) {
            Ok(handle) => handle,
            Err(error) => {
                self.model
                    .sharing
                    .failed_to_start(format!("could not start sharing ({error})"));
                return;
            }
        };
        let shared = self.shared.clone();
        if let Err(error) = bridge::forward("hop-app-agent", rx, shared, move |update| {
            News::Agent(run, update)
        }) {
            // Nobody would hear it, so it must not run.
            handle.stop();
            self.ending = Some(handle);
            self.model
                .sharing
                .failed_to_start(format!("could not start sharing ({error})"));
            return;
        }
        self.agent = Some(Agent { run, handle });
    }

    /// Asks the agent to stop, after the model said so. An agent still
    /// waiting to start (for the last one's thread to end) never starts.
    fn stop_agent(&mut self) {
        match &self.agent {
            Some(agent) => agent.handle.stop(),
            None => {
                // Nothing runs yet: it has stopped already, and starts again
                // only if a restart wants it.
                let next = self
                    .model
                    .sharing
                    .apply(AgentUpdate::Stopped { error: None });
                self.start_pending = next == Next::StartAgain;
            }
        }
    }

    /// The computers or the Shortcuts setting changed: a running agent
    /// restarts to use them, or stops when nothing is paired any more.
    fn restart_sharing(&mut self) {
        if !self.model.has_paired_peer() {
            if self.model.sharing.stop() {
                self.stop_agent();
            }
            return;
        }
        // An agent that has not started yet reads the new settings anyway.
        if !self.start_pending && self.model.sharing.restart() {
            self.stop_agent();
        }
    }

    /// Reads the settings again. Returns whether the computers or the
    /// Shortcuts setting changed.
    ///
    /// What loading has to say goes to the log and the Activity list, as at
    /// startup: a key made just now (after the old one was deleted, say), or
    /// a new problem with the config. A note the last reading had already
    /// is not repeated.
    fn reload_settings(&mut self) -> bool {
        match settings::overview() {
            Ok(overview) => {
                let said: &[String] = match &self.model.settings {
                    Settings::Loaded(old) => &old.notes,
                    Settings::Failed(_) => &[],
                };
                let fresh: Vec<&String> = overview
                    .notes
                    .iter()
                    .filter(|note| !said.contains(note))
                    .collect();
                for note in fresh {
                    status!("{note}");
                }
                self.model.set_settings(Settings::Loaded(overview))
            }
            Err(error) => {
                let error = format!("{error:#}");
                tracing::error!(error = %error, "could not read the settings");
                self.model.set_settings(Settings::Failed(error))
            }
        }
    }

    /// Saves a change to the computer `id` with `save`, then reads the
    /// settings again and restarts sharing to use them. Returns whether it
    /// was saved. A failure is shown under that computer: the messages
    /// never repeat what was typed, but they stay on screen anyway.
    fn change(
        &mut self,
        id: &DeviceId,
        save: impl FnOnce(&DeviceId) -> anyhow::Result<()>,
    ) -> bool {
        match save(id) {
            Ok(()) => {
                self.model.peers.problem = None;
                if self.reload_settings() {
                    self.restart_sharing();
                }
                true
            }
            Err(error) => {
                let line = words::not_saved(&format!("{error:#}"));
                self.model.activity.push(line.clone());
                self.model.peers.problem = Some((id.clone(), line));
                false
            }
        }
    }

    /// Saves the Shortcuts choice, then reads the settings again and
    /// restarts sharing to use it: the agent maps keys as it starts. A
    /// failure is shown under the choice.
    fn set_shortcuts(&mut self, choice: CmdCtrl) {
        match settings::set_cmd_ctrl(choice) {
            Ok(()) => {
                tracing::info!(
                    swap = choice == CmdCtrl::Swap,
                    "the Shortcuts setting changed"
                );
                self.model.shortcuts_problem = None;
                if self.reload_settings() {
                    self.restart_sharing();
                }
            }
            Err(error) => {
                let line = words::not_saved(&format!("{error:#}"));
                self.model.activity.push(line.clone());
                self.model.shortcuts_problem = Some(line);
            }
        }
    }

    /// Starts a pairing session as `role` says.
    fn start_pairing(&mut self, role: PairRole) {
        let id = self.take_id();
        let (tx, rx) = mpsc::channel();
        let session = match PairSession::start(role, tx) {
            Ok(session) => session,
            Err(error) => {
                let why = format!("could not start pairing ({error})");
                self.model
                    .pairing
                    .failed_to_start(&why, &mut self.model.activity);
                return;
            }
        };
        let shared = self.shared.clone();
        if let Err(error) = bridge::forward("hop-app-pair", rx, shared, move |event| {
            News::Pair(id, event)
        }) {
            // Dropping the session cancels it.
            drop(session);
            let why = format!("could not start pairing ({error})");
            self.model
                .pairing
                .failed_to_start(&why, &mut self.model.activity);
            return;
        }
        self.pair = Some(Pair { id, session });
    }

    /// Carries out a click.
    fn act(&mut self, action: Action, now: Instant) {
        if self.model.quitting {
            // Nothing new starts while the window closes.
            return;
        }
        match action {
            Action::StartSharing => {
                if self.model.sharing.start() {
                    self.start_pending = true;
                    self.start_when_free();
                }
            }
            Action::StopSharing => {
                if self.model.sharing.stop() {
                    self.stop_agent();
                }
            }
            Action::SetSide { id, side } => {
                self.change(&id, |id| settings::set_side(id, side));
            }
            Action::SaveAddress(id) => {
                let typed = self
                    .model
                    .peers
                    .addresses
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                if self.change(&id, |id| settings::set_address(id, Some(&typed))) {
                    self.model.show_saved_address(&id);
                }
            }
            Action::AskForget(id) => self.model.peers.forgetting = Some(id),
            Action::CancelForget => self.model.peers.forgetting = None,
            Action::Forget(id) => {
                self.model.peers.forgetting = None;
                self.change(&id, settings::forget);
            }
            Action::SetShortcuts(choice) => self.set_shortcuts(choice),
            Action::ShowCode => {
                if self.model.can_pair() && self.model.pairing.begin_show() {
                    // Showing a code wiped the typed one.
                    self.forget_typed_code();
                    self.start_pairing(PairRole::Show);
                }
            }
            Action::OpenJoin => self.model.pairing.open_join(),
            Action::CloseJoin => {
                self.model.pairing.close_join();
                self.forget_typed_code();
            }
            Action::Join => {
                if self.model.can_pair()
                    && let Some(role) = self.model.pairing.begin_join()
                {
                    self.start_pairing(role);
                }
            }
            Action::CancelPairing => {
                if self.model.pairing.cancel()
                    && let Some(pair) = &self.pair
                {
                    pair.session.cancel();
                }
            }
            Action::Answer(yes) => {
                if self.model.pairing.answer(yes, now)
                    && let Some(pair) = &self.pair
                {
                    pair.session.answer(yes);
                }
            }
            Action::AllowAccessibility => {
                system::request_accessibility();
                system::open_accessibility_settings();
                self.next_permission_check = Some(now);
            }
            Action::OpenLogFolder => system::open_log_folder(),
            Action::RetrySettings => {
                if self.reload_settings() {
                    self.restart_sharing();
                }
            }
        }
    }

    /// Holds macOS's App Nap exemption while an agent runs, so its timers
    /// keep time with the window hidden. It must stay on this thread.
    fn hold_awake(&mut self) {
        let sharing = self.model.sharing.is_active() || self.agent.is_some();
        if sharing && self.awake.is_none() {
            self.awake = Some(system::stay_responsive());
        } else if !sharing {
            self.awake = None;
        }
    }

    /// Checks the Accessibility permission every 2 seconds until it is
    /// granted (only on a Mac: elsewhere there is none to check).
    fn check_permission(&mut self, now: Instant) {
        let Some(state) = self.model.permission else {
            return;
        };
        if state == PermissionState::Granted
            || self.next_permission_check.is_some_and(|at| now < at)
        {
            return;
        }
        self.model.permission = Some(system::accessibility());
        self.next_permission_check = Some(now + PERMISSION_POLL);
    }

    /// Closing the window stops sharing and pairing first. The window stays
    /// up (saying "Stopping…") until both have, or for at most 3 seconds,
    /// then closes.
    fn handle_close(&mut self, ctx: &egui::Context, now: Instant) {
        let asked = ctx.input(|input| input.viewport().close_requested());
        if asked && self.quit_by.is_none() {
            self.begin_quit(now);
        }
        if self.quit_by.is_none() {
            return;
        }
        if self.ready_to_close(now) {
            if !self.close_sent {
                self.close_sent = true;
                if !asked {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        } else if asked {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        }
    }

    /// The window started closing: stop everything, failing open.
    fn begin_quit(&mut self, now: Instant) {
        tracing::info!("the window is closing");
        self.model.quitting = true;
        self.quit_by = Some(now + QUIT_WAIT);
        self.model.peers.forgetting = None;
        if self.model.sharing.stop() {
            self.stop_agent();
        }
        self.start_pending = false;
        if let Some(agent) = &self.agent {
            agent.handle.stop();
        }
        self.model.pairing.cancel();
        if let Some(pair) = &self.pair {
            pair.session.cancel();
        }
    }

    /// Whether the window may close: nothing runs any more, or the time is
    /// up.
    fn ready_to_close(&self, now: Instant) -> bool {
        let idle = self.agent.is_none()
            && self.ending.as_ref().is_none_or(AgentHandle::is_finished)
            && self.pair.is_none();
        idle || self.quit_by.is_some_and(|by| now >= by)
    }

    /// Asks for the next frame when something is due: a start waiting for
    /// a thread, the permission check, the code's countdown, the window
    /// waiting to close.
    fn schedule(&self, ctx: &egui::Context, now: Instant) {
        let mut next: Option<Duration> = None;
        let mut soon = |wait: Duration| next = Some(next.map_or(wait, |next| next.min(wait)));
        if self.start_pending && self.ending.is_some() {
            soon(THREAD_POLL);
        }
        if self.model.permission != Some(PermissionState::Granted)
            && let Some(at) = self.next_permission_check
        {
            soon(at.saturating_duration_since(now));
        }
        if self.model.pairing.is_counting_down() {
            soon(COUNTDOWN_TICK);
        }
        if let Some(by) = self.quit_by
            && !self.close_sent
        {
            soon(THREAD_POLL.min(by.saturating_duration_since(now)));
        }
        if let Some(wait) = next {
            ctx.request_repaint_after(wait);
        }
    }

    /// The way out, however the window goes (closed, Cmd+Q, a panic): stop
    /// sharing and pairing, failing open, wait at most 3 seconds (counted
    /// from when closing began) for them to end, and give the log a moment.
    /// Runs once; it blocks only here, at the very end.
    fn shutdown(&mut self) {
        if std::mem::replace(&mut self.shut_down, true) {
            return;
        }
        let by = self.quit_by.unwrap_or_else(|| Instant::now() + QUIT_WAIT);
        self.model.quitting = true;
        self.start_pending = false;
        if let Some(agent) = &self.agent {
            agent.handle.stop();
        }
        if let Some(pair) = &self.pair {
            pair.session.cancel();
        }
        let mut in_time = true;
        let agents = self
            .agent
            .take()
            .map(|agent| agent.handle)
            .into_iter()
            .chain(self.ending.take());
        for handle in agents {
            in_time &= handle.wait(by.saturating_duration_since(Instant::now()));
        }
        if let Some(pair) = self.pair.take() {
            while !pair.session.is_finished() && Instant::now() < by {
                thread::sleep(THREAD_POLL);
            }
            in_time &= pair.session.is_finished();
        }
        if !in_time {
            tracing::warn!("Hop quit before sharing or pairing had fully stopped");
        }
        self.awake = None;
        if let Some(log) = &self.log {
            log.flush(LOG_FLUSH);
        }
    }
}

impl HopApp {
    /// Runs `work`, part of a frame. A panic in it stops sharing and pairing
    /// first ([`HopApp::shutdown`]), and only then goes on. On a Mac, AppKit
    /// calls into the frame through functions a panic cannot unwind out of
    /// (while the window is resized, say), so the process would abort before
    /// anything was dropped.
    fn guarded(&mut self, work: impl FnOnce(&mut HopApp)) {
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| work(self))) {
            self.shutdown();
            panic::resume_unwind(payload);
        }
    }
}

impl eframe::App for HopApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.guarded(|app| app.step(ctx, Instant::now()));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.guarded(|app| {
            let now = Instant::now();
            let mut actions = Vec::new();
            view::show(ui, &mut app.model, now, &mut actions);
            if !actions.is_empty() {
                for action in actions {
                    app.act(action, now);
                }
                ui.ctx().request_repaint();
            }
        });
    }

    #[cfg(not(feature = "screenshot"))]
    fn on_exit(&mut self) {
        self.shutdown();
    }

    /// With the `screenshot` feature eframe also has its OpenGL renderer,
    /// which gives this its context.
    #[cfg(feature = "screenshot")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }
}

impl Drop for HopApp {
    /// A panic that unwinds past the app (one eframe catches, or one on
    /// Windows) drops it on the way: sharing still stops in order before the
    /// process ends. Where a panic cannot unwind, [`HopApp::guarded`] and the
    /// panic hook stop sharing first.
    fn drop(&mut self) {
        self.shutdown();
    }
}
