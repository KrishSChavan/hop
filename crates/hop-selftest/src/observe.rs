//! What the capture thread hands to the self-test.
//!
//! The sink runs on the backend's capture thread. It classifies each event,
//! sends it over a channel and returns, so it never blocks that thread. On
//! the way it drops every key's identity: the self-test only ever learns
//! that *a* key went down or up, except for the two keys of the copy
//! shortcut that step 2 asks for. The modifiers it tracks to spot the
//! emergency chord stay inside the sink.

use std::collections::BTreeSet;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use hop_core::Chord;
use hop_platform::CapturedEvent;
use hop_types::{InputEvent, KeyCode, MouseButton, Os, Point};

use crate::keys::{KEY_C, command_key};

/// One captured event, stripped down to what the self-test may know.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Observation {
    /// When the capture thread saw it.
    pub at: Instant,
    /// False for Hop's own injected events.
    pub physical: bool,
    /// The cursor position before the event.
    pub cursor: Point,
    pub kind: Observed,
}

/// The kind of a captured event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Observed {
    /// Pointer motion with the delta the user attempted.
    Motion {
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Scroll,
    /// A key went down or up. Which key is not kept, except for its role in
    /// the copy shortcut.
    Key {
        pressed: bool,
        role: KeyRole,
    },
}

/// What a captured key means to the self-test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyRole {
    /// Any key the self-test does not ask for. Never identified further.
    Other,
    /// Command on macOS, Control on Windows (either side).
    CopyModifier,
    /// C, without the modifier held (or its release).
    CopyLetter,
    /// A press of C while the modifier is held: the copy shortcut.
    CopyShortcut,
}

/// Tracks the physically held modifiers, which is all the sink needs to
/// spot the emergency chord and the copy shortcut. Hop's own injected keys
/// never count.
pub(crate) struct KeyWatch {
    chord: Chord,
    command: KeyCode,
    /// Held modifiers, folded to their left-hand key.
    held: BTreeSet<KeyCode>,
}

impl KeyWatch {
    pub(crate) fn new(os: Os) -> Self {
        Self {
            chord: Chord::default(),
            command: command_key(os),
            held: BTreeSet::new(),
        }
    }

    /// Classifies `event` and reports whether it completes the emergency
    /// chord.
    pub(crate) fn observe(&mut self, event: &CapturedEvent, at: Instant) -> (Observation, bool) {
        let (kind, chord) = match event.event {
            InputEvent::PointerMotion { dx, dy } => (Observed::Motion { dx, dy }, false),
            InputEvent::Button { button, pressed } => (Observed::Button { button, pressed }, false),
            InputEvent::Scroll { .. } => (Observed::Scroll, false),
            InputEvent::Key {
                code,
                pressed,
                repeat,
            } => {
                let (role, chord) = self.key(code, pressed, repeat, event.physical);
                (Observed::Key { pressed, role }, chord)
            }
        };
        let observation = Observation {
            at,
            physical: event.physical,
            cursor: event.cursor,
            kind,
        };
        (observation, chord)
    }

    fn key(
        &mut self,
        code: KeyCode,
        pressed: bool,
        repeat: bool,
        physical: bool,
    ) -> (KeyRole, bool) {
        if !physical {
            return (KeyRole::Other, false);
        }
        // Either side's modifiers work for the chord: aborting is always
        // safe, so the self-test errs on the side of stopping.
        let code = left_side(code);
        if code.is_modifier() {
            if pressed {
                self.held.insert(code);
            } else {
                self.held.remove(&code);
            }
        }
        let event = InputEvent::Key {
            code,
            pressed,
            repeat,
        };
        let chord = self.chord.is_completed_by(&event, &self.held);
        let role = if code == self.command {
            KeyRole::CopyModifier
        } else if code == KEY_C {
            if pressed && self.held.contains(&self.command) {
                KeyRole::CopyShortcut
            } else {
                KeyRole::CopyLetter
            }
        } else {
            KeyRole::Other
        };
        (role, chord)
    }
}

/// The left-hand key of a right-hand modifier; every other key unchanged.
fn left_side(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::RIGHT_CONTROL => KeyCode::LEFT_CONTROL,
        KeyCode::RIGHT_SHIFT => KeyCode::LEFT_SHIFT,
        KeyCode::RIGHT_ALT => KeyCode::LEFT_ALT,
        KeyCode::RIGHT_META => KeyCode::LEFT_META,
        other => other,
    }
}

/// The sink type [`hop_platform::InputCapture::start`] takes.
pub(crate) type Sink = Box<dyn FnMut(CapturedEvent) + Send>;

