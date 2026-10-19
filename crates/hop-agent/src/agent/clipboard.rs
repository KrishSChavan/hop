//! Text clipboard sync (docs/M4-M5-DESIGN.md, section 4), on the core
//! thread.
//!
//! Text travels when the cursor leaves a computer, never on every copy: the
//! controller offers its clipboard right after it sends `Enter`, and the
//! controlled side right before it sends `Leave`. Both go on the ordered
//! control stream, and the receiving side writes the text before it handles
//! the next event. So after an `Enter` the text is written before the next
//! key from the controller, and before a `Leave` it is written before the
//! controller takes its own keyboard back: Ctrl+V right after crossing, on
//! either computer, pastes it. (The executor's docs say what sending before
//! the `Leave` costs.)
//!
//! [`ClipboardSync`] decides what to read, send and remember; the executor
//! hands it the platform's clipboard and carries out the sending. Every
//! clipboard call returns within [`hop_platform::CLIPBOARD_TIMEOUT`], so
//! the core thread waits at most that long for one.
//!
//! What is remembered is the change count both computers last had in
//! common, never the text:
//! - it starts at the count when sharing starts, so what was copied before
//!   is never pushed; a count that cannot be read then counts as already
//!   shared too (the first crossing only records it), and the agent keeps
//!   trying to read it while nothing happens;
//! - a clipboard that has not changed since is not even read;
//! - one that holds nothing to send (no text, a password manager's item,
//!   text over the limit) is not read again until it changes;
//! - a read that failed (it took too long, say) records nothing, so the
//!   next crossing tries again;
//! - text a connection on its way out did not take is offered again at the
//!   first crossing on the next connection;
//! - text from the other computer, once written, counts as in common, so it
//!   is never sent straight back.
//!
//! Text from the other computer that cannot be written (another app holds
//! the clipboard, say) waits as a pending write, tried again from the
//! agent's tick for [`RETRY_WRITE_FOR`], though never while a crossing
//! waits for its answer (`Agent::tick` says why): it goes once a try
//! works, and is dropped when newer text arrives or when something is
//! copied here meanwhile, which wins. Until something is copied here, what
//! this clipboard holds is older than that text, so it is never sent back
//! over it, not even once the text is given up. Only the text is kept for
//! that, in memory; it is never logged.
//!
//! A copy made here shows as a change count newer than the one the
//! clipboard had when the write failed. When that count cannot be read (on
//! a Mac, every clipboard call waits while an app is slow to hand over what
//! it copied), the newest count read before stands in for it. An older one
//! would not do: the clipboard would look as if something had been copied
//! here since, so the other computer's text would be dropped and the older
//! text here sent back over it. Nor is a write that used up all of
//! [`CLIPBOARD_TIMEOUT`] followed by a read of the count, which would only
//! wait as long again.
//!
//! Nothing here logs.

use std::fmt;
use std::time::{Duration, Instant};

use hop_platform::{CLIPBOARD_TIMEOUT, PlatformError, TextClipboard};
use hop_types::CoreMessage;

/// How long text from the other computer that could not be written is
/// tried again before it is given up.
const RETRY_WRITE_FOR: Duration = Duration::from_secs(2);

/// The wait before the first new try at a pending write. Each later try
/// waits twice as long as the one before (100, 200, 400, 800 ms), so a
/// clipboard that hangs (up to `CLIPBOARD_TIMEOUT` a call) holds the core
/// thread up only a few times.
const FIRST_RETRY_AFTER: Duration = Duration::from_millis(100);

/// Said once for each connection that carries clipboard text, so the user
/// always knows that what they copy can travel. Nothing is said per copy.
pub(crate) fn works_with(peer: &str) -> String {
    format!("Copy and paste now work between this computer and {peer}.")
}

/// Said once a run when this computer's clipboard could not be read as the
/// cursor left for `peer`, with the platform's reason, which never holds
/// any text: on a Mac where macOS does not let Hop read the clipboard, it
/// names the setting that lets it.
pub(crate) fn unreadable(peer: &str, error: &PlatformError) -> String {
    let reason = error.to_string();
    let reason = reason.trim_end_matches('.');
    format!("Copied text did not go to {peer}: {reason}.")
}

