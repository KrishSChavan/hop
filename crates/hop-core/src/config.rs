//! Tuning knobs of the hand-off state machine.

use crate::Millis;
use crate::chord::Chord;

/// Settings the core runs with. `CoreConfig::default()` gives the values from
/// docs/ARCHITECTURE.md.
///
/// User-facing validation belongs to `hop-config`. The core still never trusts
/// these numbers blindly: [`Core::new`](crate::Core::new) replaces any value it
/// cannot use (a negative or non-finite distance, a pointer speed that is not
/// positive, a timeout or heartbeat interval of zero, an empty input buffer)
/// with its default, and shortens a heartbeat interval longer than a third of
/// the peer timeout, so a bad config file can never make it panic or
/// misbehave.
#[derive(Clone, Debug, PartialEq)]
pub struct CoreConfig {
    /// How long the cursor must keep pushing against an edge before it
    /// crosses, in milliseconds. 0 crosses immediately.
    pub edge_delay_ms: Millis,
    /// Pushing within this many units of either end of an edge never crosses,
    /// so screen corners (hot corners, the Start button) stay usable.
    /// 0 disables the dead zone.
    pub corner_dead_zone: f64,
    /// Displays of different devices count as touching when their edges are
    /// at most this many units apart.
    pub adjacency_tolerance: f64,
    /// Multiplier applied to the controller's motion when this device is
    /// controlled.
    pub pointer_speed: f64,
    /// Physical pointer travel on a controlled device, in units since the
    /// session began, that takes control back.
    pub takeback_threshold: f64,
    /// How long to wait for `EnterAck` before giving up on a crossing.
    pub enter_timeout_ms: Millis,
    /// A `Heartbeat` is sent when nothing reliable went to the session peer
    /// for this long. At most a third of `peer_timeout_ms`.
    pub heartbeat_interval_ms: Millis,
    /// A session ends when nothing arrives from its peer for this long.
    pub peer_timeout_ms: Millis,
    /// Most input events kept while waiting for `EnterAck`.
    pub enter_buffer_limit: usize,
    /// The emergency chord that ends a session from the controller.
    pub emergency_chord: Chord,
}

impl CoreConfig {
    /// The default edge delay: cross as soon as the cursor pushes.
    pub const DEFAULT_EDGE_DELAY_MS: Millis = 0;
    /// The default corner dead zone, in logical units.
    pub const DEFAULT_CORNER_DEAD_ZONE: f64 = 10.0;
    /// The default adjacency tolerance, in logical units.
    pub const DEFAULT_ADJACENCY_TOLERANCE: f64 = 1.0;
    /// The default pointer speed multiplier.
    pub const DEFAULT_POINTER_SPEED: f64 = 1.0;
    /// The default take-back threshold, in logical units.
    pub const DEFAULT_TAKEBACK_THRESHOLD: f64 = 4.0;
    /// The default enter timeout.
    pub const DEFAULT_ENTER_TIMEOUT_MS: Millis = 500;
    /// The default heartbeat interval.
    pub const DEFAULT_HEARTBEAT_INTERVAL_MS: Millis = 250;
    /// The default peer timeout.
    pub const DEFAULT_PEER_TIMEOUT_MS: Millis = 1500;
    /// The default size of the input buffer used while entering.
    pub const DEFAULT_ENTER_BUFFER_LIMIT: usize = 64;

