//! Detection stages (`docs/DESIGN.md` §9.8).
//!
//! A detector reads signals, emits a typed artifact rather than samples, and
//! leaves the group's signals exactly as it found them so what it found can be
//! drawn over the trace that produced it (§10.3).
//!
//! Two of them. [`Threshold`] answers "where was it loud", which is the
//! question a pulse capture asks; [`PeakFind`] answers "where were the
//! features", which is the question a spectrum or a correlation surface asks,
//! and it answers it with **prominence** rather than with a level, so a
//! ripple on the shoulder of a real peak is not a second detection.

use sp_core::Diagnostic;
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{
    ArtifactOut, PortKind, PortSpec, PropertyPatch, Stage, StageCtx, StageDescriptor, StageOutput,
};
use sp_proc::{GroupFrame, SignalRef};

use crate::artifacts::{Detections, Peaks};
use crate::SIGNALS_IN;

const PARAMS: &[ParamSpec] = &[
    ParamSpec::required(
        "level",
        "Threshold",
        ParamKind::Float {
            min: None,
            max: None,
        },
    ),
    ParamSpec::new(
        "polarity",
        "Crossing",
        ParamKind::Enum {
            variants: &["above", "below"],
        },
        ParamDefault::Text("above"),
    ),
    ParamSpec::new(
        "hysteresis",
        "Hysteresis",
        ParamKind::Float {
            min: Some(0.0),
            max: None,
        },
        ParamDefault::Float(0.0),
    )
    .with_help("How far back past the threshold a detection has to fall to end."),
    ParamSpec::new(
        "min_width_s",
        "Minimum width",
        ParamKind::DurationS,
        ParamDefault::Float(0.0),
    )
    .with_unit("s"),
];

const OUTPUTS: &[PortSpec] = &[PortSpec::required(
    "detections",
    PortKind::Artifact("detections.v1"),
)];

static THRESHOLD: StageDescriptor = StageDescriptor::new("dsp.detect.threshold", 1, "Threshold")
    .describing("Finds spans where a signal crosses a level.")
    .reading(SIGNALS_IN)
    .writing(OUTPUTS)
    .taking(PARAMS);

/// Which side of the threshold counts as a detection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Polarity {
    #[default]
    Above,
    Below,
}

/// Finds spans where a signal is past a threshold.
#[derive(Debug, Default)]
pub struct Threshold {
    level: f64,
    polarity: Polarity,
    hysteresis: f64,
    min_width_s: f64,
}

impl Threshold {
    /// Whether a sample is inside a detection.
    fn is_in(&self, value: f64, already_in: bool) -> bool {
        // Hysteresis widens the exit threshold, so a signal hovering on the
        // level produces one detection rather than a burst of them.
        let level = match (self.polarity, already_in) {
            (Polarity::Above, false) => self.level,
            (Polarity::Above, true) => self.level - self.hysteresis,
            (Polarity::Below, false) => self.level,
            (Polarity::Below, true) => self.level + self.hysteresis,
        };
        match self.polarity {
            Polarity::Above => value >= level,
            Polarity::Below => value <= level,
        }
    }

