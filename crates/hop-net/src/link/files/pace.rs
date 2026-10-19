//! Pacing file data, so a large transfer never sends faster than
//! `Limits::file_rate` and does not fill the Wi-Fi queue in front of the
//! input (docs/M4-M5-DESIGN.md, "Pace").

use std::time::Duration;

use tokio::time::Instant;

/// How much sending a pause earns: after this long without data, the next
/// writes may go at once until they catch up with the rate again.
const BURST: Duration = Duration::from_millis(20);

/// A token bucket in virtual time: every write books the time its bytes
/// take at the rate, and a write waits until its booking ends.
#[derive(Debug)]
pub(super) struct Pacer {
    /// Bytes per second; zero turns pacing off.
    rate: u64,
    /// When the bytes booked so far are sent, at the rate.
    booked_until: Option<Instant>,
}

impl Pacer {
    /// A pacer for `rate` bytes per second (zero: no pacing).
    pub(super) fn new(rate: u64) -> Pacer {
        Pacer {
            rate,
            booked_until: None,
        }
    }

    /// Books `bytes` at `now` and returns how long to wait before writing
    /// them. Over any stretch of time, at most `rate` bytes per second plus
    /// what 20 ms at the rate allows go out.
    pub(super) fn delay(&mut self, now: Instant, bytes: usize) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        let cost = Duration::from_secs_f64(bytes as f64 / self.rate as f64);
        // A pause is credited only up to the burst.
        let earliest = now.checked_sub(BURST).unwrap_or(now);
        let start = self
            .booked_until
            .map_or(earliest, |booked| booked.max(earliest));
        let end = start + cost;
        self.booked_until = Some(end);
        end.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_steady_stream_goes_at_the_rate() {
        let mut pacer = Pacer::new(MIB);
        let mut now = Instant::now();
        let chunk = 64 * 1024;
        let mut waited = Duration::ZERO;
        // 16 chunks are one MiB: one second at the rate, minus the burst.
        for _ in 0..16 {
            let delay = pacer.delay(now, chunk);
            waited += delay;
            now += delay;
        }
        let expected = Duration::from_secs(1) - BURST;
        let error = waited.abs_diff(expected);
        assert!(error < Duration::from_millis(2), "waited {waited:?}");
    }

    #[test]
    fn a_pause_earns_only_a_short_burst() {
        let mut pacer = Pacer::new(MIB);
        let start = Instant::now();
        pacer.delay(start, 64 * 1024);
        // A long pause, then a big write: only 20 ms of credit.
        let later = start + Duration::from_secs(60);
        let delay = pacer.delay(later, MIB as usize);
        let expected = Duration::from_secs(1) - BURST;
        assert!(
            delay.abs_diff(expected) < Duration::from_millis(2),
            "{delay:?}"
        );
    }

    #[test]
    fn small_writes_after_a_pause_go_at_once() {
        let mut pacer = Pacer::new(12 * MIB);
        let now = Instant::now() + Duration::from_secs(1);
        // 20 ms at 12 MiB/s is about 245 KiB.
        assert_eq!(pacer.delay(now, 64 * 1024), Duration::ZERO);
        assert_eq!(pacer.delay(now, 64 * 1024), Duration::ZERO);
        assert_eq!(pacer.delay(now, 64 * 1024), Duration::ZERO);
        assert!(pacer.delay(now, 64 * 1024) > Duration::ZERO);
    }

    #[test]
    fn a_zero_rate_never_waits() {
        let mut pacer = Pacer::new(0);
        let now = Instant::now();
        for _ in 0..100 {
            assert_eq!(pacer.delay(now, usize::MAX), Duration::ZERO);
        }
    }
}
