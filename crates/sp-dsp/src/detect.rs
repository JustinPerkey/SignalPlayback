//! Threshold detection (`docs/DESIGN.md` §9.8).
//!
//! The simplest detector there is, and the one that shows the shape of every
//! other: it reads signals, emits a typed artifact rather than samples, and
//! leaves the group's signals exactly as it found them so the detections can
//! be drawn over the trace they came from (§10.3).

use sp_core::Diagnostic;
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{
    ArtifactOut, PortKind, PortSpec, PropertyPatch, Stage, StageCtx, StageDescriptor, StageOutput,
};
use sp_proc::{GroupFrame, SignalRef};

use crate::artifacts::Detections;
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
