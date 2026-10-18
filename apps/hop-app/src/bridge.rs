//! How news reaches the window: the log's status lines, the agent's
//! updates and the pairing flow's events.
//!
//! Each arrives on another thread. A bridge thread (one per agent run or
//! pairing session) drains its channel into [`Shared`], and the log's tap
//! adds status lines there; both then ask the window to repaint. The window
//! takes everything out at the start of its next frame, on its own thread.
//! Nothing here ever waits for the window, and the window never waits for
//! the network.

use std::collections::VecDeque;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;

use hop_agent::pairing::PairEvent;
use hop_agent::service::AgentUpdate;

use crate::model::ACTIVITY_LINES;

/// News for the window, tagged with the run or session it belongs to, so
/// the last words of one that already ended are not mistaken for the next
/// one's.
pub enum News {
    /// From the agent's run with this number.
    Agent(u64, AgentUpdate),
    /// From the pairing session with this number.
    Pair(u64, PairEvent),
}

/// What the window's thread and the other threads share.
pub struct Shared {
    queue: Mutex<Queue>,
    /// The window's context, once it exists, to ask it for a repaint.
    window: OnceLock<eframe::egui::Context>,
}

#[derive(Default)]
struct Queue {
    /// Status lines, at most [`ACTIVITY_LINES`]: the list keeps no more.
    lines: VecDeque<String>,
    /// Updates and events, all of them: each one moves a state on.
    news: VecDeque<News>,
}

impl Shared {
    /// An empty queue, with no window yet.
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            window: OnceLock::new(),
        })
    }

    /// Tells the queue which window to wake.
    pub fn set_window(&self, ctx: &eframe::egui::Context) {
        let _ = self.window.set(ctx.clone());
        ctx.request_repaint();
    }

    /// The window's context, once it exists.
    pub fn window(&self) -> Option<&eframe::egui::Context> {
        self.window.get()
    }

    /// Adds a status line for the Activity list. Called on the log's
    /// writer thread (and so never from inside the log), through [`tap`].
    pub fn push_line(&self, line: &str) {
        {
            let mut queue = self.lock();
            queue.lines.push_back(line.to_owned());
            while queue.lines.len() > ACTIVITY_LINES {
                queue.lines.pop_front();
            }
        }
        self.wake();
    }

    /// Adds an update or an event.
    pub fn push_news(&self, news: News) {
        self.lock().news.push_back(news);
        self.wake();
    }

    /// Everything that arrived since the last call: status lines first.
    pub fn take(&self) -> (VecDeque<String>, VecDeque<News>) {
        let mut queue = self.lock();
        (
            std::mem::take(&mut queue.lines),
            std::mem::take(&mut queue.news),
        )
    }

    /// Asks the window to repaint, which applies what is queued. Before the
    /// window exists there is nothing to wake: its first frame takes the
    /// queue anyway.
    fn wake(&self) {
        if let Some(ctx) = self.window.get() {
            ctx.request_repaint();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        // A panic elsewhere must not cut the window off from its news.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The log's tap for the Activity list: each status line goes to `shared`,
/// on the log's writer thread. A panic while adding a line (waking the
/// window, say) is caught, so that thread goes on writing the log and
/// feeding the list.
pub fn tap(shared: Arc<Shared>) -> impl FnMut(&str) + Send + 'static {
    move |line| {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| shared.push_line(line)));
    }
}

/// Starts a bridge thread named `name` that hands everything from `from`
/// to `shared`, wrapped by `wrap`, until the channel closes (its thread
/// ended after its last word).
///
/// A panic while handing one item over (waking the window, say) does not
/// end the bridge: the items after it, the last word above all, still
/// arrive, so the window never waits for a `Stopped` or `Finished` that
/// cannot come. The panic hook has stopped sharing by then.
///
/// # Errors
///
/// The thread could not start.
pub fn forward<T: Send + 'static>(
    name: &str,
    from: Receiver<T>,
    shared: Arc<Shared>,
    wrap: impl Fn(T) -> News + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            for item in from {
                let _ = panic::catch_unwind(AssertUnwindSafe(|| shared.push_news(wrap(item))));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn status_lines_are_capped_and_news_is_kept_in_order() {
        let shared = Shared::new();
        for n in 0..(ACTIVITY_LINES + 20) {
            shared.push_line(&format!("line {n}"));
        }
        shared.push_news(News::Agent(1, AgentUpdate::Disconnected));
        shared.push_news(News::Agent(1, AgentUpdate::Stopped { error: None }));
        let (lines, news) = shared.take();
        assert_eq!(lines.len(), ACTIVITY_LINES);
        assert_eq!(lines.front().map(String::as_str), Some("line 20"));
        assert!(matches!(
            news.iter().collect::<Vec<_>>().as_slice(),
            [
                News::Agent(1, AgentUpdate::Disconnected),
                News::Agent(1, AgentUpdate::Stopped { .. })
            ]
        ));
        let (lines, news) = shared.take();
        assert!(lines.is_empty() && news.is_empty(), "taken once");
    }

    #[test]
    fn a_bridge_forwards_until_its_channel_closes() {
        let shared = Shared::new();
        let (tx, rx) = mpsc::channel();
        forward("hop-app-test-bridge", rx, shared.clone(), |update| {
            News::Agent(7, update)
        })
        .expect("thread");
        tx.send(AgentUpdate::Disconnected).expect("send");
        tx.send(AgentUpdate::Stopped { error: None }).expect("send");
        drop(tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < 2 && Instant::now() < deadline {
            got.extend(shared.take().1);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            got.as_slice(),
            [
                News::Agent(7, AgentUpdate::Disconnected),
                News::Agent(7, AgentUpdate::Stopped { error: None })
            ]
        ));
    }

    #[test]
    fn a_panic_while_handing_over_one_item_does_not_end_the_bridge() {
        let shared = Shared::new();
        let (tx, rx) = mpsc::channel();
        forward("hop-app-test-bridge", rx, shared.clone(), |update| {
            if update == AgentUpdate::Disconnected {
                panic!("a test panic while handing an update over");
            }
            News::Agent(7, update)
        })
        .expect("thread");
        tx.send(AgentUpdate::Disconnected).expect("send");
        tx.send(AgentUpdate::Stopped { error: None }).expect("send");
        drop(tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.is_empty() && Instant::now() < deadline {
            got.extend(shared.take().1);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(
                got.as_slice(),
                [News::Agent(7, AgentUpdate::Stopped { error: None })]
            ),
            "the last word still arrives"
        );
    }

    #[test]
    fn the_logs_tap_feeds_the_activity_list() {
        let shared = Shared::new();
        let mut feed = tap(shared.clone());
        feed("Hop is starting.");
        feed("Hop stopped.");
        let (lines, news) = shared.take();
        assert_eq!(lines, ["Hop is starting.", "Hop stopped."]);
        assert!(news.is_empty());
    }
}