    /// Detections in one signal, on the absolute timeline.
    fn scan(&self, signal: &SignalRef, into: &mut Detections) -> Result<(), StageError> {
        let values = signal.read_values()?;
        let timebase = signal.timebase();
        let time_of = |index: usize| timebase.time_of(index as u64).unwrap_or(0.0);
        let period = timebase.sample_period_s().unwrap_or(0.0);

        let mut open: Option<(usize, f64)> = None;
        for (index, &value) in values.iter().enumerate() {
            let inside = self.is_in(value, open.is_some());
            match (&mut open, inside) {
                (None, true) => open = Some((index, value)),
                (Some((_, peak)), true) => {
                    let better = match self.polarity {
                        Polarity::Above => value > *peak,
                        Polarity::Below => value < *peak,
                    };
                    if better {
                        *peak = value;
                    }
                }
                (Some((start, peak)), false) => {
                    let (start, peak) = (*start, *peak);
                    open = None;
                    self.close(signal, into, start, index, peak, time_of, period);
                }
                (None, false) => {}
            }
        }
        // A detection still open at the end of the signal ends with it.
        if let Some((start, peak)) = open {
            self.close(signal, into, start, values.len(), peak, time_of, period);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn close(
        &self,
        signal: &SignalRef,
        into: &mut Detections,
        start: usize,
        end: usize,
        peak: f64,
        time_of: impl Fn(usize) -> f64,
        period: f64,
    ) {
        let start_s = time_of(start);
        // The span covers the last sample rather than stopping at its start.
        let end_s = time_of(end.saturating_sub(1)) + period;
        if end_s - start_s + f64::EPSILON < self.min_width_s {
            return;
        }
        into.push(signal.name(), (start_s, end_s), peak);
    }
}

impl Stage for Threshold {
    fn descriptor(&self) -> &'static StageDescriptor {
        &THRESHOLD
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(PARAMS)?;
        self.level = params.f64_or("level", 0.0);
        self.polarity = match params.str_or("polarity", "above") {
            "below" => Polarity::Below,
            _ => Polarity::Above,
        };
        self.hysteresis = params.f64_or("hysteresis", 0.0);
        self.min_width_s = params.f64_or("min_width_s", 0.0);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        let mut detections = Detections::default();
        for signal in &input.signals {
            ctx.check()?;
            self.scan(signal, &mut detections)?;
        }

        output.metric("detections", detections.len() as f64);
        output.metric("widest_s", detections.widest_s());
        // The count is worth having on the group itself: it is what a search
        // for "groups where anything fired" looks at.
        output.properties.push(PropertyPatch::group(
            "detection_count",
            detections.len() as i64,
        ));
        if detections.is_empty() {
            output.diagnose(Diagnostic::info(format!(
                "nothing crossed {:.3}",
                self.level
            )));
        }
        output
            .artifacts
            .push(ArtifactOut::publish("detections", &detections)?);
        Ok(output)
    }
}

const PEAK_PARAMS: &[ParamSpec] = &[
    ParamSpec::new(
        "polarity",
        "Find",
        ParamKind::Enum {
            variants: &["maxima", "minima"],
        },
        ParamDefault::Text("maxima"),
    ),
    ParamSpec::new(
        "min_prominence",
        "Minimum prominence",
        ParamKind::Float {
            min: Some(0.0),
            max: None,
        },
        ParamDefault::Float(0.0),
    )
    .with_help("How far the signal must descend from a peak before a higher one is reached."),
    ParamSpec::new(
        "min_distance_s",
        "Minimum separation",
        ParamKind::DurationS,
        ParamDefault::Float(0.0),
    )
    .with_unit("s")
    .with_help("Of two peaks closer than this, the taller is kept."),
    ParamSpec::new(
        "max_count",
        "Keep at most",
        ParamKind::Int {
            min: Some(0),
            max: None,
        },
        ParamDefault::Int(0),
    )
    .with_help("The tallest n peaks. 0 keeps every peak that passes the filters."),
    ParamSpec::new(
        "window_s",
        "Prominence window",
        ParamKind::DurationS,
        ParamDefault::Float(0.0),
    )
    .with_unit("s")
    .with_help("Bounds the search either side of a peak. 0 searches the whole signal, which costs a pass per peak."),
];

const PEAK_OUTPUTS: &[PortSpec] = &[PortSpec::required("peaks", PortKind::Artifact("peaks.v1"))];

static PEAKS: StageDescriptor = StageDescriptor::new("dsp.detect.peaks", 1, "Peak find")
    .describing("Local maxima ranked by prominence, not by level.")
    .reading(SIGNALS_IN)
    .writing(PEAK_OUTPUTS)
    .taking(PEAK_PARAMS);

/// One candidate, before the filters have had their say.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    index: usize,
    /// Height on the working signal, which is negated for minima so that
    /// "taller" means the same thing either way.
    height: f64,
    prominence: f64,
}

/// Finds the features of a signal, by prominence.
///
/// Prominence is the honest measure: the height of a peak above the highest
/// saddle separating it from anything taller. A level threshold would report
/// every ripple riding on a strong return; a prominence threshold reports the
/// returns.
#[derive(Debug, Default)]
pub struct PeakFind {
    minima: bool,
    min_prominence: f64,
    min_distance_s: f64,
    max_count: usize,
    window_s: f64,
}

