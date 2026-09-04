//! Conditioning stages: gain, detrend, normalise (`docs/DESIGN.md` §9.8).
//!
//! These are the plainest stages there are, and they exist as much to exercise
//! the harness as to be useful: each one replaces samples in place, says what
//! it did to every input signal, and reports a metric the results screen can
//! chart across groups.

use sp_core::Diagnostic;
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{AttrPatch, Stage, StageCtx, StageDescriptor, StageOutput};
use sp_proc::{GroupFrame, SignalOut};

use crate::{buffer_like, SIGNALS_IN, SIGNALS_OUT};

// ---------------------------------------------------------------------------
// Gain
// ---------------------------------------------------------------------------

const GAIN_PARAMS: &[ParamSpec] = &[ParamSpec::new(
    "gain",
    "Gain",
    ParamKind::Float {
        min: Some(-1.0e9),
        max: Some(1.0e9),
    },
    ParamDefault::Float(1.0),
)
.with_help("Every sample is multiplied by this.")];

static GAIN: StageDescriptor = StageDescriptor::new("dsp.condition.gain", 1, "Gain")
    .describing("Scales every sample by a constant.")
    .reading(SIGNALS_IN)
    .writing(SIGNALS_OUT)
    .taking(GAIN_PARAMS);

/// Multiplies every sample by a constant.
#[derive(Debug, Default)]
pub struct Gain {
    gain: f64,
}

impl Stage for Gain {
    fn descriptor(&self) -> &'static StageDescriptor {
        &GAIN
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(GAIN_PARAMS)?;
        self.gain = params.f64_or("gain", 1.0);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        for (ordinal, signal) in input.signals.iter().enumerate() {
            ctx.check()?;
            let values: Vec<f64> = signal
                .read_values()?
                .into_iter()
                .map(|v| v * self.gain)
                .collect();
            let peak = values
                .iter()
                .copied()
                .fold(0.0_f64, |acc, v| acc.max(v.abs()));
            output.metric(format!("{}.peak", signal.name()), peak);
            output.set_signal(
                ordinal,
                SignalOut::Replace {
                    ordinal,
                    samples: buffer_like(signal, &values),
                    patch: AttrPatch::new().set("gain_applied", self.gain),
                },
            );
        }
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Detrend
// ---------------------------------------------------------------------------

const DETREND_PARAMS: &[ParamSpec] = &[ParamSpec::new(
    "mode",
    "Mode",
    ParamKind::Enum {
        variants: &["mean", "linear"],
    },
    ParamDefault::Text("mean"),
)
.with_help("Remove the average, or a straight-line fit.")];

static DETREND: StageDescriptor = StageDescriptor::new("dsp.condition.detrend", 1, "Detrend")
    .describing("Removes a DC offset or a linear trend.")
    .reading(SIGNALS_IN)
    .writing(SIGNALS_OUT)
    .taking(DETREND_PARAMS);

/// What [`Detrend`] removes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DetrendMode {
    #[default]
    Mean,
    Linear,
}

/// Removes a constant offset or a straight-line trend.
#[derive(Debug, Default)]
pub struct Detrend {
    mode: DetrendMode,
}

impl Detrend {
    /// Returns the detrended values and the offset that was removed at the
    /// midpoint, which is the number worth reporting as a metric.
    fn apply(&self, values: &[f64]) -> (Vec<f64>, f64) {
        if values.is_empty() {
            return (Vec::new(), 0.0);
        }
        let n = values.len() as f64;
        let mean = values.iter().sum::<f64>() / n;
        match self.mode {
            DetrendMode::Mean => (values.iter().map(|v| v - mean).collect(), mean),
            DetrendMode::Linear => {
                // Least-squares fit against the sample index. Centring the
                // index makes the sums small and the arithmetic stable even
                // for a long signal.
                let centre = (n - 1.0) / 2.0;
                let mut sxy = 0.0;
                let mut sxx = 0.0;
                for (i, value) in values.iter().enumerate() {
                    let x = i as f64 - centre;
                    sxy += x * (value - mean);
                    sxx += x * x;
                }
                let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
                let out = values
                    .iter()
                    .enumerate()
                    .map(|(i, value)| value - (mean + slope * (i as f64 - centre)))
                    .collect();
                (out, mean)
            }
        }
    }
}

