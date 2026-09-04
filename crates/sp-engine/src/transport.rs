//! The transport state machine (`docs/DESIGN.md` §11.1).
//!
//! ```text
//!                  ┌──────────┐  play   ┌──────────┐
//!         seek ───►│ Stopped  ├────────►│ Playing  │◄─── rate change
//!                  └────▲─────┘         └────┬─────┘
//!                       │ stop               │ pause
//!                       │              ┌─────▼────┐
//!                       └──────────────┤  Paused  │
//!                              stop    └──────────┘
//! ```
//!
//! `Playing` advances the playhead, `Paused` retains it, `Stopped` resets it
//! to the loop start. Seek is legal in every state and changes none of them.

use sp_core::TimeRange;

/// Slowest and fastest playback rates (§11.2). A rate may be negative, which
/// plays backwards.
pub const MIN_RATE: f64 = 0.01;
pub const MAX_RATE: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl TransportState {
    #[must_use]
    pub const fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Playing => "Playing",
            Self::Paused => "Paused",
        }
    }
}

/// What happens when the playhead reaches the end of the loop range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    /// Clamp at the end and pause.
    #[default]
    Once,
    /// Wrap around to the loop start.
    Loop,
    /// Reverse direction at each end.
    PingPong,
}

impl LoopMode {
    pub const ALL: [Self; 3] = [Self::Once, Self::Loop, Self::PingPong];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Once => "Once",
            Self::Loop => "Loop",
            Self::PingPong => "Ping-pong",
        }
    }
}

impl std::fmt::Display for LoopMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Playhead, rate and loop policy over one absolute timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transport {
    state: TransportState,
    playhead_s: f64,
    /// Playback speed. Negative plays backwards; the magnitude is clamped
    /// into `[MIN_RATE, MAX_RATE]`.
    rate: f64,
    loop_mode: LoopMode,
    /// The span playback runs over — the whole timeline until the user sets
    /// loop points with `[` and `]`.
    range: TimeRange,
    /// Ping-pong direction: `+1` forwards, `-1` backwards. Kept apart from
    /// `rate` so reversing at a loop end does not rewrite the user's rate.
    direction: f64,
}

impl Default for Transport {
    fn default() -> Self {
        Self::new(TimeRange::new(0.0, 0.0))
    }
}

impl Transport {
    #[must_use]
    pub fn new(range: TimeRange) -> Self {
        Self {
            state: TransportState::Stopped,
            playhead_s: range.start_s,
            rate: 1.0,
            loop_mode: LoopMode::default(),
            range,
            direction: 1.0,
        }
    }

    #[must_use]
    pub fn state(&self) -> TransportState {
        self.state
    }

    #[must_use]
    pub fn playhead_s(&self) -> f64 {
        self.playhead_s
    }

    #[must_use]
    pub fn rate(&self) -> f64 {
        self.rate
    }

    #[must_use]
    pub fn loop_mode(&self) -> LoopMode {
        self.loop_mode
    }

    #[must_use]
    pub fn range(&self) -> TimeRange {
        self.range
    }

    /// Playhead position within the loop range, as a fraction in `[0, 1]`.
    #[must_use]
    pub fn progress(&self) -> f64 {
        self.range.fraction_of(self.playhead_s)
    }

    /// Starts or resumes playback. A transport whose range is empty has
    /// nothing to play and stays stopped.
    pub fn play(&mut self) {
        if self.range.is_empty() {
            return;
        }
        // Playing on from the end would advance nothing; restart instead.
        if self.state != TransportState::Playing && self.at_end() {
            self.playhead_s = self.start_for_direction();
        }
        self.state = TransportState::Playing;
    }

    pub fn pause(&mut self) {
        if self.state == TransportState::Playing {
            self.state = TransportState::Paused;
        }
    }

