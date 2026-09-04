//! The viewport: a time and amplitude window over the absolute timeline, and
//! the mapping between it and pixels (`docs/DESIGN.md` §11.3, §11.4).
//!
//! Every signal maps onto one timeline in seconds, so traces at different
//! sample rates overlay without resampling — the viewport is the only thing
//! that knows about pixels, and it is what decides how much of a column a
//! frame has to read.

use sp_core::{TimeRange, Timebase};

/// The narrowest and widest a viewport may get, in seconds. The lower bound
/// keeps `seconds_per_pixel` away from zero at any sane width; the upper one
/// stops a fit over a signal with a corrupt timebase running away.
pub const MIN_DURATION_S: f64 = 1e-12;
pub const MAX_DURATION_S: f64 = 1e12;

/// A vertical window in engineering units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Amplitude {
    pub min: f64,
    pub max: f64,
}

impl Default for Amplitude {
    fn default() -> Self {
        Self::UNIT
    }
}

impl Amplitude {
    /// The window a signal with no statistics yet is drawn in.
    pub const UNIT: Self = Self {
        min: -1.0,
        max: 1.0,
    };

    #[must_use]
    pub fn new(min: f64, max: f64) -> Self {
        if !(min.is_finite() && max.is_finite()) || max <= min {
            return Self::UNIT;
        }
        Self { min, max }
    }

    /// A window around `[min, max]` with `margin` of its span added at each
    /// end, so a trace never touches the frame. A flat signal opens to a unit
    /// band rather than a zero-height one.
    #[must_use]
    pub fn around(min: f64, max: f64, margin: f64) -> Self {
        if !(min.is_finite() && max.is_finite()) {
            return Self::UNIT;
        }
        let span = max - min;
        if span <= 0.0 {
            let centre = (min + max) / 2.0;
            return Self::new(centre - 0.5, centre + 0.5);
        }
        let pad = span * margin.max(0.0);
        Self::new(min - pad, max + pad)
    }

    #[must_use]
    pub fn span(self) -> f64 {
        self.max - self.min
    }

    #[must_use]
    pub fn centre(self) -> f64 {
        (self.min + self.max) / 2.0
    }

    /// Zooms about `value`, which stays where it is. `factor` above one widens
    /// the window (zooms out).
    #[must_use]
    pub fn zoom_about(self, value: f64, factor: f64) -> Self {
        if !(factor.is_finite() && factor > 0.0) {
            return self;
        }
        let anchor = if value.is_finite() {
            value
        } else {
            self.centre()
        };
        Self::new(
            anchor - (anchor - self.min) * factor,
            anchor + (self.max - anchor) * factor,
        )
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self::new(self.min.min(other.min), self.max.max(other.max))
    }
}

/// Where the playhead sits while playing (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowMode {
    /// The viewport is fixed and a cursor sweeps across it.
    #[default]
    Playhead,
    /// The playhead is pinned and the window scrolls under it, like a
    /// strip-chart recorder.
    Scrolling,
}

impl FollowMode {
    pub const ALL: [Self; 2] = [Self::Playhead, Self::Scrolling];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Playhead => "Playhead",
            Self::Scrolling => "Scrolling",
        }
    }
}

impl std::fmt::Display for FollowMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Where a pinned playhead sits across the viewport in scrolling mode.
pub const SCROLL_ANCHOR: f64 = 0.8;

/// A time and amplitude window, plus the pixel width it is drawn into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    time: TimeRange,
    pub amplitude: Amplitude,
    /// Width of the drawing area. Never zero: the reducer divides by it.
    width_px: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new(TimeRange::new(0.0, 1.0), Amplitude::UNIT, 1_000.0)
    }
}

impl Viewport {
    #[must_use]
    pub fn new(time: TimeRange, amplitude: Amplitude, width_px: f32) -> Self {
        let mut viewport = Self {
            time,
            amplitude,
            width_px: 1.0,
        };
        viewport.set_width(width_px);
        viewport.set_time(time);
        viewport
    }

    #[must_use]
    pub fn time(&self) -> TimeRange {
        self.time
    }

    #[must_use]
    pub fn width_px(&self) -> f32 {
        self.width_px
    }

    /// Follows the canvas. A width of zero — a collapsed pane — is held at one
    /// pixel so the mapping stays finite.
    pub fn set_width(&mut self, width_px: f32) {
        self.width_px = if width_px.is_finite() {
            width_px.max(1.0)
        } else {
            1.0
        };
    }

    /// Sets the time window, clamped to the zoom limits.
    pub fn set_time(&mut self, time: TimeRange) {
        let duration = time.duration_s();
        self.time = if !time.start_s.is_finite() || !duration.is_finite() || duration <= 0.0 {
            TimeRange::from_duration(
                if time.start_s.is_finite() {
                    time.start_s
                } else {
                    0.0
                },
                MIN_DURATION_S,
            )
        } else {
            TimeRange::from_duration(time.start_s, duration.clamp(MIN_DURATION_S, MAX_DURATION_S))
        };
    }