impl PeakFind {
    /// Indices of the local maxima of `values`, one per plateau, taken at the
    /// plateau's centre.
    ///
    /// A maximum needs a rise on its left and a fall on its right, so a signal
    /// that opens or closes at its highest value has no peak there: the edge
    /// that would have made it one is off the end of the capture, and claiming
    /// it would be claiming a sample nobody recorded. A NaN is never a peak
    /// and never bounds one, for the same reason.
    fn local_maxima(values: &[f64]) -> Vec<usize> {
        let mut found = Vec::new();
        let mut index = 1;
        while index + 1 < values.len() {
            // `partial_cmp` rather than `>`: a NaN on either side is not a
            // rise, and cannot bound a peak it says nothing about.
            let rises = matches!(
                values[index].partial_cmp(&values[index - 1]),
                Some(std::cmp::Ordering::Greater)
            );
            if !rises {
                index += 1;
                continue;
            }
            // Walk the plateau this sample may be the start of.
            let mut end = index;
            while end + 1 < values.len() && values[end + 1] == values[index] {
                end += 1;
            }
            if end + 1 < values.len() && values[end + 1] < values[index] {
                found.push((index + end) / 2);
            }
            index = end + 1;
        }
        found
    }

    /// How far the signal descends either side of `at` before it climbs to
    /// something at least as high.
    ///
    /// Bounded by `window`, in samples, when the user gave one: unbounded, the
    /// walk is a pass over the signal per peak, which is the cost of an exact
    /// answer. A window can only understate a prominence — what it did not
    /// look at cannot have been deeper — so it is a speed setting rather than
    /// a different measurement.
    ///
    /// "Higher" means strictly higher, so two peaks of exactly the same height
    /// each keep their full prominence: neither one dominates the other.
    fn prominence(values: &[f64], at: usize, window: Option<usize>) -> f64 {
        let height = values[at];
        let reach = |from: usize, to: usize| -> f64 {
            // The deepest point reached before meeting higher ground.
            let mut floor = height;
            let step: isize = if to >= from { 1 } else { -1 };
            let mut index = from as isize;
            loop {
                if index < 0 || index as usize >= values.len() {
                    break;
                }
                let value = values[index as usize];
                if value > height {
                    break;
                }
                if value < floor {
                    floor = value;
                }
                if index as usize == to {
                    break;
                }
                index += step;
            }
            floor
        };

        let span = window.unwrap_or(values.len());
        let left = reach(at, at.saturating_sub(span));
        let right = reach(at, (at + span).min(values.len().saturating_sub(1)));
        height - left.max(right)
    }

    /// The peaks of one signal, already filtered and ordered by time.
    fn scan(&self, signal: &SignalRef, into: &mut Peaks) -> Result<(), StageError> {
        let raw = signal.read_values()?;
        // Minima are maxima of the negated signal; everything downstream then
        // has one case to reason about.
        let values: Vec<f64> = if self.minima {
            raw.iter().map(|v| -v).collect()
        } else {
            raw.clone()
        };

        let timebase = signal.timebase();
        let period = timebase.sample_period_s().unwrap_or(0.0);
        let window = (self.window_s > 0.0 && period > 0.0)
            .then(|| ((self.window_s / period).ceil() as usize).max(1));

        let mut candidates: Vec<Candidate> = Self::local_maxima(&values)
            .into_iter()
            .map(|index| Candidate {
                index,
                height: values[index],
                prominence: Self::prominence(&values, index, window),
            })
            .filter(|candidate| candidate.prominence >= self.min_prominence)
            .collect();

        // Taller first, so both the separation rule and the count keep the
        // peak a reader would have pointed at.
        candidates.sort_by(|a, b| {
            b.height
                .partial_cmp(&a.height)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.index.cmp(&b.index))
        });

        let gap = if period > 0.0 {
            (self.min_distance_s / period).ceil() as usize
        } else {
            0
        };
        let mut kept: Vec<Candidate> = Vec::new();
        for candidate in candidates {
            if gap > 0
                && kept
                    .iter()
                    .any(|other| other.index.abs_diff(candidate.index) < gap)
            {
                continue;
            }
            kept.push(candidate);
            if self.max_count > 0 && kept.len() == self.max_count {
                break;
            }
        }

        kept.sort_by_key(|candidate| candidate.index);
        for candidate in kept {
            let time_s = timebase.time_of(candidate.index as u64).unwrap_or(0.0);
            into.push(
                signal.name(),
                time_s,
                raw[candidate.index],
                candidate.prominence,
            );
        }
        Ok(())
    }
}