/// A capture sink that sends observations to `tx` and sets `abort` when the
/// emergency chord is pressed.
pub(crate) fn sink(tx: Sender<Observation>, abort: Arc<AtomicBool>, os: Os) -> Sink {
    let mut watch = KeyWatch::new(os);
    Box::new(move |event: CapturedEvent| {
        // This runs inside the backend's OS callback, so nothing may unwind
        // out of it. Should anything here panic, stop the run: that is the
        // fail-open direction.
        let handled = panic::catch_unwind(AssertUnwindSafe(|| {
            let (observation, chord) = watch.observe(&event, Instant::now());
            if chord {
                abort.store(true, Ordering::SeqCst);
            }
            // Unbounded, so this never blocks. It fails only once the run is
            // over and the receiver is gone, when the event is not needed.
            let _ = tx.send(observation);
        }));
        if handled.is_err() {
            abort.store(true, Ordering::SeqCst);
        }
    })
}

/// Event counts of one kind of origin (physical or injected).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub motion: u32,
    pub button: u32,
    pub scroll: u32,
    pub key: u32,
}

/// Counts of what capture saw during part of a step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub physical: Counts,
    pub injected: Counts,
    /// Physical left-button presses and releases.
    pub left_presses: u32,
    pub left_releases: u32,
    /// The parts of the copy shortcut pressed physically.
    pub copy_modifier: bool,
    pub copy_letter: bool,
    pub copy_shortcut: bool,
}

fn bump(count: &mut u32) {
    *count = count.saturating_add(1);
}

