//! Carries out the core's actions, in order, on the platform and the
//! network (docs/M2-DESIGN.md, "The agent").
//!
//! The executor owns the [`Platform`], so it is the one place that gives
//! input back: [`Executor::fail_open`] on the way out, and its `Drop` when
//! the core thread unwinds from a panic.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use hop_core::{Action, CaptureMode, EndReason, Notice};
use hop_keymap::{ButtonTracker, ModifierPolicy, Translator};
use hop_platform::{CaptureMode as PlatformMode, Platform};
use hop_types::{CoreMessage, DeviceId, InputEvent, Os, Point, RejectReason};

use super::link::Link;
use crate::status;

/// A repeated warning is written at most this often; the rest are counted.
const WARNING_PERIOD: Duration = Duration::from_secs(10);

/// What carrying out a batch of actions found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use]
pub(crate) struct Outcome {
    /// A platform call a session depends on (suppressing, parking, moving
    /// the cursor, injecting) failed, so the core must be told with
    /// `Event::LocalStall`.
    pub(crate) stalled: bool,
}

/// Which side of a session the core last reported, for wording status
/// lines.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Role {
    None,
    Controlling,
    Controlled,
}

/// Carries out the core's actions.
pub(crate) struct Executor<L: Link> {
    pub(crate) platform: Platform,
    pub(crate) link: L,
    /// This device's OS: the target of every key translation.
    os: Os,
    policy: ModifierPolicy,
    /// One translator per keyboard OS the injected keys came from. Each
    /// incoming session starts and ends with `ReleaseAllInjected`, which
    /// empties this, so every session gets fresh translators. The core
    /// also injects a few keys for local apps (modifiers pressed again when
    /// the cursor comes home); those have `source_os` = this OS, which maps
    /// every key to itself, and get a translator of their own so their
    /// key-ups always match.
    translators: Vec<(Os, Translator)>,
    /// Mouse buttons injected and still down. Releasing everything lets go
    /// of these before any key, as both platform injectors do themselves: a
    /// drag cut short then drops with the modifiers the user held (a
    /// Ctrl-drag in Explorer stays a copy instead of becoming a move).
    buttons: ButtonTracker,
    /// Where the cursor was when it was parked, while it is parked: a
    /// fail-open restore puts it back there.
    parked_from: Option<Point>,
    /// The cursor position capture last reported, in case the platform
    /// cannot say where the cursor is when it is parked.
    pub(crate) last_cursor: Point,
    /// Peer names for status lines.
    names: HashMap<DeviceId, String>,
    role: Role,
    quitting: bool,
    limiter: RateLimit,
}

impl<L: Link> Executor<L> {
    /// An executor for a device running `os`, mapping keys with `policy`.
    pub(crate) fn new(platform: Platform, link: L, os: Os, policy: ModifierPolicy) -> Self {
        Executor {
            platform,
            link,
            os,
            policy,
            translators: Vec::new(),
            buttons: ButtonTracker::new(),
            parked_from: None,
            last_cursor: Point::default(),
            names: HashMap::new(),
            role: Role::None,
            quitting: false,
            limiter: RateLimit::default(),
        }
    }

    /// Remembers `name` for status lines about `peer`.
    pub(crate) fn set_peer_name(&mut self, peer: DeviceId, name: String) {
        self.names.insert(peer, name);
    }

    /// The name to show for `peer`.
    pub(crate) fn name_of(&self, peer: &DeviceId) -> String {
        self.names
            .get(peer)
            .cloned()
            .unwrap_or_else(|| "the other computer".to_owned())
    }

    /// From now on, status lines say that Hop is quitting.
    pub(crate) fn begin_quit(&mut self) {
        self.quitting = true;
    }

    /// Whether [`Executor::begin_quit`] was called.
    pub(crate) fn is_quitting(&self) -> bool {
        self.quitting
    }

    /// Carries out `actions` in order. Every action is attempted even after
    /// one fails: the core's actions are one consistent sequence, and the
    /// `LocalStall` the caller feeds afterwards ends the session cleanly.
    pub(crate) fn execute(&mut self, actions: Vec<Action>) -> Outcome {
        let mut outcome = Outcome::default();
        for action in actions {
            match action {
                Action::Send { to, msg } => self.send(&to, msg),
                Action::SetCaptureMode(mode) => {
                    if !self.set_capture_mode(mode) && mode == CaptureMode::Suppress {
                        outcome.stalled = true;
                    }
                }
                Action::ParkCursor => {
                    if !self.park() {
                        outcome.stalled = true;
                    }
                }
                Action::RestoreCursor { at } => self.restore(at),
                Action::MoveCursorTo { to } => {
                    if let Err(error) = self.platform.injector.move_cursor_to(to) {
                        self.warn("move", "could not move the cursor", &error);
                        outcome.stalled = true;
                    }
                }
                Action::Inject { event, source_os } => {
                    if let Err(error) = self.inject(event, source_os) {
                        self.warn("inject", "could not inject input", &error);
                        outcome.stalled = true;
                    }
                }
                Action::ReleaseAllInjected => self.release_all_injected(),
                Action::Notify(notice) => self.notify(notice),
            }
        }
        outcome
    }

