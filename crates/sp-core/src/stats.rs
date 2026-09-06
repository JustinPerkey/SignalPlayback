//! Summary statistics and the min/max pair the render pyramid is built from.
//!
//! Statistics are NaN-aware: a missing CSV cell becomes `NaN` and is drawn as a
//! gap in the trace (§7.3), so it must not poison the summary.

use serde::{Deserialize, Serialize};

use crate::signal::SampleBuffer;

/// A `(min, max)` pair over a span of samples — one pyramid cell (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

impl MinMax {
    /// The identity for folding: an empty pair that loses to every real value.
    pub const EMPTY: Self = Self {
        min: f32::INFINITY,
        max: f32::NEG_INFINITY,
    };

    #[must_use]
    pub fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.min > self.max
    }

    /// Widens to include `value`; non-finite values are ignored.
    pub fn include(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        let v = value as f32;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }
}

impl Default for MinMax {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Cached per-signal statistics, mirrored into the `signal` row so the library
/// table can sort on them without reading samples.
///
/// The accumulator keeps running sums rather than derived values, so
/// [`SignalStats::push`] costs one add per sample and `mean`/`rms` are computed
/// only when asked for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SignalStats {
    min: f64,
    max: f64,
    /// Finite samples counted.
    count: u64,
    /// Samples skipped because they were NaN or infinite.
    non_finite: u64,
    sum: f64,
    sum_sq: f64,
}

impl SignalStats {
    /// An empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            count: 0,
            non_finite: 0,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    /// Rebuilds a summary from the columns the database stores.
    ///
    /// `signal` and `pulse_field` persist min/max/mean/rms rather than the
    /// running sums, because those are what the library table sorts on. The
    /// sums are recoverable from `mean`, `rms` and `count`, which is what keeps
    /// a reloaded summary mergeable with a freshly computed one — a stage that
    /// appends to a column must not have to rescan it to update the statistics.
    #[must_use]
    pub fn from_stored(
        min: f64,
        max: f64,
        mean: f64,
        rms: f64,
        count: u64,
        non_finite: u64,
    ) -> Self {
        if count == 0 {
            return Self {
                non_finite,
                ..Self::new()
            };
        }
        let n = count as f64;
        Self {
            min,
            max,
            count,
            non_finite,
            sum: mean * n,
            sum_sq: rms * rms * n,
        }
    }

    /// Accumulates one value. Sums are kept in `f64` regardless of the source
    /// dtype so a 100 M-sample `f32` signal does not lose precision.
    pub fn push(&mut self, value: f64) {
        if !value.is_finite() {
            self.non_finite += 1;
            return;
        }
        self.count += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value;
        self.sum_sq += value * value;
    }

    /// Combines two partial summaries, so a signal can be summarised in
    /// parallel chunks and folded.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.count += other.count;
        self.non_finite += other.non_finite;
        self.sum += other.sum;
        self.sum_sq += other.sum_sq;
        self
    }

    /// Whether any finite sample was seen. An all-NaN signal summarises to
    /// nothing rather than to zeros.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Finite samples seen.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Samples skipped because they were NaN or infinite.
    #[must_use]
    pub fn non_finite(&self) -> u64 {
        self.non_finite
    }

    /// Smallest finite sample, or `None` if there was none.
    #[must_use]
    pub fn min(&self) -> Option<f64> {
        (!self.is_empty()).then_some(self.min)
    }

    /// Largest finite sample, or `None` if there was none.
    #[must_use]
    pub fn max(&self) -> Option<f64> {
        (!self.is_empty()).then_some(self.max)
    }

    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        (!self.is_empty()).then(|| self.sum / self.count as f64)
    }

    #[must_use]
    pub fn rms(&self) -> Option<f64> {
        (!self.is_empty()).then(|| (self.sum_sq / self.count as f64).sqrt())
    }

    #[must_use]
    pub fn peak_to_peak(&self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            self.max - self.min
        }
    }

    /// Population variance of the finite samples, from the running sums.
    ///
    /// `E[x²] - E[x]²` is one subtraction rather than a second pass, and the
    /// result is clamped at zero: with a large mean and a small spread the two
    /// terms are nearly equal and rounding can make the difference negative.
    #[must_use]
    pub fn variance(&self) -> Option<f64> {
        let mean = self.mean()?;
        Some((self.sum_sq / self.count as f64 - mean * mean).max(0.0))
    }

    /// Standard deviation — the AC part of the signal, where `rms` is the
    /// whole of it.
    #[must_use]
    pub fn std_dev(&self) -> Option<f64> {
        self.variance().map(f64::sqrt)
    }

    /// The `(min, max)` pair for this span, for pyramid level 0.
    #[must_use]
    pub fn min_max(&self) -> MinMax {
        if self.is_empty() {
            MinMax::EMPTY
        } else {
            MinMax::new(self.min as f32, self.max as f32)
        }
    }
}