impl Stage for PeakFind {
    fn descriptor(&self) -> &'static StageDescriptor {
        &PEAKS
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(PEAK_PARAMS)?;
        self.minima = params.str_or("polarity", "maxima") == "minima";
        self.min_prominence = params.f64_or("min_prominence", 0.0);
        self.min_distance_s = params.f64_or("min_distance_s", 0.0);
        self.max_count = usize::try_from(params.i64_or("max_count", 0)).unwrap_or(0);
        self.window_s = params.f64_or("window_s", 0.0);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        let mut peaks = Peaks::default();
        for signal in &input.signals {
            ctx.check()?;
            self.scan(signal, &mut peaks)?;
        }

        let strongest = peaks.strongest();
        output.metric("peaks", peaks.len() as f64);
        output.metric(
            "prominence_max",
            strongest.map_or(0.0, |at| peaks.prominence[at]),
        );
        // Where the feature is, not just that there is one: charted across
        // groups this is the curve a sweep is looking for.
        output.metric("strongest_s", strongest.map_or(0.0, |at| peaks.time_s[at]));
        output
            .properties
            .push(PropertyPatch::group("peak_count", peaks.len() as i64));
        if peaks.is_empty() {
            output.diagnose(Diagnostic::info(format!(
                "no {} stood {:.3} above their surroundings",
                if self.minima { "minima" } else { "maxima" },
                self.min_prominence
            )));
        }
        output
            .artifacts
            .push(ArtifactOut::publish("peaks", &peaks)?);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{frame, process, values_of};

    fn detector(params: ParamSet) -> Threshold {
        let mut stage = Threshold::default();
        stage.configure(&params).unwrap();
        stage
    }

    fn detections(output: &StageOutput) -> Detections {
        serde_json::from_str(&output.artifacts[0].payload_json).unwrap()
    }

    #[test]
    fn a_span_above_the_level_is_found_on_the_absolute_timeline() {
        // 1 kHz, so each sample is a millisecond.
        let mut stage = detector(ParamSet::new().with("level", 0.5));
        let (_, output) = process(&mut stage, &frame(&[("a", vec![0.0, 1.0, 2.0, 0.0, 0.0])]));
        let found = detections(&output);
        assert_eq!(found.len(), 1);
        let (start, end) = found.spans[0];
        assert!((start - 0.001).abs() < 1e-9, "{start}");
        assert!((end - 0.003).abs() < 1e-9, "{end}");
        assert_eq!(found.peak[0], 2.0);
        assert_eq!(found.signal[0], "a");
    }