    #[must_use]
    pub fn duration_s(&self) -> f64 {
        self.time.duration_s()
    }

    #[must_use]
    pub fn seconds_per_pixel(&self) -> f64 {
        self.duration_s() / f64::from(self.width_px)
    }

    /// Samples one pixel column covers for a signal at `rate_hz`.
    #[must_use]
    pub fn samples_per_pixel(&self, rate_hz: f64) -> f64 {
        self.seconds_per_pixel() * rate_hz
    }

    /// Horizontal position of `t_s`, in pixels from the left edge. Values
    /// outside the window map outside the canvas, which is what lets the
    /// caller clip rather than guess.
    #[must_use]
    pub fn x_of(&self, t_s: f64) -> f32 {
        let fraction = (t_s - self.time.start_s) / self.duration_s();
        (fraction * f64::from(self.width_px)) as f32
    }

    /// The time under a horizontal pixel position.
    #[must_use]
    pub fn time_at(&self, x_px: f32) -> f64 {
        self.time.start_s + f64::from(x_px) * self.seconds_per_pixel()
    }

    /// Vertical position of `value` in a canvas `height_px` tall, measured
    /// downwards from the top.
    #[must_use]
    pub fn y_of(&self, value: f64, height_px: f32) -> f32 {
        let fraction = (value - self.amplitude.min) / self.amplitude.span();
        ((1.0 - fraction) * f64::from(height_px)) as f32
    }

    /// The value at a vertical pixel position.
    #[must_use]
    pub fn value_at(&self, y_px: f32, height_px: f32) -> f64 {
        let fraction = 1.0 - f64::from(y_px) / f64::from(height_px.max(1.0));
        self.amplitude.min + fraction * self.amplitude.span()
    }