impl Default for SignalStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FromIterator<f64> for SignalStats {
    fn from_iter<T: IntoIterator<Item = f64>>(iter: T) -> Self {
        let mut stats = Self::new();
        for value in iter {
            stats.push(value);
        }
        stats
    }
}

/// Summarises a whole buffer in one pass.
#[must_use]
pub fn summarise(buffer: &SampleBuffer) -> SignalStats {
    buffer.values().collect()
}

/// A fixed-bin histogram of a signal's values, for the Inspector (§12.1).
///
/// The bin edges are decided before the first sample is pushed — from the
/// cached min/max the `signal` row already carries — so a signal is
/// histogrammed in the same single streaming pass that computes its
/// statistics, however many samples it has. Values outside the span are
/// counted rather than dropped: an accumulator built from stale bounds still
/// says how much fell off each end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Histogram {
    low: f64,
    high: f64,
    counts: Vec<u64>,
    below: u64,
    above: u64,
    non_finite: u64,
}

impl Histogram {
    /// Bins the span `[low, high]` into `bins` equal buckets.
    ///
    /// A degenerate span — one constant value, or bounds that arrive the wrong
    /// way round — widens to a unit interval around the value, so a DC signal
    /// still draws as one full bin rather than dividing by zero.
    #[must_use]
    pub fn new(low: f64, high: f64, bins: usize) -> Self {
        let bins = bins.max(1);
        let (low, high) = match (low.is_finite() && high.is_finite(), low < high) {
            (true, true) => (low, high),
            (true, false) => (low - 0.5, low + 0.5),
            (false, _) => (-0.5, 0.5),
        };
        Self {
            low,
            high,
            counts: vec![0; bins],
            below: 0,
            above: 0,
            non_finite: 0,
        }
    }

    /// Files one value. The top edge belongs to the last bin, so `max` itself
    /// is inside the histogram rather than one past it.
    pub fn push(&mut self, value: f64) {
        if !value.is_finite() {
            self.non_finite += 1;
            return;
        }
        if value < self.low {
            self.below += 1;
            return;
        }
        if value > self.high {
            self.above += 1;
            return;
        }
        let bins = self.counts.len();
        let position = (value - self.low) / (self.high - self.low) * bins as f64;
        let index = (position as usize).min(bins - 1);
        self.counts[index] += 1;
    }

