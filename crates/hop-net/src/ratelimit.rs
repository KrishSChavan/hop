//! Rate limiting for warnings that strangers can trigger.
//!
//! Any device on the network can cause failed handshakes, so warnings about
//! them must not be able to flood the log (docs/M2-DESIGN.md, "Logs").

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// At most one warning of each kind per this interval.
const INTERVAL: Duration = Duration::from_secs(10);

/// At most this many kinds are tracked. Kinds are static strings from this
/// crate, so this is never reached; it keeps the table bounded regardless.
const MAX_KINDS: usize = 64;

struct Entry {
    kind: &'static str,
    last: Instant,
    suppressed: u64,
}

static RECENT: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Whether a warning of `kind` may be logged now. `Some(n)` means yes, and
/// `n` warnings of that kind were suppressed since the last one logged.
pub(crate) fn allow(kind: &'static str) -> Option<u64> {
    let now = Instant::now();
    let mut recent = RECENT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(entry) = recent.iter_mut().find(|entry| entry.kind == kind) {
        if now.duration_since(entry.last) < INTERVAL {
            entry.suppressed = entry.suppressed.saturating_add(1);
            return None;
        }
        let suppressed = entry.suppressed;
        entry.last = now;
        entry.suppressed = 0;
        return Some(suppressed);
    }
    if recent.len() >= MAX_KINDS {
        recent.remove(0);
    }
    recent.push(Entry {
        kind,
        last: now,
        suppressed: 0,
    });
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_are_suppressed_and_counted() {
        assert_eq!(allow("test kind a"), Some(0));
        assert_eq!(allow("test kind a"), None);
        assert_eq!(allow("test kind a"), None);
        assert_eq!(allow("test kind b"), Some(0), "kinds are independent");
    }
}
