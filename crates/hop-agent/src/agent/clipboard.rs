//! Text clipboard sync (docs/M4-M5-DESIGN.md, section 4), on the core
//! thread.
//!
//! Text travels when the cursor leaves a computer, never on every copy: the
//! controller offers its clipboard right after it sends `Enter`, and the
//! controlled side right after it sends `Leave`. Both go on the ordered
//! control stream, so the text always arrives before the next key, and
//! Ctrl+V right after crossing pastes it. The receiving side writes it
//! before it handles the next event.
//!
//! [`ClipboardSync`] decides what to read, send and remember; the executor
//! hands it the platform's clipboard and carries out the sending. Every
//! clipboard call returns within [`hop_platform::CLIPBOARD_TIMEOUT`], so
//! the core thread waits at most that long for one.
//!
//! What is remembered is the change count both computers last had in
//! common, never the text:
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
//! Nothing here logs.

use hop_platform::{PlatformError, TextClipboard};
use hop_types::CoreMessage;

/// Said once for each connection that carries clipboard text, so the user
/// always knows that what they copy can travel. Nothing is said per copy.
pub(crate) fn works_with(peer: &str) -> String {
    format!("Copy and paste now work between this computer and {peer}.")
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

/// The core thread's side of clipboard sync: whether it is on, and the
/// change count both computers last had in common.
#[derive(Debug)]
pub(crate) struct ClipboardSync {
    settings: ClipboardSettings,
    /// The clipboard's change count when both computers last had the same
    /// content: after this computer sent its text (or found nothing to
    /// send), or right after it wrote the other computer's text. `None`
    /// until known, and then the next crossing offers whatever the
    /// clipboard holds.
    synced: Option<u64>,
    /// The generation of the last text offered.
    generation: u64,
}

impl ClipboardSync {
    /// Sync with `settings`. Call [`ClipboardSync::start`] as sharing starts.
    pub(crate) fn new(settings: ClipboardSettings) -> ClipboardSync {
        ClipboardSync {
            settings,
            synced: None,
            generation: 0,
        }
    }

    /// Whether text travels at all.
    pub(crate) fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Remembers the clipboard's change count as sharing starts, so a
    /// clipboard from before is never pushed at the first crossing. When the
    /// clipboard cannot say, the count stays unknown, and the first crossing
    /// offers whatever it holds then.
    pub(crate) fn start(&mut self, clipboard: &dyn TextClipboard) {
        if self.settings.enabled {
            self.synced = clipboard.change_count().ok();
        }
    }

    /// What to send the other computer as the cursor leaves for it: the
    /// clipboard's text, if the clipboard changed since the two computers
    /// last had the same content. Unchanged, it is not even read.
    ///
    /// # Errors
    ///
    /// The clipboard could not be read (it took too long, say). Nothing is
    /// recorded then, so the next crossing tries again.
    pub(crate) fn offer(&mut self, clipboard: &dyn TextClipboard) -> Result<Offer, PlatformError> {
        if !self.settings.enabled {
            return Ok(Offer::Unchanged);
        }
        let count = clipboard.change_count()?;
        if self.synced == Some(count) {
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
    /// not send the text straight back. Does nothing while sync is off.
    ///
    /// # Errors
    ///
    /// The write failed. Nothing is recorded then.
    pub(crate) fn receive(
        &mut self,
        clipboard: &mut dyn TextClipboard,
        text: &str,
    ) -> Result<(), PlatformError> {
        if self.settings.enabled {
            self.synced = Some(clipboard.write_text(text)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
    fn an_unknown_count_at_the_start_offers_whatever_the_clipboard_holds() {
        let (platform, fake) = Fake::new(1512.0, 982.0);
        fake.copy("copied before sharing started");
        fake.state().fail_clipboard_count = true;
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        fake.state().fail_clipboard_count = false;
        let offer = sync.offer(platform.clipboard.as_ref()).expect("read");
        assert_eq!(text(&offer), Some((1, "copied before sharing started")));
    }

    #[test]
    fn text_from_the_other_computer_is_written_and_never_sent_back() {
        let (mut platform, fake) = Fake::new(1512.0, 982.0);
        let mut sync = ClipboardSync::new(ON);
        sync.start(platform.clipboard.as_ref());
        sync.receive(platform.clipboard.as_mut(), "from the PC")
            .expect("write");
        assert_eq!(fake.state().clipboard.as_deref(), Some("from the PC"));
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged),
            "the echo guard"
        );

        // A write that fails records nothing.
        fake.state().fail_clipboard_write = true;
        assert!(sync.receive(platform.clipboard.as_mut(), "lost").is_err());
        assert_eq!(fake.state().clipboard.as_deref(), Some("from the PC"));
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
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
        fake.copy("copied here");
        assert_eq!(
            sync.offer(platform.clipboard.as_ref()).ok(),
            Some(Offer::Unchanged)
        );
        assert!(
            sync.receive(platform.clipboard.as_mut(), "from the PC")
                .is_ok()
        );
        assert_eq!(fake.state().clipboard.as_deref(), Some("copied here"));
    }
}