    /// Adds another histogram over the same span. Merging across different
    /// spans is not meaningful, so a mismatch is refused rather than silently
    /// producing a wrong picture.
    #[must_use]
    pub fn merge(mut self, other: &Self) -> Self {
        if self.counts.len() != other.counts.len()
            || self.low != other.low
            || self.high != other.high
        {
            return self;
        }
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine += theirs;
        }
        self.below += other.below;
        self.above += other.above;
        self.non_finite += other.non_finite;
        self
    }

    /// Per-bin counts, lowest bin first.
    #[must_use]
    pub fn counts(&self) -> &[u64] {
        &self.counts
    }

    /// Number of bins.
    #[must_use]
    pub fn bins(&self) -> usize {
        self.counts.len()
    }

    /// The span the bins cover.
    #[must_use]
    pub fn span(&self) -> (f64, f64) {
        (self.low, self.high)
    }

    /// Width of one bin.
    #[must_use]
    pub fn bin_width(&self) -> f64 {
        (self.high - self.low) / self.counts.len() as f64
    }

    /// The half-open value range bin `index` covers; the last bin is closed.
    #[must_use]
    pub fn bin_span(&self, index: usize) -> (f64, f64) {
        let width = self.bin_width();
        (
            self.low + width * index as f64,
            self.low + width * (index + 1) as f64,
        )
    }

    /// The largest bin count, which is what a drawn histogram scales to.
    #[must_use]
    pub fn peak(&self) -> u64 {
        self.counts.iter().copied().max().unwrap_or(0)
    }

    /// Values that landed in a bin.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// Values below the span, above it, and non-finite.
    #[must_use]
    pub fn outside(&self) -> (u64, u64, u64) {
        (self.below, self.above, self.non_finite)
    }

    /// Whether nothing at all was filed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Everything the Inspector computes in one pass over a column: the summary,
/// the histogram and the zero crossings (§12.1).
///
/// Zero crossings need the samples in order, which is why this is an
/// accumulator fed chunk by chunk rather than a fold over independent parts:
/// the crossing between the last sample of one chunk and the first of the next
/// belongs to neither of them alone.
#[derive(Debug, Clone)]
pub struct Profiler {
    stats: SignalStats,
    histogram: Histogram,
    crossings: u64,
    /// Sign of the last finite non-zero sample: `1`, `-1`, or `0` before one
    /// has been seen.
    last_sign: i8,
}

/// The finished result of a [`Profiler`].
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub stats: SignalStats,
    pub histogram: Histogram,
    /// Sign changes between consecutive finite samples. A run of exact zeros
    /// is not a crossing on its own; the crossing is counted when the sign on
    /// the far side of it differs from the sign before it.
    pub zero_crossings: u64,
}

impl Profiler {
    /// A profiler binning `[low, high]` into `bins` buckets.
    #[must_use]
    pub fn new(low: f64, high: f64, bins: usize) -> Self {
        Self {
            stats: SignalStats::new(),
            histogram: Histogram::new(low, high, bins),
            crossings: 0,
            last_sign: 0,
        }
    }

    /// Accumulates one sample.
    pub fn push(&mut self, value: f64) {
        self.stats.push(value);
        self.histogram.push(value);
        if !value.is_finite() {
            return;
        }
        let sign = if value > 0.0 {
            1
        } else if value < 0.0 {
            -1
        } else {
            return;
        };
        if self.last_sign != 0 && sign != self.last_sign {
            self.crossings += 1;
        }
        self.last_sign = sign;
    }

    /// Accumulates a chunk of samples, in order.
    pub fn extend(&mut self, values: impl IntoIterator<Item = f64>) {
        for value in values {
            self.push(value);
        }
    }