    /// Gives input back, whatever the core believes: capture passive,
    /// every injected key and button released, a parked cursor shown again
    /// where it was. Safe to call any number of times.
    pub(crate) fn fail_open(&mut self) {
        if let Err(error) = self.platform.capture.set_mode(PlatformMode::Passive) {
            self.warn(
                "passive",
                "could not stop suppressing input (the lease ends it within a second)",
                &error,
            );
        }
        self.release_all_injected();
        if let Some(at) = self.parked_from.take()
            && let Err(error) = self.platform.cursor.restore(at)
        {
            self.warn("restore", "could not show the cursor again", &error);
        }
    }

    fn send(&mut self, to: &DeviceId, msg: CoreMessage) {
        // Only the error is logged, never the message: it may carry keys.
        if let Err(error) = self.link.send(to, msg)
            && let Some(suppressed) = self.limiter.allow("send")
        {
            tracing::info!(peer = %to, error = %error, suppressed, "could not send to the other computer");
        }
    }

    /// Returns false if the platform refused.
    fn set_capture_mode(&mut self, mode: CaptureMode) -> bool {
        let platform_mode = match mode {
            CaptureMode::Passive => PlatformMode::Passive,
            CaptureMode::Suppress => PlatformMode::Suppress,
        };
        match self.platform.capture.set_mode(platform_mode) {
            Ok(()) => true,
            Err(error) => {
                self.warn(
                    "capture mode",
                    "could not change the input capture mode",
                    &error,
                );
                false
            }
        }
    }

    /// Returns false if the cursor could not be parked.
    fn park(&mut self) -> bool {
        let from = self.platform.cursor.position().unwrap_or(self.last_cursor);
        // Recorded before parking: a park that fails halfway still gets a
        // restore when Hop fails open.
        self.parked_from = Some(from);
        match self.platform.cursor.park() {
            Ok(()) => true,
            Err(error) => {
                self.warn("park", "could not park the cursor", &error);
                false
            }
        }
    }

    fn restore(&mut self, at: Point) {
        self.parked_from = None;
        if let Err(error) = self.platform.cursor.restore(at) {
            self.warn("restore", "could not show the cursor again", &error);
        }
    }

    /// Injects `event`, mapping keys from `source_os`'s keyboard.
    fn inject(&mut self, event: InputEvent, source_os: Os) -> hop_platform::Result<()> {
        let event = match event {
            InputEvent::Key {
                code,
                pressed,
                repeat,
            } => {
                let target = self.os;
                let policy = self.policy;
                if !self.translators.iter().any(|(os, _)| *os == source_os) {
                    self.translators
                        .push((source_os, Translator::new(policy, source_os, target)));
                }
                let Some((_, keys)) = self.translators.iter_mut().find(|(os, _)| *os == source_os)
                else {
                    return Ok(());
                };
                let code = if !pressed {
                    keys.release(code)
                } else if repeat {
                    keys.repeat(code)
                } else {
                    keys.press(code)
                };
                InputEvent::Key {
                    code,
                    pressed,
                    repeat,
                }
            }
            // The core moves the cursor with `MoveCursorTo`; relative motion
            // is never injected.
            InputEvent::PointerMotion { .. } => return Ok(()),
            InputEvent::Button { button, pressed } => {
                let result = self.platform.injector.inject(&event);
                // A press that failed holds nothing. A release that failed
                // is forgotten too: the injector's own `release_all`, which
                // always runs last, still lets go of whatever it holds.
                if !pressed {
                    self.buttons.release(button);
                } else if result.is_ok() {
                    self.buttons.press(button);
                }
                return result;
            }
            other => other,
        };
        self.platform.injector.inject(&event)
    }

    /// Releases every injected button, then every key the translators hold
    /// (letters before modifiers, as a person lets go of a shortcut), then
    /// anything else the injector still holds.
    fn release_all_injected(&mut self) {
        for button in self.buttons.release_all() {
            let up = InputEvent::Button {
                button,
                pressed: false,
            };
            if let Err(error) = self.platform.injector.inject(&up) {
                self.warn("release", "could not release an injected button", &error);
            }
        }
        for (_, mut keys) in std::mem::take(&mut self.translators) {
            for code in keys.release_all() {
                let up = InputEvent::Key {
                    code,
                    pressed: false,
                    repeat: false,
                };
                if let Err(error) = self.platform.injector.inject(&up) {
                    self.warn("release", "could not release an injected key", &error);
                }
            }
        }
        if let Err(error) = self.platform.injector.release_all() {
            self.warn("release", "could not release injected input", &error);
        }
    }