    #[test]
    fn samples_are_left_exactly_as_they_were() {
        // Detections are drawn over the trace, so the trace must survive.
        let mut stage = detector(ParamSet::new().with("level", 0.5));
        let input = frame(&[("a", vec![0.0, 1.0, 0.0])]);
        let (next, _) = process(&mut stage, &input);
        assert_eq!(values_of(&next, 0), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn a_detection_open_at_the_end_of_the_signal_still_closes() {
        let mut stage = detector(ParamSet::new().with("level", 0.5));
        let (_, output) = process(&mut stage, &frame(&[("a", vec![0.0, 1.0, 1.0])]));
        let found = detections(&output);
        assert_eq!(found.len(), 1);
        assert!((found.spans[0].1 - 0.003).abs() < 1e-9);
    }

    #[test]
    fn hysteresis_joins_what_would_otherwise_be_two_detections() {
        let values = vec![0.0, 1.0, 0.45, 1.0, 0.0];
        let mut plain = detector(ParamSet::new().with("level", 0.5));
        let (_, without) = process(&mut plain, &frame(&[("a", values.clone())]));
        assert_eq!(detections(&without).len(), 2);

        let mut sticky = detector(ParamSet::new().with("level", 0.5).with("hysteresis", 0.2));
        let (_, with) = process(&mut sticky, &frame(&[("a", values)]));
        assert_eq!(detections(&with).len(), 1);
    }

    #[test]
    fn a_span_narrower_than_the_minimum_is_dropped() {
        let mut stage = detector(
            ParamSet::new()
                .with("level", 0.5)
                .with("min_width_s", 0.003),
        );
        let (_, output) = process(
            &mut stage,
            &frame(&[("a", vec![0.0, 1.0, 0.0, 1.0, 1.0, 1.0, 0.0])]),
        );
        let found = detections(&output);
        assert_eq!(found.len(), 1, "only the three-sample span survives");
        assert!((found.widest_s() - 0.003).abs() < 1e-9);
    }

    #[test]
    fn below_polarity_finds_troughs() {
        let mut stage = detector(
            ParamSet::new()
                .with("level", -0.5)
                .with("polarity", "below"),
        );
        let (_, output) = process(&mut stage, &frame(&[("a", vec![0.0, -1.0, -2.0, 0.0])]));
        let found = detections(&output);
        assert_eq!(found.len(), 1);
        assert_eq!(found.peak[0], -2.0, "the peak is the extreme reached");
    }

    #[test]
    fn a_quiet_group_says_so_and_still_publishes_an_empty_artifact() {
        let mut stage = detector(ParamSet::new().with("level", 5.0));
        let (next, output) = process(&mut stage, &frame(&[("a", vec![0.0, 1.0])]));
        assert!(detections(&output).is_empty());
        assert_eq!(output.metrics.get("detections"), Some(&0.0));
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(next.group.attributes.get_i64("detection_count"), Some(0));
    }

    #[test]
    fn every_signal_of_the_group_is_scanned() {
        let mut stage = detector(ParamSet::new().with("level", 0.5));
        let (_, output) = process(
            &mut stage,
            &frame(&[("a", vec![0.0, 1.0, 0.0]), ("b", vec![1.0, 1.0, 0.0])]),
        );
        let found = detections(&output);
        assert_eq!(found.len(), 2);
        assert_eq!(found.signal, ["a", "b"]);
    }

    #[test]
    fn a_missing_threshold_is_refused() {
        // `level` has no sensible default, so it is required (§9.2).
        let mut stage = Threshold::default();
        let err = stage.configure(&ParamSet::new()).unwrap_err();
        assert_eq!(err.parameter(), Some("level"));
    }
}

#[cfg(test)]
mod peak_tests {
    use super::*;
    use crate::tests::{frame, process, values_of};

    fn finder(params: ParamSet) -> PeakFind {
        let mut stage = PeakFind::default();
        stage.configure(&params).unwrap();
        stage
    }

    fn peaks(output: &StageOutput) -> Peaks {
        serde_json::from_str(&output.artifacts[0].payload_json).unwrap()
    }

    #[test]
    fn every_local_maximum_is_found_with_its_time_and_prominence() {
        // 1 kHz, so sample n is at n milliseconds.
        let mut stage = finder(ParamSet::new());
        let (_, output) = process(
            &mut stage,
            &frame(&[("a", vec![0.0, 1.0, 0.0, 3.0, 0.0, 1.0, 0.0])]),
        );
        let found = peaks(&output);
        assert_eq!(found.len(), 3);
        assert_eq!(found.amplitude, [1.0, 3.0, 1.0]);
        assert_eq!(found.prominence, [1.0, 3.0, 1.0]);
        assert!((found.time_s[1] - 0.003).abs() < 1e-9, "{:?}", found.time_s);
        assert_eq!(found.signal, ["a", "a", "a"]);
    }

    #[test]
    fn a_bump_on_the_flank_of_a_real_peak_is_not_a_second_peak() {
        // The reason the stage filters on prominence rather than on level:
        // the 2.0 bump is *taller* than the 1.0 spike beside it and means
        // nothing, because the signal never comes back down before reaching
        // 5.0.
        let values = vec![0.0, 1.0, 0.0, 2.0, 1.9, 5.0, 0.0];
        let mut plain = finder(ParamSet::new());
        let (_, all) = process(&mut plain, &frame(&[("a", values.clone())]));
        assert_eq!(peaks(&all).len(), 3, "the bump is a local maximum");
        assert!((peaks(&all).prominence[1] - 0.1).abs() < 1e-9);

        let mut fussy = finder(ParamSet::new().with("min_prominence", 0.5));
        let (_, kept) = process(&mut fussy, &frame(&[("a", values)]));
        let found = peaks(&kept);
        assert_eq!(found.len(), 2);
        assert_eq!(found.amplitude, [1.0, 5.0], "the shorter spike survives");
    }

    #[test]
    fn of_two_peaks_closer_than_the_minimum_the_taller_survives() {
        let mut stage = finder(
            ParamSet::new()
                .with("min_distance_s", 0.003)
                .with("min_prominence", 0.5),
        );
        let (_, output) = process(&mut stage, &frame(&[("a", vec![0.0, 2.0, 0.0, 1.0, 0.0])]));
        let found = peaks(&output);
        assert_eq!(found.len(), 1);
        assert_eq!(found.amplitude, [2.0]);
    }

    #[test]
    fn the_count_keeps_the_tallest_and_hands_them_back_in_time_order() {
        let mut stage = finder(ParamSet::new().with("max_count", 2));
        let (_, output) = process(
            &mut stage,
            &frame(&[("a", vec![0.0, 3.0, 0.0, 1.0, 0.0, 2.0, 0.0])]),
        );
        let found = peaks(&output);
        assert_eq!(found.amplitude, [3.0, 2.0], "tallest two, earliest first");
        assert!(found.time_s[0] < found.time_s[1]);
    }

    #[test]
    fn a_plateau_is_one_peak_at_its_centre() {
        let mut stage = finder(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("a", vec![0.0, 1.0, 1.0, 1.0, 0.0])]));
        let found = peaks(&output);
        assert_eq!(found.len(), 1);
        assert!((found.time_s[0] - 0.002).abs() < 1e-9, "{:?}", found.time_s);
    }