/// The `[clipboard]` settings the core thread runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipboardSettings {
    /// Text travels between the computers (`clipboard.enabled`).
    pub(crate) enabled: bool,
    /// Longer text, in bytes of UTF-8, is not sent at all rather than cut
    /// short (`clipboard.max_text_bytes`).
    pub(crate) max_text_bytes: usize,
}

impl ClipboardSettings {
    /// Nothing travels.
    pub(crate) const OFF: ClipboardSettings = ClipboardSettings {
        enabled: false,
        max_text_bytes: 0,
    };
}

/// What [`ClipboardSync::offer`] found.
#[derive(Debug, PartialEq)]
pub(crate) enum Offer {
    /// Sync is off, or the clipboard has not changed since the two computers
    /// last had the same content: nothing to do.
    Unchanged,
    /// The clipboard changed, but holds nothing to send: no text, a password
    /// manager's item, or text over the limit. That is recorded, so it is
    /// not read again until it changes.
    NothingToSend,
    /// Send `message`, a [`CoreMessage::ClipboardText`], then call
    /// [`ClipboardSync::sent`] with `count` once it went out.
    Text {
        /// The change count the text was read at.
        count: u64,
        /// The message carrying the text.
        message: CoreMessage,
    },
}

/// What [`ClipboardSync::retry`] did.
#[derive(Debug)]
pub(crate) enum Retried {
    /// No write is pending, or its next try is not due yet.
    Nothing,
    /// The pending text is on the clipboard now.
    Written,
    /// The try failed; the next one comes later, until the time is up.
    Failed(PlatformError),
    /// Something was copied here since the write failed: that wins, and the
    /// pending text is dropped.
    Superseded,
    /// The time is up: the pending text is dropped.
    GaveUp,
}

/// Text from the other computer that could not be written yet.
struct PendingWrite {
    text: String,
    /// The clipboard's change count after the last failed write (or, when
    /// that could not be read, the newest one read before): if the
    /// clipboard moves on from it, something was copied here since, and
    /// that wins.
    count: u64,
    /// When to give up.
    until: Instant,
    /// When the next try is due, and how long the one after it waits.
    next: Instant,
    wait: Duration,
}

impl fmt::Debug for PendingWrite {
    // Never the text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingWrite")
            .field("count", &self.count)
            .field("until", &self.until)
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

/// The core thread's side of clipboard sync: whether it is on, the change
/// count both computers last had in common, and text from the other
/// computer still waiting to be written.
#[derive(Debug)]
pub(crate) struct ClipboardSync {
    settings: ClipboardSettings,
    /// The clipboard's change count when both computers last had the same
    /// content: after this computer sent its text (or found nothing to
    /// send), or right after it wrote the other computer's text. `None`
    /// until known. Unknown counts as already shared: the next crossing
    /// then only records the count and sends nothing, since what the
    /// clipboard holds may have been copied before sharing started. It
    /// never goes back to an older count, which would send again what
    /// already went.
    synced: Option<u64>,
    /// The newest change count read from the clipboard, or left by a
    /// write, so far; `None` before any. Counts only grow, so this is the
    /// closest there is to the clipboard's own count when that cannot be
    /// read.
    seen: Option<u64>,
    /// The generation of the last text offered.
    generation: u64,
    /// Text from the other computer that could not be written yet.
    pending: Option<PendingWrite>,
}

impl ClipboardSync {
    /// Sync with `settings`. Call [`ClipboardSync::start`] as sharing starts.
    pub(crate) fn new(settings: ClipboardSettings) -> ClipboardSync {
        ClipboardSync {
            settings,
            synced: None,
            seen: None,
            generation: 0,
            pending: None,
        }
    }

    /// Whether text travels at all.
    pub(crate) fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Remembers the clipboard's change count as sharing starts, so a
    /// clipboard from before is never pushed at the first crossing. When the
    /// clipboard cannot say, the count stays unknown, and that counts as
    /// already shared: the first crossing only records the count, and text
    /// goes once the clipboard changes after that.
    /// [`ClipboardSync::learn_start`] tries again meanwhile.
    pub(crate) fn start(&mut self, clipboard: &dyn TextClipboard) {
        if self.settings.enabled {
            self.synced = self.read_count(clipboard).ok();
        }
    }