    /// Zooms time about `t_s`, which stays under the pointer — the scroll
    /// gesture (§11.4). `factor` above one zooms out.
    pub fn zoom_time_about(&mut self, t_s: f64, factor: f64) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let anchor = if t_s.is_finite() {
            t_s
        } else {
            self.time.start_s + self.duration_s() / 2.0
        };
        let start = anchor - (anchor - self.time.start_s) * factor;
        let duration = self.duration_s() * factor;
        self.set_time(TimeRange::from_duration(start, duration));
    }

    /// Pans by `dt_s` — shift+scroll, and what scrolling mode does each tick.
    pub fn pan(&mut self, dt_s: f64) {
        if !dt_s.is_finite() {
            return;
        }
        self.set_time(TimeRange::from_duration(
            self.time.start_s + dt_s,
            self.duration_s(),
        ));
    }

    /// Pans by a number of pixels.
    pub fn pan_pixels(&mut self, dx_px: f32) {
        self.pan(f64::from(dx_px) * self.seconds_per_pixel());
    }

    /// Zooms amplitude about the centre of the window — ctrl+scroll.
    pub fn zoom_amplitude(&mut self, factor: f64) {
        self.amplitude = self.amplitude.zoom_about(self.amplitude.centre(), factor);
    }

    /// Fits `range` exactly — double-click, and `Home`/`End` bounds.
    pub fn fit_time(&mut self, range: TimeRange) {
        if range.is_empty() {
            self.set_time(TimeRange::from_duration(range.start_s, MIN_DURATION_S));
        } else {
            self.set_time(range);
        }
    }

    /// Keeps `t_s` at the window's anchor without changing the zoom — the
    /// strip-chart behaviour of scrolling mode (§11.4).
    pub fn follow(&mut self, t_s: f64, anchor: f64) {
        if !t_s.is_finite() {
            return;
        }
        let duration = self.duration_s();
        let start = t_s - duration * anchor.clamp(0.0, 1.0);
        self.set_time(TimeRange::from_duration(start, duration));
    }

    /// Whether `t_s` is inside the window.
    #[must_use]
    pub fn contains(&self, t_s: f64) -> bool {
        self.time.contains(t_s)
    }

    /// The sample range of a signal on `timebase` that the window covers,
    /// clamped to `count`. `None` for an irregular timebase, whose indices
    /// come from a time column rather than from arithmetic.
    #[must_use]
    pub fn sample_range(&self, timebase: Timebase, count: u64) -> Option<sp_core::SampleRange> {
        let rate = timebase.sample_rate_hz?;
        if count == 0 {
            return Some(sp_core::SampleRange::new(0, 0));
        }
        let first = ((self.time.start_s - timebase.t0_s) * rate).floor();
        let last = ((self.time.end_s - timebase.t0_s) * rate).ceil();
        let start = first.clamp(0.0, count as f64) as u64;
        // One sample past the right edge, so a bar at the edge is drawn from
        // real data rather than from a gap.
        let end = (last + 1.0).clamp(0.0, count as f64) as u64;
        Some(sp_core::SampleRange::new(start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        Viewport::new(TimeRange::new(0.0, 1.0), Amplitude::new(-1.0, 1.0), 1_000.0)
    }

    #[test]
    fn time_and_pixels_round_trip() {
        let v = viewport();
        assert_eq!(v.x_of(0.0), 0.0);
        assert_eq!(v.x_of(1.0), 1_000.0);
        assert!((v.time_at(500.0) - 0.5).abs() < 1e-12);
        assert!((v.seconds_per_pixel() - 0.001).abs() < 1e-15);
        assert!((v.samples_per_pixel(1_000_000.0) - 1_000.0).abs() < 1e-9);
    }

    #[test]
    fn amplitude_maps_downwards() {
        let v = viewport();
        assert_eq!(v.y_of(1.0, 100.0), 0.0);
        assert_eq!(v.y_of(-1.0, 100.0), 100.0);
        assert_eq!(v.y_of(0.0, 100.0), 50.0);
        assert!((v.value_at(50.0, 100.0)).abs() < 1e-12);
    }

    #[test]
    fn zooming_keeps_the_anchor_under_the_pointer() {
        let mut v = viewport();
        v.zoom_time_about(0.25, 0.5);
        assert!((v.x_of(0.25) - 250.0).abs() < 1e-3);
        assert!((v.duration_s() - 0.5).abs() < 1e-12);

        v.zoom_time_about(0.25, 2.0);
        assert!((v.duration_s() - 1.0).abs() < 1e-12);
        assert!((v.time().start_s).abs() < 1e-12);
    }

    #[test]
    fn zoom_is_bounded_at_both_ends() {
        let mut v = viewport();
        for _ in 0..200 {
            v.zoom_time_about(0.5, 0.1);
        }
        // The floor is the clamp, give or take the rounding of adding a
        // picosecond to a whole second.
        let narrowest = v.duration_s();
        assert!(
            narrowest > 0.0 && narrowest <= MIN_DURATION_S * 1.01,
            "narrowest window was {narrowest}"
        );
        for _ in 0..200 {
            v.zoom_time_about(0.5, 10.0);
        }
        assert!(v.duration_s() <= MAX_DURATION_S);
    }

    #[test]
    fn panning_moves_the_window_and_keeps_its_width() {
        let mut v = viewport();
        v.pan(2.0);
        assert_eq!(v.time(), TimeRange::new(2.0, 3.0));
        v.pan_pixels(-100.0);
        assert!((v.time().start_s - 1.9).abs() < 1e-12);
        assert!((v.duration_s() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_collapsed_canvas_still_maps_finitely() {
        let mut v = viewport();
        v.set_width(0.0);
        assert_eq!(v.width_px(), 1.0);
        assert!(v.seconds_per_pixel().is_finite());
        v.set_width(f32::NAN);
        assert_eq!(v.width_px(), 1.0);
    }

    #[test]
    fn fit_of_an_empty_range_opens_the_narrowest_window() {
        let mut v = viewport();
        v.fit_time(TimeRange::new(5.0, 5.0));
        assert_eq!(v.time().start_s, 5.0);
        assert!(v.duration_s() >= MIN_DURATION_S);
    }

    #[test]
    fn following_pins_the_playhead_at_the_anchor() {
        let mut v = viewport();
        v.follow(10.0, SCROLL_ANCHOR);
        assert!((v.x_of(10.0) - 800.0).abs() < 1e-3);
        assert!((v.duration_s() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_sample_range_covers_the_window_and_clamps_to_the_signal() {
        let v = viewport();
        let tb = Timebase::regular(1_000.0, 0.0);
        let range = v.sample_range(tb, 10_000).unwrap();
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 1_001);

        // A window past the end of a short signal reduces to nothing.
        let mut v = viewport();
        v.pan(100.0);
        assert!(v.sample_range(tb, 10_000).unwrap().is_empty());

        // An irregular timebase has no arithmetic answer.
        assert!(v.sample_range(Timebase::irregular(0.0), 10).is_none());
    }

    #[test]
    fn amplitude_pads_a_flat_signal_into_a_band() {
        let flat = Amplitude::around(2.0, 2.0, 0.1);
        assert!(flat.span() > 0.0);
        assert!((flat.centre() - 2.0).abs() < 1e-12);

        let padded = Amplitude::around(-1.0, 1.0, 0.05);
        assert!((padded.min + 1.1).abs() < 1e-12);
        assert!((padded.max - 1.1).abs() < 1e-12);

        // Nonsense in, a usable window out.
        assert_eq!(Amplitude::around(f64::NAN, 1.0, 0.1), Amplitude::UNIT);
        assert_eq!(Amplitude::new(5.0, 5.0), Amplitude::UNIT);
    }

    #[test]
    fn amplitude_zoom_keeps_its_anchor() {
        let a = Amplitude::new(-1.0, 1.0).zoom_about(0.0, 2.0);
        assert_eq!(a, Amplitude::new(-2.0, 2.0));
        let a = Amplitude::new(0.0, 10.0).zoom_about(10.0, 0.5);
        assert_eq!(a, Amplitude::new(5.0, 10.0));
    }
}
