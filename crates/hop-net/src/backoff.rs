//! Reconnect backoff with jitter.

use std::time::Duration;

/// Exponential backoff from `min` to `max`, each delay moved by up to 25%
/// either way so two laptops that lost each other at the same moment do
/// not keep dialling in step (docs/M2-DESIGN.md, "One connection per
/// pair").
#[derive(Debug)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
    rng: XorShift,
}

impl Backoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Backoff {
        let max = max.max(min);
        Backoff {
            min,
            max,
            next: min,
            rng: XorShift::seeded(),
        }
    }

    /// The delay before the next attempt; each call doubles the following
    /// one, up to `max`.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let base = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        jitter(base, self.rng.next_unit())
    }

    /// Starts again from `min`, after a connection stayed up long enough.
    pub(crate) fn reset(&mut self) {
        self.next = self.min;
    }
}

/// `base` scaled by a factor between 0.75 and 1.25, chosen by `unit` in
/// `[0, 1)`.
fn jitter(base: Duration, unit: f64) -> Duration {
    base.mul_f64(0.75 + 0.5 * unit.clamp(0.0, 1.0))
}

/// A small non-cryptographic generator (xorshift64*). Jitter needs spread,
/// not secrecy; the seed comes from the OS so the two laptops differ.
#[derive(Debug)]
struct XorShift(u64);

impl XorShift {
    fn seeded() -> XorShift {
        let mut seed = [0u8; 8];
        if getrandom::fill(&mut seed).is_err() {
            // Without OS randomness, fall back to the clock: still differs
            // between the two laptops.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or(1);
            seed = u64::from(nanos).to_le_bytes();
        }
        // Zero is the one state xorshift never leaves.
        XorShift(u64::from_le_bytes(seed) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `[0, 1)`.
    fn next_unit(&mut self) -> f64 {
        // The top 53 bits fill an f64 mantissa exactly.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_the_cap_within_the_jitter() {
        let min = Duration::from_millis(250);
        let max = Duration::from_secs(5);
        let mut backoff = Backoff::new(min, max);
        let bases = [250, 500, 1000, 2000, 4000, 5000, 5000];
        for base in bases {
            let delay = backoff.next_delay().as_secs_f64() * 1000.0;
            let base = f64::from(base);
            assert!(
                delay >= base * 0.75 - 1e-6 && delay <= base * 1.25 + 1e-6,
                "{delay} vs {base}"
            );
        }
        backoff.reset();
        assert!(backoff.next_delay() <= Duration::from_micros(312_500));
    }

    #[test]
    fn jitter_spans_both_directions() {
        let base = Duration::from_secs(1);
        assert_eq!(jitter(base, 0.0), Duration::from_millis(750));
        assert_eq!(jitter(base, 0.5), base);
        assert!(jitter(base, 0.999_999) > Duration::from_millis(1249));
        let mut rng = XorShift::seeded();
        let samples: Vec<f64> = (0..1000).map(|_| rng.next_unit()).collect();
        assert!(samples.iter().all(|s| (0.0..1.0).contains(s)));
        assert!(samples.iter().any(|s| *s < 0.25) && samples.iter().any(|s| *s > 0.75));
    }
}