    /// What the pass found.
    #[must_use]
    pub fn finish(self) -> Profile {
        Profile {
            stats: self.stats,
            histogram: self.histogram,
            zero_crossings: self.crossings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{DType, SampleBuffer};

    #[test]
    fn summarises_a_buffer() {
        let buf = SampleBuffer::from_f64(DType::F64, &[-2.0, 0.0, 2.0, 4.0]);
        let stats = summarise(&buf);
        assert_eq!(stats.count(), 4);
        assert_eq!(stats.min(), Some(-2.0));
        assert_eq!(stats.max(), Some(4.0));
        assert_eq!(stats.mean(), Some(1.0));
        assert!((stats.rms().unwrap() - 6.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(stats.peak_to_peak(), 6.0);
    }

    #[test]
    fn missing_values_do_not_poison_the_summary() {
        let buf = SampleBuffer::from_f64(DType::F64, &[1.0, f64::NAN, 3.0, f64::INFINITY]);
        let stats = summarise(&buf);
        assert_eq!(stats.count(), 2);
        assert_eq!(stats.non_finite(), 2);
        assert_eq!(stats.mean(), Some(2.0));
        assert_eq!(stats.max(), Some(3.0));
    }

    #[test]
    fn an_all_nan_signal_summarises_to_nothing() {
        let stats: SignalStats = [f64::NAN, f64::NAN].into_iter().collect();
        assert!(stats.is_empty());
        assert_eq!(stats.min(), None);
        assert_eq!(stats.mean(), None);
        assert_eq!(stats.peak_to_peak(), 0.0);
        assert!(stats.min_max().is_empty());
    }

    #[test]
    fn chunked_summaries_fold_to_the_serial_result() {
        let values: Vec<f64> = (0..1_000).map(|i| f64::from(i) * 0.5 - 100.0).collect();
        let serial: SignalStats = values.iter().copied().collect();
        let folded = values
            .chunks(97)
            .map(|chunk| chunk.iter().copied().collect::<SignalStats>())
            .fold(SignalStats::new(), SignalStats::merge);

        assert_eq!(folded.count(), serial.count());
        assert_eq!(folded.min(), serial.min());
        assert_eq!(folded.max(), serial.max());
        assert!((folded.mean().unwrap() - serial.mean().unwrap()).abs() < 1e-9);
        assert!((folded.rms().unwrap() - serial.rms().unwrap()).abs() < 1e-9);
    }

    #[test]
    fn a_summary_survives_a_round_trip_through_the_database_columns() {
        let values: Vec<f64> = (0..500).map(|i| f64::from(i) * 0.25 - 40.0).collect();
        let original: SignalStats = values.iter().copied().chain([f64::NAN, f64::NAN]).collect();

        // Exactly the columns `signal` and `pulse_field` persist.
        let reloaded = SignalStats::from_stored(
            original.min().unwrap(),
            original.max().unwrap(),
            original.mean().unwrap(),
            original.rms().unwrap(),
            original.count(),
            original.non_finite(),
        );
        assert_eq!(reloaded.count(), original.count());
        assert_eq!(reloaded.non_finite(), original.non_finite());
        assert_eq!(reloaded.min(), original.min());

        // And it is still mergeable, which is the point of restoring the sums.
        let extra: SignalStats = [1_000.0].into_iter().collect();
        let grown = reloaded.merge(extra);
        let from_scratch: SignalStats = values
            .iter()
            .copied()
            .chain([f64::NAN, f64::NAN, 1_000.0])
            .collect();
        assert_eq!(grown.count(), from_scratch.count());
        assert!((grown.mean().unwrap() - from_scratch.mean().unwrap()).abs() < 1e-9);
        assert!((grown.rms().unwrap() - from_scratch.rms().unwrap()).abs() < 1e-9);
    }

    #[test]
    fn an_empty_stored_summary_reloads_as_empty() {
        let reloaded = SignalStats::from_stored(0.0, 0.0, 0.0, 0.0, 0, 7);
        assert!(reloaded.is_empty());
        assert_eq!(reloaded.non_finite(), 7);
        assert_eq!(reloaded.mean(), None);
    }

    #[test]
    fn standard_deviation_is_the_spread_around_the_mean() {
        // A square wave about a large offset: RMS is dominated by the offset,
        // the standard deviation is the amplitude.
        let stats: SignalStats = [999.0, 1001.0, 999.0, 1001.0].into_iter().collect();
        assert_eq!(stats.mean(), Some(1000.0));
        assert!((stats.std_dev().unwrap() - 1.0).abs() < 1e-6);
        assert!(stats.rms().unwrap() > 999.0);
    }

    #[test]
    fn a_constant_signal_has_no_spread() {
        let stats: SignalStats = std::iter::repeat_n(3.25, 100).collect();
        // The two terms of E[x²] - E[x]² are equal here, and rounding must not
        // leave a negative variance behind.
        assert_eq!(stats.variance(), Some(0.0));
        assert_eq!(stats.std_dev(), Some(0.0));
        assert_eq!(SignalStats::new().std_dev(), None);
    }

    #[test]
    fn a_histogram_bins_across_its_span() {
        let mut hist = Histogram::new(0.0, 4.0, 4);
        for value in [0.0, 0.5, 1.0, 1.5, 2.5, 4.0] {
            hist.push(value);
        }
        // [0,1) has two, [1,2) has two, [2,3) has one, and the top edge lands
        // in the last bin rather than falling off the end.
        assert_eq!(hist.counts(), [2, 2, 1, 1]);
        assert_eq!(hist.total(), 6);
        assert_eq!(hist.peak(), 2);
        assert_eq!(hist.bin_width(), 1.0);
        assert_eq!(hist.bin_span(2), (2.0, 3.0));
    }

    #[test]
    fn values_outside_the_span_are_counted_not_dropped() {
        let mut hist = Histogram::new(0.0, 1.0, 2);
        for value in [-5.0, 0.25, 7.0, f64::NAN] {
            hist.push(value);
        }
        assert_eq!(hist.total(), 1);
        assert_eq!(hist.outside(), (1, 1, 1));
    }

    #[test]
    fn a_constant_value_still_makes_a_histogram() {
        let mut hist = Histogram::new(2.0, 2.0, 8);
        hist.push(2.0);
        assert_eq!(hist.total(), 1);
        assert_eq!(hist.outside(), (0, 0, 0));
        assert!(hist.bin_width() > 0.0);
    }

    #[test]
    fn histograms_over_the_same_span_merge() {
        let mut a = Histogram::new(0.0, 2.0, 2);
        a.push(0.5);
        let mut b = Histogram::new(0.0, 2.0, 2);
        b.push(1.5);
        assert_eq!(a.clone().merge(&b).counts(), [1, 1]);
        // A different span is refused rather than mixed in.
        let other = Histogram::new(0.0, 8.0, 2);
        assert_eq!(a.clone().merge(&other), a);
    }

    #[test]
    fn a_profile_counts_zero_crossings_across_chunk_boundaries() {
        let mut profiler = Profiler::new(-1.0, 1.0, 8);
        // Two chunks, and the crossing sits between them.
        profiler.extend([1.0, 1.0, 0.5]);
        profiler.extend([-0.5, -1.0, 0.75]);
        let profile = profiler.finish();
        assert_eq!(profile.zero_crossings, 2);
        assert_eq!(profile.stats.count(), 6);
        assert_eq!(profile.histogram.total(), 6);
    }

    #[test]
    fn zeros_and_gaps_do_not_invent_crossings() {
        let mut profiler = Profiler::new(-1.0, 1.0, 4);
        profiler.extend([1.0, 0.0, 0.0, 1.0, f64::NAN, 1.0]);
        let profile = profiler.finish();
        assert_eq!(profile.zero_crossings, 0);
        assert_eq!(profile.stats.non_finite(), 1);

        let mut through = Profiler::new(-1.0, 1.0, 4);
        through.extend([1.0, 0.0, -1.0]);
        assert_eq!(through.finish().zero_crossings, 1);
    }

    #[test]
    fn min_max_pairs_merge_and_ignore_non_finite() {
        let mut a = MinMax::EMPTY;
        a.include(1.0);
        a.include(f64::NAN);
        let mut b = MinMax::EMPTY;
        b.include(-3.0);
        assert_eq!(a.merge(b), MinMax::new(-3.0, 1.0));
        assert!(MinMax::EMPTY.is_empty());
    }
}
