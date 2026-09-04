//! Time, sample indexing and the absolute timeline.
//!
//! Every signal in the library maps onto one absolute timeline measured in
//! seconds (`docs/DESIGN.md` §11.3), so signals recorded or produced at
//! different sample rates overlay without resampling.

use serde::{Deserialize, Serialize};

/// A zero-based index into a signal's samples.
pub type SampleIndex = u64;

/// A wall-clock instant, stored as RFC 3339 in the database.
pub type Timestamp = time::OffsetDateTime;

/// Current UTC instant.
#[must_use]
pub fn now_utc() -> Timestamp {
    Timestamp::now_utc()
}

/// Half-open range of sample indices, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleRange {
    pub start: SampleIndex,
    pub end: SampleIndex,
}

impl SampleRange {
    /// A range covering `[start, end)`; `end` is clamped up to `start`.
    #[must_use]
    pub fn new(start: SampleIndex, end: SampleIndex) -> Self {
        Self {
            start,
            end: end.max(start),
        }
    }

    /// The first `count` samples.
    #[must_use]
    pub fn first(count: u64) -> Self {
        Self {
            start: 0,
            end: count,
        }
    }

    #[must_use]
    pub fn len(self) -> u64 {
        self.end - self.start
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }

    #[must_use]
    pub fn contains(self, index: SampleIndex) -> bool {
        index >= self.start && index < self.end
    }

    /// Overlap with `other`; empty when they do not overlap.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self::new(self.start.max(other.start), self.end.min(other.end))
    }

    /// Splits into at most `chunks` sub-ranges of near-equal length, for
    /// fan-out across a worker pool.
    #[must_use]
    pub fn split(self, chunks: usize) -> Vec<Self> {
        let len = self.len();
        if len == 0 || chunks == 0 {
            return Vec::new();
        }
        let chunks = (chunks as u64).min(len);
        let per = len / chunks;
        let remainder = len % chunks;
        let mut out = Vec::with_capacity(chunks as usize);
        let mut cursor = self.start;
        for i in 0..chunks {
            let extra = u64::from(i < remainder);
            let end = cursor + per + extra;
            out.push(Self { start: cursor, end });
            cursor = end;
        }
        out
    }
}

/// Half-open span of the absolute timeline, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeRange {
    pub start_s: f64,
    pub end_s: f64,
}

impl TimeRange {
    #[must_use]
    pub fn new(start_s: f64, end_s: f64) -> Self {
        Self {
            start_s,
            end_s: end_s.max(start_s),
        }
    }

    #[must_use]
    pub fn from_duration(start_s: f64, duration_s: f64) -> Self {
        Self::new(start_s, start_s + duration_s.max(0.0))
    }

    #[must_use]
    pub fn duration_s(self) -> f64 {
        self.end_s - self.start_s
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.end_s <= self.start_s
    }

    #[must_use]
    pub fn contains(self, t_s: f64) -> bool {
        t_s >= self.start_s && t_s < self.end_s
    }

    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self::new(self.start_s.max(other.start_s), self.end_s.min(other.end_s))
    }

    /// Smallest range containing both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self::new(self.start_s.min(other.start_s), self.end_s.max(other.end_s))
    }

    /// Clamps `t_s` into the range.
    #[must_use]
    pub fn clamp(self, t_s: f64) -> f64 {
        t_s.clamp(self.start_s, self.end_s)
    }

    /// Position of `t_s` within the range as a fraction in `[0, 1]`.
    #[must_use]
    pub fn fraction_of(self, t_s: f64) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            ((t_s - self.start_s) / self.duration_s()).clamp(0.0, 1.0)
        }
    }
}

/// How a signal's sample indices map onto the absolute timeline.
///
/// `sample_rate_hz` is `None` for irregularly-sampled signals, whose timestamps
/// live in a companion time blob (`signal.time_blob_id`); those signals answer
/// index/time queries through the store rather than arithmetic here.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Timebase {
    pub sample_rate_hz: Option<f64>,
    pub t0_s: f64,
}

impl Timebase {
    /// A regularly-sampled timebase. `sample_rate_hz` must be finite and > 0.
    #[must_use]
    pub fn regular(sample_rate_hz: f64, t0_s: f64) -> Self {
        debug_assert!(sample_rate_hz.is_finite() && sample_rate_hz > 0.0);
        Self {
            sample_rate_hz: Some(sample_rate_hz),
            t0_s,
        }
    }