    /// This config with every unusable value replaced by its default.
    pub(crate) fn sanitized(mut self) -> Self {
        fn distance(value: f64, default: f64) -> f64 {
            if value.is_finite() && value >= 0.0 {
                value
            } else {
                default
            }
        }
        self.corner_dead_zone = distance(self.corner_dead_zone, Self::DEFAULT_CORNER_DEAD_ZONE);
        self.adjacency_tolerance =
            distance(self.adjacency_tolerance, Self::DEFAULT_ADJACENCY_TOLERANCE);
        self.takeback_threshold =
            distance(self.takeback_threshold, Self::DEFAULT_TAKEBACK_THRESHOLD);
        if !(self.pointer_speed.is_finite() && self.pointer_speed > 0.0) {
            self.pointer_speed = Self::DEFAULT_POINTER_SPEED;
        }
        if self.enter_buffer_limit == 0 {
            // A zero-sized buffer would abort every crossing that sees any input.
            self.enter_buffer_limit = Self::DEFAULT_ENTER_BUFFER_LIMIT;
        }
        // Zero timeouts would end every crossing and session at the next
        // event, and a zero interval would send a heartbeat with every one.
        for (value, default) in [
            (&mut self.enter_timeout_ms, Self::DEFAULT_ENTER_TIMEOUT_MS),
            (
                &mut self.heartbeat_interval_ms,
                Self::DEFAULT_HEARTBEAT_INTERVAL_MS,
            ),
            (&mut self.peer_timeout_ms, Self::DEFAULT_PEER_TIMEOUT_MS),
        ] {
            if *value == 0 {
                *value = default;
            }
        }
        // The peer must hear a few heartbeats per timeout, or every quiet
        // session ends although both sides are fine. Fall back to the
        // defaults' ratio, one heartbeat per sixth of the timeout.
        if self.heartbeat_interval_ms > self.peer_timeout_ms / 3 {
            self.heartbeat_interval_ms =
                (self.peer_timeout_ms / 6).clamp(1, Self::DEFAULT_HEARTBEAT_INTERVAL_MS);
        }
        self
    }
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            edge_delay_ms: Self::DEFAULT_EDGE_DELAY_MS,
            corner_dead_zone: Self::DEFAULT_CORNER_DEAD_ZONE,
            adjacency_tolerance: Self::DEFAULT_ADJACENCY_TOLERANCE,
            pointer_speed: Self::DEFAULT_POINTER_SPEED,
            takeback_threshold: Self::DEFAULT_TAKEBACK_THRESHOLD,
            enter_timeout_ms: Self::DEFAULT_ENTER_TIMEOUT_MS,
            heartbeat_interval_ms: Self::DEFAULT_HEARTBEAT_INTERVAL_MS,
            peer_timeout_ms: Self::DEFAULT_PEER_TIMEOUT_MS,
            enter_buffer_limit: Self::DEFAULT_ENTER_BUFFER_LIMIT,
            emergency_chord: Chord::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let c = CoreConfig::default();
        assert_eq!(c.enter_timeout_ms, 500);
        assert_eq!(c.heartbeat_interval_ms, 250);
        assert_eq!(c.peer_timeout_ms, 1500);
        assert_eq!(c.takeback_threshold, 4.0);
        assert_eq!(c.enter_buffer_limit, 64);
        assert_eq!(c.edge_delay_ms, 0);
    }

    #[test]
    fn sanitizing_replaces_unusable_values() {
        let c = CoreConfig {
            corner_dead_zone: f64::NAN,
            adjacency_tolerance: -1.0,
            pointer_speed: 0.0,
            takeback_threshold: f64::INFINITY,
            enter_buffer_limit: 0,
            enter_timeout_ms: 0,
            heartbeat_interval_ms: 0,
            peer_timeout_ms: 0,
            ..CoreConfig::default()
        }
        .sanitized();
        assert_eq!(c, CoreConfig::default());
    }

    #[test]
    fn sanitizing_keeps_heartbeats_well_inside_the_peer_timeout() {
        // A heartbeat at or near the peer timeout would end every quiet
        // session: the peer gives up before the next heartbeat arrives.
        for interval in [2000, 1500, 501] {
            let c = CoreConfig {
                heartbeat_interval_ms: interval,
                peer_timeout_ms: 1500,
                ..CoreConfig::default()
            }
            .sanitized();
            assert_eq!(c.heartbeat_interval_ms, 250, "{interval}");
        }
        // Three heartbeats per timeout are enough, so this one stays.
        let c = CoreConfig {
            heartbeat_interval_ms: 500,
            peer_timeout_ms: 1500,
            ..CoreConfig::default()
        };
        assert_eq!(c.clone().sanitized(), c);
        // With a short timeout the interval shrinks to match.
        let c = CoreConfig {
            peer_timeout_ms: 300,
            ..CoreConfig::default()
        }
        .sanitized();
        assert_eq!(c.heartbeat_interval_ms, 50);
    }

    #[test]
    fn sanitizing_keeps_usable_values() {
        let c = CoreConfig {
            corner_dead_zone: 0.0,
            pointer_speed: 2.5,
            edge_delay_ms: 150,
            ..CoreConfig::default()
        };
        assert_eq!(c.clone().sanitized(), c);
    }
}
