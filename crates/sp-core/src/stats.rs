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