impl Tally {
    pub(crate) fn add(&mut self, observation: &Observation) {
        let counts = if observation.physical {
            &mut self.physical
        } else {
            &mut self.injected
        };
        match observation.kind {
            Observed::Motion { .. } => bump(&mut counts.motion),
            Observed::Button { button, pressed } => {
                bump(&mut counts.button);
                if observation.physical && button == MouseButton::Left {
                    bump(if pressed {
                        &mut self.left_presses
                    } else {
                        &mut self.left_releases
                    });
                }
            }
            Observed::Scroll => bump(&mut counts.scroll),
            Observed::Key { pressed, role } => {
                bump(&mut counts.key);
                if observation.physical && pressed {
                    match role {
                        KeyRole::Other => {}
                        KeyRole::CopyModifier => self.copy_modifier = true,
                        KeyRole::CopyLetter => self.copy_letter = true,
                        KeyRole::CopyShortcut => {
                            self.copy_letter = true;
                            self.copy_shortcut = true;
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn add_all(&mut self, observations: &[Observation]) {
        for observation in observations {
            self.add(observation);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn key_event(code: KeyCode, pressed: bool, physical: bool) -> CapturedEvent {
        CapturedEvent {
            event: InputEvent::Key {
                code,
                pressed,
                repeat: false,
            },
            cursor: Point::new(1.0, 2.0),
            physical,
        }
    }

    fn press_all(watch: &mut KeyWatch, codes: &[KeyCode], physical: bool) -> Vec<bool> {
        codes
            .iter()
            .map(|&code| {
                watch
                    .observe(&key_event(code, true, physical), Instant::now())
                    .1
            })
            .collect()
    }

    #[test]
    fn the_chord_completes_on_escape_with_all_three_modifiers_on_either_side() {
        let chord = [
            KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::LEFT_SHIFT,
            KeyCode::ESCAPE,
        ];
        let mut watch = KeyWatch::new(Os::MacOs);
        assert_eq!(
            press_all(&mut watch, &chord, true),
            [false, false, false, true]
        );

        let mixed = [
            KeyCode::RIGHT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::RIGHT_SHIFT,
            KeyCode::ESCAPE,
        ];
        let mut watch = KeyWatch::new(Os::Windows);
        assert_eq!(
            press_all(&mut watch, &mixed, true),
            [false, false, false, true]
        );
    }

    #[test]
    fn the_chord_needs_every_modifier_still_held() {
        let mut watch = KeyWatch::new(Os::MacOs);
        press_all(
            &mut watch,
            &[
                KeyCode::LEFT_CONTROL,
                KeyCode::LEFT_ALT,
                KeyCode::LEFT_SHIFT,
            ],
            true,
        );
        watch.observe(&key_event(KeyCode::LEFT_ALT, false, true), Instant::now());
        assert_eq!(press_all(&mut watch, &[KeyCode::ESCAPE], true), [false]);
    }

    #[test]
    fn hops_own_keys_never_complete_the_chord() {
        let chord = [
            KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::LEFT_SHIFT,
            KeyCode::ESCAPE,
        ];
        let mut watch = KeyWatch::new(Os::MacOs);
        assert_eq!(press_all(&mut watch, &chord, false), [false; 4]);
        // Injected modifiers are not remembered as held either.
        assert_eq!(press_all(&mut watch, &[KeyCode::ESCAPE], true), [false]);
    }

    #[test]
    fn only_the_copy_shortcut_keys_are_identified() {
        let mut watch = KeyWatch::new(Os::MacOs);
        let roles: Vec<_> = [
            (KeyCode::keyboard(0x10), true), // M, some other key
            (KEY_C, true),
            (KEY_C, false),
            (KeyCode::LEFT_META, true),
            (KEY_C, true),
            (KeyCode::LEFT_CONTROL, true),
        ]
        .into_iter()
        .map(|(code, pressed)| {
            match watch
                .observe(&key_event(code, pressed, true), Instant::now())
                .0
                .kind
            {
                Observed::Key { role, .. } => role,
                other => panic!("not a key: {other:?}"),
            }
        })
        .collect();
        assert_eq!(
            roles,
            [
                KeyRole::Other,
                KeyRole::CopyLetter,
                KeyRole::CopyLetter,
                KeyRole::CopyModifier,
                KeyRole::CopyShortcut,
                // Control is not the copy modifier on a Mac.
                KeyRole::Other,
            ]
        );

        // On Windows the copy modifier is Control, either side.
        let mut watch = KeyWatch::new(Os::Windows);
        watch.observe(
            &key_event(KeyCode::RIGHT_CONTROL, true, true),
            Instant::now(),
        );
        let (observation, _) = watch.observe(&key_event(KEY_C, true, true), Instant::now());
        assert_eq!(
            observation.kind,
            Observed::Key {
                pressed: true,
                role: KeyRole::CopyShortcut
            }
        );
    }

    #[test]
    fn the_sink_hands_events_off_and_raises_the_abort_flag() {
        let (tx, rx) = mpsc::channel();
        let abort = Arc::new(AtomicBool::new(false));
        let mut sink = sink(tx, Arc::clone(&abort), Os::MacOs);
        sink(CapturedEvent {
            event: InputEvent::PointerMotion { dx: 3.0, dy: -1.0 },
            cursor: Point::new(10.0, 20.0),
            physical: true,
        });
        let first = rx.try_recv().unwrap();
        assert_eq!(first.kind, Observed::Motion { dx: 3.0, dy: -1.0 });
        assert_eq!(first.cursor, Point::new(10.0, 20.0));
        assert!(first.physical);
        assert!(!abort.load(Ordering::SeqCst));

        for code in [
            KeyCode::LEFT_CONTROL,
            KeyCode::LEFT_ALT,
            KeyCode::LEFT_SHIFT,
            KeyCode::ESCAPE,
        ] {
            sink(key_event(code, true, true));
        }
        assert!(abort.load(Ordering::SeqCst));
        assert_eq!(rx.try_iter().count(), 4);

        // With the receiver gone, the sink still returns quietly.
        drop(rx);
        sink(key_event(KEY_C, true, true));
    }

    #[test]
    fn the_tally_counts_by_kind_and_origin() {
        let at = Instant::now();
        let cursor = Point::default();
        let observation = |physical, kind| Observation {
            at,
            physical,
            cursor,
            kind,
        };
        let mut tally = Tally::default();
        tally.add_all(&[
            observation(true, Observed::Motion { dx: 1.0, dy: 0.0 }),
            observation(true, Observed::Motion { dx: 1.0, dy: 0.0 }),
            observation(false, Observed::Motion { dx: 1.0, dy: 0.0 }),
            observation(
                true,
                Observed::Button {
                    button: MouseButton::Left,
                    pressed: true,
                },
            ),
            observation(
                true,
                Observed::Button {
                    button: MouseButton::Left,
                    pressed: false,
                },
            ),
            observation(
                true,
                Observed::Button {
                    button: MouseButton::Right,
                    pressed: true,
                },
            ),
            observation(true, Observed::Scroll),
            observation(
                true,
                Observed::Key {
                    pressed: true,
                    role: KeyRole::CopyModifier,
                },
            ),
            observation(
                true,
                Observed::Key {
                    pressed: true,
                    role: KeyRole::CopyShortcut,
                },
            ),
            observation(
                false,
                Observed::Key {
                    pressed: true,
                    role: KeyRole::Other,
                },
            ),
        ]);
        assert_eq!(
            tally.physical,
            Counts {
                motion: 2,
                button: 3,
                scroll: 1,
                key: 2
            }
        );
        assert_eq!(
            tally.injected,
            Counts {
                motion: 1,
                button: 0,
                scroll: 0,
                key: 1
            }
        );
        assert_eq!((tally.left_presses, tally.left_releases), (1, 1));
        assert!(tally.copy_modifier && tally.copy_letter && tally.copy_shortcut);
    }
}