impl Stage for Detrend {
    fn descriptor(&self) -> &'static StageDescriptor {
        &DETREND
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(DETREND_PARAMS)?;
        self.mode = match params.str_or("mode", "mean") {
            "linear" => DetrendMode::Linear,
            _ => DetrendMode::Mean,
        };
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        for (ordinal, signal) in input.signals.iter().enumerate() {
            ctx.check()?;
            let (values, removed) = self.apply(&signal.read_values()?);
            output.metric(format!("{}.offset_removed", signal.name()), removed);
            output.set_signal(
                ordinal,
                SignalOut::replace(ordinal, buffer_like(signal, &values)),
            );
        }
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Normalise
// ---------------------------------------------------------------------------

const NORMALISE_PARAMS: &[ParamSpec] = &[
    ParamSpec::new(
        "mode",
        "Reference",
        ParamKind::Enum {
            variants: &["peak", "rms"],
        },
        ParamDefault::Text("peak"),
    )
    .with_help("Scale so the peak, or the RMS, hits the target."),
    ParamSpec::new(
        "target",
        "Target",
        ParamKind::Float {
            min: Some(1.0e-12),
            max: Some(1.0e9),
        },
        ParamDefault::Float(1.0),
    ),
];

static NORMALISE: StageDescriptor = StageDescriptor::new("dsp.condition.normalise", 1, "Normalise")
    .describing("Scales each signal to a target peak or RMS level.")
    .reading(SIGNALS_IN)
    .writing(SIGNALS_OUT)
    .taking(NORMALISE_PARAMS);

/// What [`Normalise`] measures before scaling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NormaliseMode {
    #[default]
    Peak,
    Rms,
}

/// Scales each signal so its peak or RMS hits a target.
#[derive(Debug)]
pub struct Normalise {
    mode: NormaliseMode,
    target: f64,
}

impl Default for Normalise {
    fn default() -> Self {
        Self {
            mode: NormaliseMode::Peak,
            target: 1.0,
        }
    }
}