    /// Play when paused or stopped, pause when playing — the space bar.
    pub fn toggle(&mut self) {
        if self.state.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Stops and returns the playhead to the loop start (§11.1).
    pub fn stop(&mut self) {
        self.state = TransportState::Stopped;
        self.direction = 1.0;
        self.playhead_s = self.range.start_s;
    }

    /// Moves the playhead. Legal in every state and changes none of them.
    pub fn seek(&mut self, t_s: f64) {
        self.playhead_s = if self.range.is_empty() {
            self.range.start_s
        } else {
            self.range.clamp(t_s)
        };
    }

    /// Seeks by a fraction of the loop range, for a scrub bar.
    pub fn seek_fraction(&mut self, fraction: f64) {
        let f = fraction.clamp(0.0, 1.0);
        self.seek(self.range.start_s + self.range.duration_s() * f);
    }

    /// Sets the playback rate, clamping its magnitude to
    /// `[MIN_RATE, MAX_RATE]`. Does not change the transport state (§11.1).
    pub fn set_rate(&mut self, rate: f64) {
        if !rate.is_finite() || rate == 0.0 {
            return;
        }
        let magnitude = rate.abs().clamp(MIN_RATE, MAX_RATE);
        self.rate = magnitude.copysign(rate);
    }

    pub fn set_loop_mode(&mut self, mode: LoopMode) {
        self.loop_mode = mode;
        if mode != LoopMode::PingPong {
            self.direction = 1.0;
        }
    }

    /// Replaces the playable span, keeping the playhead inside it.
    ///
    /// Called when the trace set changes: the timeline is the union of what is
    /// loaded, and a playhead outside it would be unreachable.
    pub fn set_range(&mut self, range: TimeRange) {
        self.range = range;
        self.playhead_s = if range.is_empty() {
            range.start_s
        } else {
            range.clamp(self.playhead_s)
        };
        if range.is_empty() {
            self.state = TransportState::Stopped;
        }
    }

    /// Sets the loop in-point, keeping the out-point.
    pub fn set_loop_start(&mut self, t_s: f64) {
        self.set_range(TimeRange::new(t_s.min(self.range.end_s), self.range.end_s));
    }

    /// Sets the loop out-point, keeping the in-point.
    pub fn set_loop_end(&mut self, t_s: f64) {
        self.set_range(TimeRange::new(
            self.range.start_s,
            t_s.max(self.range.start_s),
        ));
    }

    /// Applies `dt_wall_s` seconds of wall time, scaled by the rate, and
    /// resolves the loop policy at the ends (§11.2).
    ///
    /// Does nothing unless playing, so a paused transport is free to tick.
    pub fn advance(&mut self, dt_wall_s: f64) {
        if !self.state.is_playing() || !dt_wall_s.is_finite() || self.range.is_empty() {
            return;
        }
        let step = dt_wall_s * self.rate * self.direction;
        let target = self.playhead_s + step;
        let (start, end) = (self.range.start_s, self.range.end_s);
        if target >= start && target <= end {
            self.playhead_s = target;
            return;
        }

        match self.loop_mode {
            LoopMode::Once => {
                self.playhead_s = if target > end { end } else { start };
                self.state = TransportState::Paused;
            }
            LoopMode::Loop => {
                self.playhead_s = wrap(target, start, end);
            }
            LoopMode::PingPong => {
                let (position, flipped) = reflect(target, start, end);
                self.playhead_s = position;
                if flipped {
                    self.direction = -self.direction;
                }
            }
        }
    }

    /// Whether the playhead sits at the end it is travelling towards.
    fn at_end(&self) -> bool {
        if self.rate * self.direction >= 0.0 {
            self.playhead_s >= self.range.end_s
        } else {
            self.playhead_s <= self.range.start_s
        }
    }

    /// Where a restart begins, given which way the transport is travelling.
    fn start_for_direction(&self) -> f64 {
        if self.rate * self.direction >= 0.0 {
            self.range.start_s
        } else {
            self.range.end_s
        }
    }
}

/// Wraps `value` into `[start, end)` — the `Loop` policy. A step longer than
/// the range (a high rate over a short loop) still lands inside it.
fn wrap(value: f64, start: f64, end: f64) -> f64 {
    let span = end - start;
    if span <= 0.0 {
        return start;
    }
    start + (value - start).rem_euclid(span)
}

/// Folds `value` back into `[start, end]` by reflection — the `PingPong`
/// policy. Returns the position and whether the direction should flip, which
/// it does after an odd number of reflections.
fn reflect(value: f64, start: f64, end: f64) -> (f64, bool) {
    let span = end - start;
    if span <= 0.0 {
        return (start, true);
    }
    let offset = (value - start).rem_euclid(2.0 * span);
    let reflections = ((value - start) / span).floor();
    let flipped = (reflections as i64).rem_euclid(2) == 1;
    let position = if offset <= span {
        start + offset
    } else {
        end - (offset - span)
    };
    (position, flipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> Transport {
        Transport::new(TimeRange::new(0.0, 10.0))
    }

    #[test]
    fn play_pause_stop_walk_the_state_machine() {
        let mut t = transport();
        assert_eq!(t.state(), TransportState::Stopped);
        t.play();
        assert_eq!(t.state(), TransportState::Playing);
        t.advance(1.0);
        assert!((t.playhead_s() - 1.0).abs() < 1e-12);
        t.pause();
        assert_eq!(t.state(), TransportState::Paused);
        // Paused retains the playhead...
        t.advance(1.0);
        assert!((t.playhead_s() - 1.0).abs() < 1e-12);
        // ...and stopped resets it.
        t.stop();
        assert_eq!(t.state(), TransportState::Stopped);
        assert_eq!(t.playhead_s(), 0.0);
    }

    #[test]
    fn seek_is_legal_in_every_state_and_changes_none() {
        let prepare: [fn(&mut Transport); 3] = [Transport::stop, Transport::play, Transport::pause];
        for step in prepare {
            let mut t = transport();
            step(&mut t);
            let before = t.state();
            t.seek(4.0);
            assert_eq!(t.state(), before);
            assert!((t.playhead_s() - 4.0).abs() < 1e-12);
        }
    }

    #[test]
    fn seek_clamps_to_the_loop_range() {
        let mut t = transport();
        t.seek(-5.0);
        assert_eq!(t.playhead_s(), 0.0);
        t.seek(500.0);
        assert_eq!(t.playhead_s(), 10.0);
        t.seek_fraction(0.25);
        assert!((t.playhead_s() - 2.5).abs() < 1e-12);
    }

    #[test]
    fn rate_scales_wall_time_and_is_clamped() {
        let mut t = transport();
        t.set_rate(2.0);
        t.play();
        t.advance(1.0);
        assert!((t.playhead_s() - 2.0).abs() < 1e-12);

        t.set_rate(1e9);
        assert_eq!(t.rate(), MAX_RATE);
        t.set_rate(-1e9);
        assert_eq!(t.rate(), -MAX_RATE);
        t.set_rate(0.0);
        assert_eq!(t.rate(), -MAX_RATE, "a zero rate is refused, not applied");
        t.set_rate(0.0001);
        assert_eq!(t.rate(), MIN_RATE);
    }

    #[test]
    fn once_clamps_and_pauses_at_each_end() {
        let mut t = transport();
        t.play();
        t.advance(50.0);
        assert_eq!(t.playhead_s(), 10.0);
        assert_eq!(t.state(), TransportState::Paused);

        t.set_rate(-1.0);
        t.play();
        t.advance(50.0);
        assert_eq!(t.playhead_s(), 0.0);
        assert_eq!(t.state(), TransportState::Paused);
    }

    #[test]
    fn playing_from_the_end_restarts_rather_than_stalling() {
        let mut t = transport();
        t.seek(10.0);
        t.play();
        assert_eq!(t.playhead_s(), 0.0);
    }

    #[test]
    fn loop_wraps_even_when_a_step_overshoots_the_range() {
        let mut t = transport();
        t.set_loop_mode(LoopMode::Loop);
        t.play();
        t.advance(12.0);
        assert!((t.playhead_s() - 2.0).abs() < 1e-9);
        assert_eq!(t.state(), TransportState::Playing);
        // A hundred range-lengths lands back where it started.
        t.seek(0.0);
        t.advance(1000.0);
        assert!(t.playhead_s() < 1e-9, "{}", t.playhead_s());
    }

    #[test]
    fn ping_pong_reverses_at_the_ends() {
        let mut t = transport();
        t.set_loop_mode(LoopMode::PingPong);
        t.play();
        t.advance(12.0);
        // Ten seconds out to the end, then two back.
        assert!((t.playhead_s() - 8.0).abs() < 1e-9, "{}", t.playhead_s());
        t.advance(12.0);
        assert!((t.playhead_s() - 4.0).abs() < 1e-9, "{}", t.playhead_s());
        assert_eq!(t.state(), TransportState::Playing);
    }

    #[test]
    fn ping_pong_stays_inside_the_range_for_any_step() {
        let mut t = transport();
        t.set_loop_mode(LoopMode::PingPong);
        t.play();
        for step in [0.3, 7.0, 25.0, 101.0, 1e6] {
            t.advance(step);
            assert!(
                t.playhead_s() >= 0.0 && t.playhead_s() <= 10.0,
                "step {step} left the playhead at {}",
                t.playhead_s()
            );
        }
    }

    #[test]
    fn loop_points_narrow_the_range_and_pull_the_playhead_in() {
        let mut t = transport();
        t.seek(9.0);
        t.set_loop_start(2.0);
        t.set_loop_end(5.0);
        assert_eq!(t.range(), TimeRange::new(2.0, 5.0));
        assert_eq!(t.playhead_s(), 5.0);
    }

    #[test]
    fn an_empty_range_cannot_play() {
        let mut t = Transport::new(TimeRange::new(3.0, 3.0));
        t.play();
        assert_eq!(t.state(), TransportState::Stopped);
        t.advance(1.0);
        assert_eq!(t.playhead_s(), 3.0);
    }

    #[test]
    fn setting_the_range_keeps_the_playhead_inside_it() {
        let mut t = transport();
        t.seek(9.0);
        t.set_range(TimeRange::new(0.0, 4.0));
        assert_eq!(t.playhead_s(), 4.0);
        t.set_range(TimeRange::new(0.0, 0.0));
        assert_eq!(t.state(), TransportState::Stopped);
    }
}
