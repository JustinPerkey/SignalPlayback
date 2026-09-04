//! The playback clock (`docs/DESIGN.md` §11.2).
//!
//! Playback time is decoupled from frame rate: each tick measures how much
//! *wall* time passed and scales it by the transport's rate, so a dropped
//! frame costs smoothness and never position.
//!
//! [`Clock`] takes the instant from its caller rather than reading it, which
//! is what makes a playback second testable in a microsecond.

use std::time::{Duration, Instant};

/// A wall gap longer than this is a stall — an unfocused window, a breakpoint,
/// a long redraw — not elapsed playback. It is dropped rather than applied so
/// the playhead never teleports after one (§11.2).
pub const MAX_STEP: Duration = Duration::from_millis(250);

/// Monotonic virtual time.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clock {
    last: Option<Instant>,
    /// Wall time discarded by the stall clamp, for diagnostics.
    stalls: u32,
}

impl Clock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the previous tick, so the next one advances nothing. Called on
    /// play and on seek: the gap since the last tick is not playback time.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Wall seconds since the previous tick.
    ///
    /// Returns `None` for the first tick after a reset and for a tick that
    /// crossed a stall — in both cases there is no elapsed playback to apply.
    /// `now` must come from a monotonic source ([`Instant`]), so a wall-clock
    /// jump cannot move the playhead.
    pub fn tick(&mut self, now: Instant) -> Option<f64> {
        let previous = self.last.replace(now)?;
        // `Instant` is monotonic, so this saturates only if the caller passes
        // an older instant than the last one; that is a stall, not a rewind.
        let elapsed = now.saturating_duration_since(previous);
        if elapsed > MAX_STEP || elapsed.is_zero() {
            if elapsed > MAX_STEP {
                self.stalls = self.stalls.saturating_add(1);
            }
            return None;
        }
        Some(elapsed.as_secs_f64())
    }

    /// How many ticks have been dropped as stalls.
    #[must_use]
    pub fn stalls(self) -> u32 {
        self.stalls
    }

    /// Whether a tick has been taken since the last reset.
    #[must_use]
    pub fn is_running(self) -> bool {
        self.last.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tick_after_a_reset_advances_nothing() {
        let mut clock = Clock::new();
        let t0 = Instant::now();
        assert_eq!(clock.tick(t0), None);
        assert!(clock.is_running());
        let dt = clock.tick(t0 + Duration::from_millis(16)).unwrap();
        assert!((dt - 0.016).abs() < 1e-9);
    }

    #[test]
    fn a_stall_is_dropped_not_applied() {
        let mut clock = Clock::new();
        let t0 = Instant::now();
        clock.tick(t0);
        assert_eq!(clock.tick(t0 + Duration::from_secs(4)), None);
        assert_eq!(clock.stalls(), 1);
        // The clock picks straight back up from the stalled instant.
        let dt = clock
            .tick(t0 + Duration::from_secs(4) + Duration::from_millis(16))
            .unwrap();
        assert!((dt - 0.016).abs() < 1e-9);
    }

    #[test]
    fn a_zero_length_tick_is_ignored() {
        let mut clock = Clock::new();
        let t0 = Instant::now();
        clock.tick(t0);
        assert_eq!(clock.tick(t0), None);
        assert_eq!(clock.stalls(), 0);
    }

    #[test]
    fn a_reset_breaks_the_gap_across_a_pause() {
        let mut clock = Clock::new();
        let t0 = Instant::now();
        clock.tick(t0);
        clock.reset();
        assert!(!clock.is_running());
        assert_eq!(clock.tick(t0 + Duration::from_millis(100)), None);
    }
}
