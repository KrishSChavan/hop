//! Motion-mark accounting (docs/DECISIONS.md, ADR-7).
//!
//! The controller sends cumulative motion over an unreliable channel. Because
//! every mark carries the running totals, the receiver can always compute the
//! motion it missed, and an `Input` message carrying the mark lets a click
//! land where it should even if motion datagrams were lost.

use hop_types::MotionMark;

/// The controller's side: running totals of the motion sent in a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MarkSender {
    mark: MotionMark,
}

impl MarkSender {
    /// Adds one motion and returns the new mark to send.
    pub fn push(&mut self, dx: f64, dy: f64) -> MotionMark {
        self.mark.n = self.mark.n.saturating_add(1);
        self.mark.total_dx += dx;
        self.mark.total_dy += dy;
        self.mark
    }

    /// The mark to attach to an `Input` message.
    pub fn current(&self) -> MotionMark {
        self.mark
    }
}

/// The controlled side: the last mark applied in a session.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MarkReceiver {
    applied: MotionMark,
}

impl MarkReceiver {
    /// The motion still to apply to reach `mark`, or `None` when `mark` is
    /// not newer than what was already applied (a late or duplicate
    /// datagram) or is unusable (non-finite totals from a broken peer, which
    /// would otherwise poison every later delta).
    pub fn catch_up(&mut self, mark: MotionMark) -> Option<(f64, f64)> {
        if mark.n <= self.applied.n {
            return None;
        }
        let dx = mark.total_dx - self.applied.total_dx;
        let dy = mark.total_dy - self.applied.total_dy;
        if !(dx.is_finite() && dy.is_finite()) {
            return None;
        }
        self.applied = mark;
        Some((dx, dy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(n: u64, total_dx: f64, total_dy: f64) -> MotionMark {
        MotionMark {
            n,
            total_dx,
            total_dy,
        }
    }

    #[test]
    fn sender_accumulates() {
        let mut s = MarkSender::default();
        assert_eq!(s.current(), mark(0, 0.0, 0.0));
        assert_eq!(s.push(3.0, -1.0), mark(1, 3.0, -1.0));
        assert_eq!(s.push(2.0, 4.0), mark(2, 5.0, 3.0));
        assert_eq!(s.current(), mark(2, 5.0, 3.0));
    }

    #[test]
    fn receiver_applies_only_newer_marks() {
        let mut r = MarkReceiver::default();
        assert_eq!(r.catch_up(mark(1, 3.0, -1.0)), Some((3.0, -1.0)));
        // Mark 2 was lost; mark 3 carries everything since mark 1.
        assert_eq!(r.catch_up(mark(3, 10.0, 0.0)), Some((7.0, 1.0)));
        // The late mark 2 and a duplicate 3 change nothing.
        assert_eq!(r.catch_up(mark(2, 5.0, 3.0)), None);
        assert_eq!(r.catch_up(mark(3, 10.0, 0.0)), None);
        assert_eq!(r.catch_up(mark(4, 10.0, 0.0)), Some((0.0, 0.0)));
    }

    #[test]
    fn receiver_skips_non_finite_marks() {
        let mut r = MarkReceiver::default();
        assert_eq!(r.catch_up(mark(1, 2.0, 2.0)), Some((2.0, 2.0)));
        assert_eq!(r.catch_up(mark(2, f64::NAN, 0.0)), None);
        assert_eq!(r.catch_up(mark(3, f64::INFINITY, 0.0)), None);
        // A later good mark still yields the right delta.
        assert_eq!(r.catch_up(mark(4, 5.0, 2.0)), Some((3.0, 0.0)));
    }
}