    /// Whether the count sharing started at could not be read yet, so it is
    /// worth trying again ([`ClipboardSync::learn_start`]).
    pub(crate) fn start_unknown(&self) -> bool {
        self.settings.enabled && self.synced.is_none()
    }

    /// Tries again to read the count sharing started at, if it is still
    /// unknown. Text copied after this goes at the next crossing; until
    /// then, whatever the clipboard holds counts as already shared. Reads
    /// only the change count, never the text.
    pub(crate) fn learn_start(&mut self, clipboard: &dyn TextClipboard) {
        if self.start_unknown() {
            self.synced = self.read_count(clipboard).ok();
        }
    }

    /// What to send the other computer as the cursor leaves for it: the
    /// clipboard's text, if the clipboard changed since the two computers
    /// last had the same content. Unchanged, it is not even read. While
    /// that count is unknown (it could not be read as sharing started),
    /// nothing is read or sent either: the count is only recorded, failing
    /// closed, since the clipboard may still hold what was copied before
    /// sharing started. Nor while the other computer's text waits to be
    /// written here and nothing was copied here since: what this clipboard
    /// holds is older than that text.
    ///
    /// # Errors
    ///
    /// The clipboard could not be read (it took too long, say). Nothing is
    /// recorded then, so the next crossing tries again.
    pub(crate) fn offer(&mut self, clipboard: &dyn TextClipboard) -> Result<Offer, PlatformError> {
        if !self.settings.enabled {
            return Ok(Offer::Unchanged);
        }
        let count = self.read_count(clipboard)?;
        let Some(synced) = self.synced else {
            self.synced = Some(count);
            return Ok(Offer::Unchanged);
        };
        let behind = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.count == count);
        if synced == count || behind {
            return Ok(Offer::Unchanged);
        }
        match clipboard.read_text(self.settings.max_text_bytes)? {
            Some(text) => {
                self.generation += 1;
                Ok(Offer::Text {
                    count,
                    message: CoreMessage::ClipboardText {
                        generation: self.generation,
                        text,
                    },
                })
            }
            None => {
                self.synced = Some(count);
                Ok(Offer::NothingToSend)
            }
        }
    }

    /// The text [`ClipboardSync::offer`] read at `count` went out (or can
    /// never go): it is not offered again until the clipboard changes.
    pub(crate) fn sent(&mut self, count: u64) {
        self.synced = Some(count);
    }

    /// Puts text from the other computer on this computer's clipboard and
    /// remembers the change count the write left, so the next crossing does
    /// not send the text straight back. It replaces any text still waiting
    /// to be written. Does nothing while sync is off.
    ///
    /// # Errors
    ///
    /// The write failed. Nothing is recorded as in common then; the text
    /// waits to be tried again ([`ClipboardSync::retry`]) from `now` on,
    /// as long as there is a change count to tell a copy made here
    /// meanwhile by: the clipboard's own after the failure, or else the
    /// newest one read before ([`ClipboardSync::count_after_failed_write`]).
    pub(crate) fn receive(
        &mut self,
        clipboard: &mut dyn TextClipboard,
        text: &str,
        now: Instant,
    ) -> Result<(), PlatformError> {
        if !self.settings.enabled {
            return Ok(());
        }
        self.pending = None;
        let started = Instant::now();
        match clipboard.write_text(text) {
            Ok(count) => {
                self.saw(count);
                self.synced = Some(count);
                Ok(())
            }
            Err(error) => {
                let count = self.count_after_failed_write(clipboard, started);
                self.pending = count.map(|count| PendingWrite {
                    text: text.to_owned(),
                    count,
                    until: now + RETRY_WRITE_FOR,
                    next: now + FIRST_RETRY_AFTER,
                    wait: FIRST_RETRY_AFTER,
                });
                Err(error)
            }
        }
    }