    /// An irregular timebase; times come from an explicit timestamp array.
    #[must_use]
    pub fn irregular(t0_s: f64) -> Self {
        Self {
            sample_rate_hz: None,
            t0_s,
        }
    }

    #[must_use]
    pub fn is_regular(self) -> bool {
        self.sample_rate_hz.is_some()
    }

    /// Seconds per sample, or `None` when irregular.
    #[must_use]
    pub fn sample_period_s(self) -> Option<f64> {
        self.sample_rate_hz.map(|fs| 1.0 / fs)
    }

    /// Absolute time of sample `index`, or `None` when irregular.
    #[must_use]
    pub fn time_of(self, index: SampleIndex) -> Option<f64> {
        self.sample_rate_hz
            .map(|fs| index as f64 / fs)
            .map(|offset| self.t0_s + offset)
    }

    /// Index of the sample at or before `t_s`, clamped to `[0, count)`.
    /// `None` when irregular or when `count` is zero.
    #[must_use]
    pub fn index_at(self, t_s: f64, count: u64) -> Option<SampleIndex> {
        if count == 0 {
            return None;
        }
        let fs = self.sample_rate_hz?;
        let raw = ((t_s - self.t0_s) * fs).floor();
        if raw <= 0.0 {
            Some(0)
        } else if raw >= (count - 1) as f64 {
            Some(count - 1)
        } else {
            Some(raw as u64)
        }
    }

    /// Duration of `count` samples, or `None` when irregular.
    #[must_use]
    pub fn duration_s(self, count: u64) -> Option<f64> {
        self.sample_rate_hz.map(|fs| count as f64 / fs)
    }

    /// Timeline span covered by `count` samples, or `None` when irregular.
    #[must_use]
    pub fn time_range(self, count: u64) -> Option<TimeRange> {
        self.duration_s(count)
            .map(|d| TimeRange::from_duration(self.t0_s, d))
    }

    /// The Nyquist frequency, or `None` when irregular.
    #[must_use]
    pub fn nyquist_hz(self) -> Option<f64> {
        self.sample_rate_hz.map(|fs| fs / 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_timebase_maps_index_to_time() {
        let tb = Timebase::regular(1_000.0, 2.0);
        assert_eq!(tb.time_of(0), Some(2.0));
        assert_eq!(tb.time_of(1_000), Some(3.0));
        assert_eq!(tb.duration_s(500), Some(0.5));
        assert_eq!(tb.nyquist_hz(), Some(500.0));
    }

    #[test]
    fn index_at_clamps_to_the_signal() {
        let tb = Timebase::regular(100.0, 0.0);
        assert_eq!(tb.index_at(-5.0, 100), Some(0));
        assert_eq!(tb.index_at(0.25, 100), Some(25));
        assert_eq!(tb.index_at(1_000.0, 100), Some(99));
        assert_eq!(tb.index_at(0.25, 0), None);
    }

    #[test]
    fn irregular_timebase_answers_nothing_by_arithmetic() {
        let tb = Timebase::irregular(0.0);
        assert!(!tb.is_regular());
        assert_eq!(tb.time_of(10), None);
        assert_eq!(tb.duration_s(10), None);
    }

    #[test]
    fn sample_range_splits_evenly_and_covers_everything() {
        let range = SampleRange::new(0, 10);
        let parts = range.split(3);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.iter().map(|p| p.len()).sum::<u64>(), 10);
        assert_eq!(parts[0].start, 0);
        assert_eq!(parts[2].end, 10);
        // Chunks never exceed the number of samples.
        assert_eq!(SampleRange::new(0, 2).split(8).len(), 2);
        assert!(SampleRange::new(4, 4).split(4).is_empty());
    }

    #[test]
    fn time_ranges_intersect_and_union() {
        let a = TimeRange::new(0.0, 10.0);
        let b = TimeRange::new(5.0, 20.0);
        assert_eq!(a.intersect(b), TimeRange::new(5.0, 10.0));
        assert_eq!(a.union(b), TimeRange::new(0.0, 20.0));
        assert!(a.intersect(TimeRange::new(50.0, 60.0)).is_empty());
        assert!((a.fraction_of(2.5) - 0.25).abs() < f64::EPSILON);
    }
}