impl Stage for Normalise {
    fn descriptor(&self) -> &'static StageDescriptor {
        &NORMALISE
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(NORMALISE_PARAMS)?;
        self.mode = match params.str_or("mode", "peak") {
            "rms" => NormaliseMode::Rms,
            _ => NormaliseMode::Peak,
        };
        self.target = params.f64_or("target", 1.0);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        for (ordinal, signal) in input.signals.iter().enumerate() {
            ctx.check()?;
            let values = signal.read_values()?;
            let reference = match self.mode {
                NormaliseMode::Peak => values.iter().fold(0.0_f64, |acc, v| acc.max(v.abs())),
                NormaliseMode::Rms => {
                    if values.is_empty() {
                        0.0
                    } else {
                        (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt()
                    }
                }
            };

            // A silent signal has nothing to normalise against; scaling it by
            // anything would only amplify whatever noise floor is left.
            if reference <= f64::EPSILON {
                output.diagnose(
                    Diagnostic::warn(format!(
                        "'{}' is flat, so it was left unscaled",
                        signal.name()
                    ))
                    .about_signal(ordinal as u32),
                );
                continue;
            }

            let scale = self.target / reference;
            output.metric(format!("{}.scale", signal.name()), scale);
            let scaled: Vec<f64> = values.into_iter().map(|v| v * scale).collect();
            output.set_signal(
                ordinal,
                SignalOut::Replace {
                    ordinal,
                    samples: buffer_like(signal, &scaled),
                    patch: AttrPatch::new().set("normalised_to", self.target),
                },
            );
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{frame, process, values_of};

    #[test]
    fn gain_scales_every_sample_and_reports_the_peak() {
        let mut stage = Gain::default();
        stage.configure(&ParamSet::new().with("gain", 2.0)).unwrap();
        let frame = frame(&[("a", vec![1.0, -2.0, 0.5])]);
        let (next, output) = process(&mut stage, &frame);
        assert_eq!(values_of(&next, 0), [2.0, -4.0, 1.0]);
        assert_eq!(output.metrics.get("a.peak"), Some(&4.0));
        assert_eq!(
            next.signals[0].attributes().get_f64("gain_applied"),
            Some(2.0)
        );
    }

    #[test]
    fn a_gain_outside_the_declared_range_is_refused_before_the_run() {
        let mut stage = Gain::default();
        let err = stage
            .configure(&ParamSet::new().with("gain", 1.0e12))
            .unwrap_err();
        assert_eq!(err.parameter(), Some("gain"));
    }

    #[test]
    fn detrend_removes_the_mean() {
        let mut stage = Detrend::default();
        stage.configure(&ParamSet::new()).unwrap();
        let frame = frame(&[("a", vec![1.0, 2.0, 3.0])]);
        let (next, output) = process(&mut stage, &frame);
        assert_eq!(values_of(&next, 0), [-1.0, 0.0, 1.0]);
        assert_eq!(output.metrics.get("a.offset_removed"), Some(&2.0));
    }

    #[test]
    fn linear_detrend_flattens_a_ramp() {
        let mut stage = Detrend::default();
        stage
            .configure(&ParamSet::new().with("mode", "linear"))
            .unwrap();
        let frame = frame(&[("a", vec![0.0, 1.0, 2.0, 3.0, 4.0])]);
        let (next, _) = process(&mut stage, &frame);
        for value in values_of(&next, 0) {
            assert!(value.abs() < 1e-12, "a straight line detrends to nothing");
        }
    }

    #[test]
    fn normalise_scales_the_peak_to_the_target() {
        let mut stage = Normalise::default();
        stage
            .configure(&ParamSet::new().with("target", 2.0))
            .unwrap();
        let frame = frame(&[("a", vec![0.0, -4.0, 2.0])]);
        let (next, output) = process(&mut stage, &frame);
        assert_eq!(values_of(&next, 0), [0.0, -2.0, 1.0]);
        assert_eq!(output.metrics.get("a.scale"), Some(&0.5));
    }

    #[test]
    fn normalise_by_rms_uses_the_rms() {
        let mut stage = Normalise::default();
        stage
            .configure(&ParamSet::new().with("mode", "rms"))
            .unwrap();
        // RMS of ±3 is 3, so the samples come back as ±1.
        let frame = frame(&[("a", vec![3.0, -3.0, 3.0, -3.0])]);
        let (next, _) = process(&mut stage, &frame);
        assert_eq!(values_of(&next, 0), [1.0, -1.0, 1.0, -1.0]);
    }

    #[test]
    fn a_flat_signal_is_left_alone_with_a_warning() {
        let mut stage = Normalise::default();
        stage.configure(&ParamSet::new()).unwrap();
        let frame = frame(&[("flat", vec![0.0, 0.0, 0.0])]);
        let (next, output) = process(&mut stage, &frame);
        assert_eq!(values_of(&next, 0), [0.0, 0.0, 0.0]);
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].signal_ordinal, Some(0));
    }

    #[test]
    fn every_signal_of_a_group_is_accounted_for() {
        // The frame refuses an output that forgets one, so this also proves
        // these stages satisfy the §9.4 contract on a multi-signal group.
        let mut stage = Gain::default();
        stage.configure(&ParamSet::new()).unwrap();
        let frame = frame(&[("a", vec![1.0]), ("b", vec![2.0]), ("c", vec![3.0])]);
        let (next, _) = process(&mut stage, &frame);
        assert_eq!(next.signals.len(), 3);
    }
}