    /// Tries again to write the other computer's text that could not be
    /// written, if its next try is due at `now`: first the change count is
    /// checked, and if it moved on, something was copied here meanwhile and
    /// the text is dropped. A pending write is given up after
    /// [`RETRY_WRITE_FOR`], and what the clipboard held when it failed then
    /// counts as in common (unless something newer already does), so it is
    /// not sent back over that text. Call it often (every tick); it does
    /// nothing between tries.
    pub(crate) fn retry(&mut self, clipboard: &mut dyn TextClipboard, now: Instant) -> Retried {
        let Some(mut pending) = self.pending.take() else {
            return Retried::Nothing;
        };
        if now >= pending.until {
            // What the clipboard held when the write failed is older than
            // the other computer's text: it counts as in common, so it is
            // never sent back over that text. Anything copied here since
            // has another count, and goes. What went over since (a newer
            // count) stays in common: going back to the older count would
            // send it again.
            self.synced = self.synced.max(Some(pending.count));
            return Retried::GaveUp;
        }
        if now < pending.next {
            self.pending = Some(pending);
            return Retried::Nothing;
        }
        pending.wait = pending.wait.saturating_mul(2);
        pending.next = now + pending.wait;
        match self.read_count(clipboard) {
            Ok(count) if count != pending.count => return Retried::Superseded,
            Ok(_) => {}
            // No way to tell whether something was copied here: not now.
            Err(error) => {
                self.pending = Some(pending);
                return Retried::Failed(error);
            }
        }
        let started = Instant::now();
        match clipboard.write_text(&pending.text) {
            Ok(count) => {
                self.saw(count);
                self.synced = Some(count);
                Retried::Written
            }
            Err(error) => {
                // A write that failed halfway may have moved the count on
                // itself; that is no copy made here.
                if let Some(count) = self.count_after_failed_write(clipboard, started) {
                    pending.count = count;
                }
                self.pending = Some(pending);
                Retried::Failed(error)
            }
        }
    }

    /// The clipboard's change count right after a write that failed, which
    /// a copy made here afterwards is told apart from. A write that used up
    /// all of [`CLIPBOARD_TIMEOUT`] means the clipboard is not answering (on
    /// a Mac, an app is slow to hand over what it copied, and every call
    /// waits behind it), so the count is not even asked for: that would
    /// only wait as long again, and fail too. Then, or when the count
    /// cannot be read, the newest one read before stands in. `None` when
    /// none was ever read.
    fn count_after_failed_write(
        &mut self,
        clipboard: &dyn TextClipboard,
        write_started: Instant,
    ) -> Option<u64> {
        if write_started.elapsed() < CLIPBOARD_TIMEOUT
            && let Ok(count) = self.read_count(clipboard)
        {
            return Some(count);
        }
        self.newest()
    }

    /// Reads the clipboard's change count, and remembers it as the newest
    /// one seen.
    fn read_count(&mut self, clipboard: &dyn TextClipboard) -> Result<u64, PlatformError> {
        let count = clipboard.change_count()?;
        self.saw(count);
        Ok(count)
    }

    /// Remembers `count`, read from the clipboard or left by a write, if it
    /// is the newest one seen.
    fn saw(&mut self, count: u64) {
        self.seen = self.seen.max(Some(count));
    }

    /// The newest change count known: counts only grow, so the larger of
    /// the newest one seen and the one in common.
    fn newest(&self) -> Option<u64> {
        self.seen.max(self.synced)
    }
}

#[cfg(test)]
mod tests {
    use hop_platform::Platform;

    use super::*;
    use crate::agent::fake::Fake;

    const ON: ClipboardSettings = ClipboardSettings {
        enabled: true,
        max_text_bytes: 64,
    };

    fn text(offer: &Offer) -> Option<(u64, &str)> {
        match offer {
            Offer::Text {
                message: CoreMessage::ClipboardText { generation, text },
                ..
            } => Some((*generation, text.as_str())),
            _ => None,
        }
    }

    #[test]
    fn the_status_line_names_the_other_computer_and_no_text() {
        assert_eq!(
            works_with("Windows PC"),
            "Copy and paste now work between this computer and Windows PC."
        );
    }