    #[test]
    fn minima_are_maxima_of_the_negated_signal_reported_at_their_own_values() {
        let mut stage = finder(ParamSet::new().with("polarity", "minima"));
        let (_, output) = process(
            &mut stage,
            &frame(&[("a", vec![0.0, -1.0, 0.0, -3.0, 0.0])]),
        );
        let found = peaks(&output);
        assert_eq!(found.amplitude, [-1.0, -3.0], "reported as they were");
        assert_eq!(found.prominence, [1.0, 3.0], "prominence is a descent");
        // The deepest trough is the strongest, which the amplitude alone
        // would have got backwards.
        assert_eq!(found.strongest(), Some(1));
    }

    #[test]
    fn a_window_bounds_the_search_rather_than_changing_which_peaks_are_found() {
        // The window trades exactness for a bounded cost, and it can only
        // understate: what it did not look at cannot have been deeper.
        let values = vec![0.0, 1.0, 0.5, 1.0, 0.0];
        let mut unbounded = finder(ParamSet::new());
        let (_, whole) = process(&mut unbounded, &frame(&[("a", values.clone())]));
        let mut bounded = finder(ParamSet::new().with("window_s", 0.001));
        let (_, near) = process(&mut bounded, &frame(&[("a", values)]));

        assert_eq!(peaks(&whole).time_s, peaks(&near).time_s);
        assert_eq!(peaks(&whole).prominence, [1.0, 1.0]);
        // One sample either way reaches the saddle and no further.
        assert_eq!(peaks(&near).prominence, [0.5, 0.5]);
    }

    #[test]
    fn the_samples_are_left_exactly_as_they_were() {
        let mut stage = finder(ParamSet::new());
        let input = frame(&[("a", vec![0.0, 1.0, 0.0])]);
        let (next, _) = process(&mut stage, &input);
        assert_eq!(values_of(&next, 0), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn a_featureless_group_says_so_and_still_publishes_an_empty_artifact() {
        let mut stage = finder(ParamSet::new().with("min_prominence", 10.0));
        let (next, output) = process(&mut stage, &frame(&[("a", vec![0.0, 1.0, 0.0])]));
        assert!(peaks(&output).is_empty());
        assert_eq!(output.metrics.get("peaks"), Some(&0.0));
        assert_eq!(output.metrics.get("prominence_max"), Some(&0.0));
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(next.group.attributes.get_i64("peak_count"), Some(0));
    }

    #[test]
    fn the_metric_says_where_the_strongest_feature_was() {
        let mut stage = finder(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("a", vec![0.0, 1.0, 0.0, 4.0, 0.0])]));
        assert_eq!(output.metrics.get("prominence_max"), Some(&4.0));
        assert_eq!(output.metrics.get("strongest_s"), Some(&0.003));
    }

    #[test]
    fn a_nan_is_never_a_peak_and_never_bounds_one() {
        let mut stage = finder(ParamSet::new());
        let (_, output) = process(
            &mut stage,
            &frame(&[("a", vec![0.0, f64::NAN, 5.0, f64::NAN, 0.0])]),
        );
        // Nothing here is a maximum the stage can honestly claim: 5.0 has an
        // unknown value on both sides.
        assert!(peaks(&output).is_empty());
    }
}