    fn notify(&mut self, notice: Notice) {
        match notice {
            Notice::ControlStarted { peer } => {
                self.role = Role::Controlling;
                tracing::info!(peer = %peer, "session started: controlling");
                status!("Controlling {}.", self.name_of(&peer));
            }
            Notice::ControlledBy { peer } => {
                self.role = Role::Controlled;
                tracing::info!(peer = %peer, "session started: controlled");
                status!("{} is controlling this computer.", self.name_of(&peer));
            }
            Notice::SessionEnded { peer, reason } => {
                let name = self.name_of(&peer);
                let why = self.reason_text(reason, &name);
                tracing::info!(peer = %peer, reason = ?reason, "session ended");
                match std::mem::replace(&mut self.role, Role::None) {
                    Role::Controlling => status!("Back on this computer: {why}."),
                    Role::Controlled => status!("{name} stopped controlling this computer: {why}."),
                    Role::None => status!("Could not move to {name}: {why}."),
                }
            }
            Notice::PeerDisplaysRejected { peer, error } => {
                tracing::warn!(peer = %peer, error = %error, "the other computer's displays are unusable");
                status!(
                    "The cursor cannot move to {}: its screens are unusable ({error}).",
                    self.name_of(&peer)
                );
            }
        }
    }

    fn reason_text(&self, reason: EndReason, name: &str) -> String {
        let controller = self.role == Role::Controlling;
        match reason {
            EndReason::Rejected(RejectReason::Busy) => {
                format!("{name} is busy with another computer")
            }
            EndReason::Rejected(RejectReason::Unavailable) => {
                format!("{name} cannot take the cursor right now")
            }
            EndReason::EnterTimeout => format!("{name} did not answer"),
            EndReason::LostTieBreak => "both computers crossed at the same moment".to_owned(),
            EndReason::BufferFull => format!("too much input arrived while waiting for {name}"),
            EndReason::CursorLeft => "the cursor crossed back".to_owned(),
            EndReason::TakenBack if controller => {
                format!("{name}'s own mouse or keyboard was used")
            }
            EndReason::TakenBack => "this computer's own mouse or keyboard was used".to_owned(),
            EndReason::Released => format!("{name} ended the session"),
            EndReason::EmergencyChord => "the emergency keys were pressed".to_owned(),
            EndReason::PeerTimeout => format!("no word from {name} for over a second"),
            EndReason::Disconnected => format!("the connection to {name} closed"),
            EndReason::Superseded => format!("{name} started a new session"),
            EndReason::UnreachableTarget => {
                "the cursor was sent on to a computer this one cannot reach".to_owned()
            }
            EndReason::Stalled if self.quitting => "Hop is quitting".to_owned(),
            // A lapsed lease, lost input or a failed platform call; the log
            // says which.
            EndReason::Stalled => "Hop could not keep control on this computer, so it gave \
                                   the keyboard and mouse back (the log says why)"
                .to_owned(),
        }
    }

    /// Logs a platform failure, at most once per [`WARNING_PERIOD`] for each
    /// kind. Platform errors name the call that failed, never the key.
    fn warn(&mut self, kind: &'static str, what: &str, error: &hop_platform::PlatformError) {
        if let Some(suppressed) = self.limiter.allow(kind) {
            tracing::warn!(error = %error, suppressed, "{what}");
        }
    }
}

impl<L: Link> Drop for Executor<L> {
    /// Fails open: this runs when the core thread returns and when it
    /// unwinds from a panic. Dropping the platform afterwards also stops
    /// capture, which gives a parked cursor back by itself.
    fn drop(&mut self) {
        self.fail_open();
        self.platform.capture.stop();
    }
}

/// Lets each kind of repeated warning through at most once per period.
#[derive(Default)]
struct RateLimit {
    seen: HashMap<&'static str, (Instant, u64)>,
}

impl RateLimit {
    /// `Some(suppressed since the last one)` when a warning of `kind` may be
    /// written now.
    fn allow(&mut self, kind: &'static str) -> Option<u64> {
        let now = Instant::now();
        match self.seen.get_mut(kind) {
            Some((last, suppressed)) if now.duration_since(*last) < WARNING_PERIOD => {
                *suppressed += 1;
                None
            }
            Some((last, suppressed)) => {
                *last = now;
                Some(std::mem::take(suppressed))
            }
            None => {
                self.seen.insert(kind, (now, 0));
                Some(0)
            }
        }
    }
}