    /// docs/INSTALL.md points to this line for the Mac's setting.
    #[test]
    fn a_clipboard_that_cannot_be_read_is_explained_with_the_platforms_reason() {
        let denied = PlatformError::Os(
            "macOS does not let Hop read the clipboard: allow it in System Settings > Privacy & \
             Security > Paste from Other Apps"
                .to_owned(),
        );
        assert_eq!(
            unreadable("Windows PC", &denied),
            "Copied text did not go to Windows PC: macOS does not let Hop read the clipboard: \
             allow it in System Settings > Privacy & Security > Paste from Other Apps."
        );
        let busy = PlatformError::Os("another app is holding the clipboard open.".to_owned());
        assert_eq!(
            unreadable("Mac", &busy),
            "Copied text did not go to Mac: another app is holding the clipboard open."
        );
    }

    #[test]
    fn a_clipboard_from_before_sharing_is_never_offered() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        fake.copy("copied before sharing started");
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );

        fake.copy("copied while sharing");
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied while sharing")));
    }

    #[test]
    fn text_is_offered_until_it_went_out_and_then_not_again() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.copy("one");
        let first = sync.offer(platform.clipboard.as_ref()).expect("read");
        // Not sent (the connection was gone, say): offered again, with the
        // next generation.
        let again = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&first), Some((1, "one")));
        assert_eq!(text(&again), Some((2, "one")));
        let Offer::Text { count, .. } = again else {
            panic!("no text");
        };
        sync.sent(count);
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        fake.copy("two");
        let next = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&next), Some((3, "two")));
    }

    #[test]
    fn a_clipboard_with_nothing_to_send_is_not_read_again_until_it_changes() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        // Longer than the limit: never cut short, so nothing to send.
        fake.copy(&"x".repeat(65));
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::NothingToSend)
        );
        fake.state().fail_clipboard_read = true;
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged),
            "not read again"
        );
        fake.state().fail_clipboard_read = false;
        fake.copy(&"x".repeat(64));
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer).map(|(_, text)| text.len()), Some(64));
    }

    #[test]
    fn a_failed_read_records_nothing_so_the_next_crossing_tries_again() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.copy("one");
        fake.state().fail_clipboard_read = true;
        assert!(sync.offer(platform.clipboard.as_ref()).is_err());
        fake.state().fail_clipboard_read = false;
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "one")));

        // Nor does a change count that cannot be read.
        fake.state().fail_clipboard_count = true;
        assert!(sync.offer(platform.clipboard.as_ref()).is_err());
    }

    #[test]
    fn an_unknown_count_at_the_start_counts_as_already_shared() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        fake.copy("copied before sharing started");
        fake.state().fail_clipboard_count = true;
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        assert!(sync.start_unknown());
        fake.state().fail_clipboard_count = false;
        // The first crossing only records the count: fail closed.
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert_eq!(fake.state().clipboard_reads, 0, "not even read");
        assert!(!sync.start_unknown());

        fake.copy("copied while sharing");
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied while sharing")));
    }

    #[test]
    fn a_count_unknown_at_the_start_is_learned_on_a_later_try() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        fake.copy("copied before sharing started");
        fake.state().fail_clipboard_count = true;
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        // Still unreadable: still unknown.
        sync.learn_start(platform.clipboard.as_ref());
        assert!(sync.start_unknown());
        fake.state().fail_clipboard_count = false;
        sync.learn_start(platform.clipboard.as_ref());
        assert!(!sync.start_unknown());
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged),
            "what was there before is still not pushed"
        );

        // Copied after the count was learned: it goes at the first crossing.
        let (platform, fake) = Fake::new(1512.0, 982.0);
        fake.state().fail_clipboard_count = true;
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.state().fail_clipboard_count = false;
        sync.learn_start(platform.clipboard.as_ref());
        fake.copy("copied while sharing");
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied while sharing")));
        // Once known, trying again changes nothing.
        let known = sync.synced;
        sync.learn_start(platform.clipboard.as_ref());
        assert_eq!(sync.synced, known);
    }

    #[test]
    fn text_from_the_other_computer_is_written_and_never_sent_back() {
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        sync.receive(platform.clipboard.as_mut(), "from the PC", Instant::now())
            .expect("write");
        assert_eq!(fake.state().clipboard.as_deref(), Some("from the PC"));
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged),
            "the echo guard"
        );

        // A write that fails records nothing.
        fake.state().fail_clipboard_write = true;
        assert!(
            sync.receive(platform.clipboard.as_mut(), "lost", Instant::now())
                .is_err()
        );
        assert_eq!(fake.state().clipboard.as_deref(), Some("from the PC"));
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
    }

    /// A sync that has had text from the other computer fail to be
    /// written at `at`: `fake` still fails writes.
    fn failed_write(text: &str) -> (Platform, Fake, ClipboardSync, Instant) {
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.state().fail_clipboard_write = true;
        let at = Instant::now();
        assert!(sync.receive(platform.clipboard.as_mut(), text, at).is_err());
        (platform, fake, sync, at)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn text_that_could_not_be_written_is_tried_again_until_it_is() {
        let (mut platform, fake, mut sync, at) = failed_write("from the PC");
        let clipboard = platform.clipboard.as_mut();
        assert!(matches!(sync.retry(clipboard, at), Retried::Nothing));
        assert!(matches!(
            sync.retry(clipboard, at + ms(99)),
            Retried::Nothing
        ));
        // Still failing: the next try waits twice as long.
        assert!(matches!(
            sync.retry(clipboard, at + ms(100)),
            Retried::Failed(_)
        ));
        assert!(matches!(
            sync.retry(clipboard, at + ms(299)),
            Retried::Nothing
        ));
        fake.state().fail_clipboard_write = false;
        assert!(matches!(
            sync.retry(clipboard, at + ms(300)),
            Retried::Written
        ));
        assert_eq!(fake.state().clipboard.as_deref(), Some("from the PC"));
        // Written once, and it counts as in common: not sent back.
        assert!(matches!(
            sync.retry(clipboard, at + ms(700)),
            Retried::Nothing
        ));
        assert_eq!(sync.offer(clipboard).ok(), Some(Offer::Unchanged));
    }

    #[test]
    fn something_copied_here_meanwhile_wins_over_text_still_waiting() {
        let (mut platform, fake, mut sync, at) = failed_write("from the PC");
        fake.copy("copied here");
        fake.state().fail_clipboard_write = false;
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Superseded
        ));
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(300)),
            Retried::Nothing
        ));
        assert_eq!(fake.state().clipboard.as_deref(), Some("copied here"));
    }

    #[test]
    fn newer_text_from_the_other_computer_replaces_text_still_waiting() {
        let (mut platform, fake, mut sync, at) = failed_write("older");
        // The newer text cannot be written either: it is the one waiting.
        assert!(
            sync.receive(platform.clipboard.as_mut(), "newer", at + ms(50))
                .is_err()
        );
        fake.state().fail_clipboard_write = false;
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(150)),
            Retried::Written
        ));
        assert_eq!(fake.state().clipboard.as_deref(), Some("newer"));

        // Newer text that can be written leaves nothing waiting.
        let (mut platform, fake, mut sync, at) = failed_write("older");
        fake.state().fail_clipboard_write = false;
        sync.receive(platform.clipboard.as_mut(), "newer", at + ms(50))
            .expect("write");
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(150)),
            Retried::Nothing
        ));
        assert_eq!(fake.state().clipboard.as_deref(), Some("newer"));
    }

    #[test]
    fn text_that_cannot_be_written_is_given_up_after_two_seconds() {
        let (mut platform, fake, mut sync, at) = failed_write("from the PC");
        let mut tries = 0;
        let mut now = at;
        while now < at + RETRY_WRITE_FOR {
            if matches!(
                sync.retry(platform.clipboard.as_mut(), now),
                Retried::Failed(_)
            ) {
                tries += 1;
            }
            now += ms(10);
        }
        assert_eq!(tries, 4, "at 100, 300, 700 and 1500 ms");
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), now),
            Retried::GaveUp
        ));
        fake.state().fail_clipboard_write = false;
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), now + ms(100)),
            Retried::Nothing
        ));
        assert_eq!(fake.state().clipboard, None);
    }

    #[test]
    fn what_is_here_is_never_sent_back_over_text_that_could_not_be_written() {
        // Copied here, but its read failed, so it was never sent.
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.copy("older, copied here");
        fake.state().fail_clipboard_read = true;
        assert!(sync.offer(platform.clipboard.as_ref()).is_err());
        fake.state().fail_clipboard_read = false;
        // Newer text from the other computer cannot be written.
        fake.state().fail_clipboard_write = true;
        let at = Instant::now();
        assert!(
            sync.receive(platform.clipboard.as_mut(), "newer, from the PC", at)
                .is_err()
        );
        // Crossing back to the PC sends nothing over its newer text,
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        // nor once that text is given up.
        let mut now = at;
        while !matches!(
            sync.retry(platform.clipboard.as_mut(), now),
            Retried::GaveUp
        ) {
            now += ms(10);
        }
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert_eq!(fake.state().clipboard_reads, 0);

        // Something copied here afterwards goes as usual.
        fake.state().fail_clipboard_write = false;
        fake.copy("copied here later");
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied here later")));
    }

    #[test]
    fn something_copied_while_text_waits_goes_as_usual() {
        let (mut platform, fake, mut sync, at) = failed_write("from the PC");
        fake.copy("copied here meanwhile");
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied here meanwhile")));
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Superseded
        ));
    }

    #[test]
    fn a_pending_write_needs_a_count_to_tell_a_copy_made_here_by() {
        // The clipboard's own count cannot be read after the failure: the
        // newest one read before (as sharing started) stands in for it.
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        {
            let mut state = fake.state();
            state.fail_clipboard_write = true;
            state.fail_clipboard_count = true;
        }
        let at = Instant::now();
        assert!(
            sync.receive(platform.clipboard.as_mut(), "from the PC", at)
                .is_err()
        );
        {
            let mut state = fake.state();
            state.fail_clipboard_write = false;
            state.fail_clipboard_count = false;
        }
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Written
        ));

        // Neither is known: nothing waits, since a copy made here could
        // not be told apart.
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        {
            let mut state = fake.state();
            state.fail_clipboard_count = true;
            state.fail_clipboard_write = true;
        }
        sync.start(platform.clipboard.as_ref());
        assert!(
            sync.receive(platform.clipboard.as_mut(), "from the PC", at)
                .is_err()
        );
        {
            let mut state = fake.state();
            state.fail_clipboard_write = false;
            state.fail_clipboard_count = false;
        }
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Nothing
        ));
        assert_eq!(fake.state().clipboard, None);
    }

    /// A sync whose clipboard could not hand over what was copied here (an
    /// app slow to hand it over, say) as the cursor left, and then answered
    /// nothing at all, neither the write nor the change count, as newer
    /// text from the other computer arrived at `at`: a Mac's clipboard does
    /// that while it waits on such an app. It answers again now.
    fn write_to_a_clipboard_that_answered_nothing() -> (Platform, Fake, ClipboardSync, Instant) {
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.copy("older, copied here");
        fake.state().fail_clipboard_read = true;
        assert!(sync.offer(platform.clipboard.as_ref()).is_err());
        {
            let mut state = fake.state();
            state.fail_clipboard_read = false;
            state.fail_clipboard_write = true;
            state.fail_clipboard_count = true;
        }
        let at = Instant::now();
        assert!(
            sync.receive(platform.clipboard.as_mut(), "newer, from the PC", at)
                .is_err()
        );
        {
            let mut state = fake.state();
            state.fail_clipboard_write = false;
            state.fail_clipboard_count = false;
        }
        (platform, fake, sync, at)
    }

    #[test]
    fn text_that_came_while_the_clipboard_answered_nothing_is_written_once_it_answers() {
        let (mut platform, fake, mut sync, at) = write_to_a_clipboard_that_answered_nothing();
        // Nothing was copied here since: the newest count read stands in
        // for the one the clipboard could not say, so it is no copy.
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Written
        ));
        assert_eq!(
            fake.state().clipboard.as_deref(),
            Some("newer, from the PC")
        );
        // Written, it counts as in common, and the older text never goes.
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert_eq!(fake.state().clipboard_reads, 0);
    }

    #[test]
    fn older_text_is_never_sent_back_over_text_that_came_while_the_clipboard_answered_nothing() {
        let (mut platform, fake, mut sync, at) = write_to_a_clipboard_that_answered_nothing();
        // Not while the newer text waits,
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        // nor once it is given up because the clipboard still takes no text.
        fake.state().fail_clipboard_write = true;
        let mut now = at;
        while !matches!(
            sync.retry(platform.clipboard.as_mut(), now),
            Retried::GaveUp
        ) {
            now += ms(10);
        }
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert_eq!(fake.state().clipboard_reads, 0);

        // Something copied here afterwards goes as usual.
        fake.copy("copied here later");
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied here later")));
    }

    #[test]
    fn giving_text_up_never_sends_again_what_already_went() {
        let (mut platform, fake, mut sync, at) = failed_write("from the PC");
        // Copied here after the write failed: it goes as usual,
        fake.copy("copied here meanwhile");
        let Offer::Text { count, .. } = sync.offer(platform.clipboard.as_ref()).expect("read")
        else {
            panic!("no text");
        };
        sync.sent(count);
        // and when the waiting text is given up before a try saw the copy,
        // it does not go again.
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + RETRY_WRITE_FOR),
            Retried::GaveUp
        ));
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert_eq!(fake.state().clipboard_reads, 1, "read once");
    }

    #[test]
    fn a_write_that_ran_out_of_time_is_not_followed_by_a_wait_for_the_count() {
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        // Every call waits CLIPBOARD_TIMEOUT and fails: the write runs out
        // of time, and the count would too.
        fake.state().stall_clipboard = true;
        let before = fake.state().clipboard_calls;
        let at = Instant::now();
        assert!(
            sync.receive(platform.clipboard.as_mut(), "from the PC", at)
                .is_err()
        );
        assert_eq!(
            fake.state().clipboard_calls,
            before + 1,
            "the write, and nothing after it"
        );
        // The text still waits, against the newest count read, and goes
        // once the clipboard answers again.
        fake.state().stall_clipboard = false;
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Written
        ));
        assert_eq!(fake.state().clipboard.as_deref(), Some("from the PC"));

        // A write that fails at once is followed by a read of the count: one
        // that failed halfway may have moved it.
        fake.state().fail_clipboard_write = true;
        let before = fake.state().clipboard_calls;
        assert!(
            sync.receive(platform.clipboard.as_mut(), "again", Instant::now())
                .is_err()
        );
        assert_eq!(fake.state().clipboard_calls, before + 2);
    }

    #[test]
    fn a_try_that_cannot_read_the_count_writes_nothing() {
        let (mut platform, fake, mut sync, at) = failed_write("from the PC");
        {
            let mut state = fake.state();
            state.fail_clipboard_write = false;
            state.fail_clipboard_count = true;
        }
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(100)),
            Retried::Failed(_)
        ));
        assert_eq!(fake.state().clipboard, None, "not written blind");
        fake.state().fail_clipboard_count = false;
        assert!(matches!(
            sync.retry(platform.clipboard.as_mut(), at + ms(300)),
            Retried::Written
        ));
    }

    #[test]
    fn pending_text_never_shows_in_debug_output() {
        let (_platform, _fake, sync, _) = failed_write("clipboard-secret");
        let shown = format!("{sync:?}");
        assert!(shown.contains("PendingWrite"), "{shown}");
        assert!(!shown.contains("clipboard-secret"), "{shown}");
    }

    #[test]
    fn with_sync_off_the_clipboard_is_never_touched() {
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ClipboardSettings::OFF);
        assert!(!sync.enabled());
        // Every call would fail, so any use would show.
        {
            let mut state = fake.state();
            state.fail_clipboard_count = true;
            state.fail_clipboard_read = true;
            state.fail_clipboard_write = true;
        }
        sync.start(platform.clipboard.as_ref());
        sync.learn_start(platform.clipboard.as_ref());
        assert!(!sync.start_unknown());
        fake.copy("copied here");
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert!(
            sync.receive(platform.clipboard.as_mut(), "from the PC", Instant::now())
                .is_ok()
        );
        assert!(matches!(
            sync.retry(
                platform.clipboard.as_mut(),
                Instant::now() + RETRY_WRITE_FOR
            ),
            Retried::Nothing
        ));
        assert_eq!(fake.state().clipboard.as_deref(), Some("copied here"));
    }
}
