//! Biquad filtering (`docs/DESIGN.md` §9.8).
//!
//! One RBJ cookbook section, applied `sections` times in cascade, in direct
//! form I. The coefficients are computed once in `configure` — they depend on
//! the sample rate, so a group whose rate differs from the one the filter was
//! designed for recomputes them per group rather than filtering at the wrong
//! corner silently.

use sp_core::Diagnostic;
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{AttrPatch, Stage, StageCtx, StageDescriptor, StageOutput};
use sp_proc::{GroupFrame, SignalOut};

use crate::{buffer_like, SIGNALS_IN, SIGNALS_OUT};

const PARAMS: &[ParamSpec] = &[
    ParamSpec::new(
        "response",
        "Response",
        ParamKind::Enum {
            variants: &["lowpass", "highpass", "bandpass", "notch"],
        },
        ParamDefault::Text("lowpass"),
    ),
    ParamSpec::required(
        "cutoff_hz",
        "Corner",
        ParamKind::FreqHz {
            min: Some(0.0),
            max: None,
        },
    )
    .with_help("Centre frequency for bandpass and notch.")
    .with_unit("Hz"),
    ParamSpec::new(
        "q",
        "Q",
        ParamKind::Float {
            min: Some(0.05),
            max: Some(100.0),
        },
        ParamDefault::Float(std::f64::consts::FRAC_1_SQRT_2),
    )
    .with_help("0.707 is the flattest single section."),
    ParamSpec::new(
        "sections",
        "Sections",
        ParamKind::Int {
            min: Some(1),
            max: Some(8),
        },
        ParamDefault::Int(1),
    )
    .with_help("Cascaded copies; each doubles the roll-off."),
];

static BIQUAD: StageDescriptor = StageDescriptor::new("dsp.filter.biquad", 1, "Biquad filter")
    .describing("Low, high, band or notch, as a cascade of RBJ biquads.")
    .reading(SIGNALS_IN)
    .writing(SIGNALS_OUT)
    .taking(PARAMS);

/// The four responses one biquad section can take.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Response {
    #[default]
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
}

impl Response {
    fn parse(token: &str) -> Self {
        match token {
            "highpass" => Self::Highpass,
            "bandpass" => Self::Bandpass,
            "notch" => Self::Notch,
            _ => Self::Lowpass,
        }
    }
}

/// Normalised direct-form-I coefficients of one section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefficients {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coefficients {
    /// RBJ cookbook design for one section at `cutoff_hz` in a signal sampled
    /// at `rate_hz`.
    #[must_use]
    pub fn design(response: Response, cutoff_hz: f64, q: f64, rate_hz: f64) -> Self {
        let w0 = std::f64::consts::TAU * cutoff_hz / rate_hz;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match response {
            Response::Lowpass => {
                let b1 = 1.0 - cos;
                (b1 / 2.0, b1, b1 / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Response::Highpass => {
                let b0 = (1.0 + cos) / 2.0;
                (b0, -(1.0 + cos), b0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            // Constant 0 dB peak gain.
            Response::Bandpass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            Response::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Filters `values` through one section, starting from rest.
    #[must_use]
    pub fn apply(&self, values: &[f64]) -> Vec<f64> {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        values
            .iter()
            .map(|&x| {
                let y = self.b0 * x + self.b1 * x1 + self.b2 * x2 - self.a1 * y1 - self.a2 * y2;
                x2 = x1;
                x1 = x;
                y2 = y1;
                y1 = y;
                y
            })
            .collect()
    }
}

/// A cascade of identical biquad sections.
#[derive(Debug)]
pub struct Biquad {
    response: Response,
    cutoff_hz: f64,
    q: f64,
    sections: usize,
}

impl Default for Biquad {
    fn default() -> Self {
        Self {
            response: Response::Lowpass,
            cutoff_hz: 0.0,
            q: std::f64::consts::FRAC_1_SQRT_2,
            sections: 1,
        }
    }
}

impl Stage for Biquad {
    fn descriptor(&self) -> &'static StageDescriptor {
        &BIQUAD
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(PARAMS)?;
        self.response = Response::parse(params.str_or("response", "lowpass"));
        self.cutoff_hz = params.f64_or("cutoff_hz", 0.0);
        self.q = params.f64_or("q", std::f64::consts::FRAC_1_SQRT_2);
        self.sections = params.i64_or("sections", 1).max(1) as usize;
        if self.cutoff_hz <= 0.0 {
            return Err(ConfigError::rejected(
                "the corner frequency must be above 0 Hz",
            ));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        for (ordinal, signal) in input.signals.iter().enumerate() {
            ctx.check()?;
            let Some(nyquist) = signal.timebase().nyquist_hz() else {
                output.diagnose(
                    Diagnostic::warn(format!(
                        "'{}' has no regular sample rate, so it was not filtered",
                        signal.name()
                    ))
                    .about_signal(ordinal as u32),
                );
                continue;
            };
            // Above Nyquist the design folds back and would filter at some
            // other frequency entirely — better to say so than to do it.
            if self.cutoff_hz >= nyquist {
                return Err(StageError::rejected(format!(
                    "the corner at {:.1} Hz is at or above the Nyquist frequency of '{}' ({:.1} Hz)",
                    self.cutoff_hz,
                    signal.name(),
                    nyquist
                )));
            }

            let rate_hz = signal.timebase().sample_rate_hz.unwrap_or_default();
            let coefficients = Coefficients::design(self.response, self.cutoff_hz, self.q, rate_hz);
            let mut values = signal.read_values()?;
            for _ in 0..self.sections {
                ctx.check()?;
                values = coefficients.apply(&values);
            }

            output.set_signal(
                ordinal,
                SignalOut::Replace {
                    ordinal,
                    samples: buffer_like(signal, &values),
                    patch: AttrPatch::new()
                        .set("filter_cutoff_hz", self.cutoff_hz)
                        .set("filter_sections", self.sections as i64),
                },
            );
        }
        output.metric("cutoff_hz", self.cutoff_hz);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{frame, frame_at, process, try_process, values_of};

    fn rms(values: &[f64]) -> f64 {
        (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt()
    }

    /// A sine at `hz`, sampled at 1 kHz for one second.
    fn tone(hz: f64) -> Vec<f64> {
        (0..1000)
            .map(|i| (std::f64::consts::TAU * hz * i as f64 / 1000.0).sin())
            .collect()
    }

    fn lowpass(cutoff_hz: f64) -> Biquad {
        let mut stage = Biquad::default();
        stage
            .configure(&ParamSet::new().with("cutoff_hz", cutoff_hz))
            .unwrap();
        stage
    }

    #[test]
    fn a_lowpass_keeps_what_is_below_it_and_removes_what_is_above() {
        let mut stage = lowpass(50.0);
        let (passed, _) = process(&mut stage, &frame(&[("low", tone(10.0))]));
        let (stopped, _) = process(&mut stage, &frame(&[("high", tone(400.0))]));

        // Ignore the settling transient at the start of each.
        let passed = values_of(&passed, 0)[200..].to_vec();
        let stopped = values_of(&stopped, 0)[200..].to_vec();
        assert!(rms(&passed) > 0.6, "{}", rms(&passed));
        assert!(rms(&stopped) < 0.05, "{}", rms(&stopped));
    }

    #[test]
    fn a_highpass_does_the_opposite() {
        let mut stage = Biquad::default();
        stage
            .configure(
                &ParamSet::new()
                    .with("response", "highpass")
                    .with("cutoff_hz", 200.0),
            )
            .unwrap();
        let (passed, _) = process(&mut stage, &frame(&[("high", tone(400.0))]));
        let (stopped, _) = process(&mut stage, &frame(&[("low", tone(10.0))]));
        assert!(rms(&values_of(&passed, 0)[200..]) > 0.6);
        assert!(rms(&values_of(&stopped, 0)[200..]) < 0.05);
    }

    #[test]
    fn a_notch_removes_its_centre_and_leaves_the_rest() {
        let mut stage = Biquad::default();
        stage
            .configure(
                &ParamSet::new()
                    .with("response", "notch")
                    .with("cutoff_hz", 100.0)
                    .with("q", 8.0),
            )
            .unwrap();
        let (notched, _) = process(&mut stage, &frame(&[("hum", tone(100.0))]));
        let (kept, _) = process(&mut stage, &frame(&[("wanted", tone(300.0))]));
        assert!(rms(&values_of(&notched, 0)[300..]) < 0.1);
        assert!(rms(&values_of(&kept, 0)[300..]) > 0.6);
    }

    #[test]
    fn cascading_sections_steepens_the_stop_band() {
        let mut one = lowpass(50.0);
        let mut four = Biquad::default();
        four.configure(&ParamSet::new().with("cutoff_hz", 50.0).with("sections", 4))
            .unwrap();

        let input = frame(&[("high", tone(200.0))]);
        let (single, _) = process(&mut one, &input);
        let (quad, _) = process(&mut four, &input);
        assert!(rms(&values_of(&quad, 0)[300..]) < rms(&values_of(&single, 0)[300..]) / 10.0);
    }

    #[test]
    fn a_dc_signal_passes_a_lowpass_at_unity() {
        let mut stage = lowpass(100.0);
        let (next, _) = process(&mut stage, &frame(&[("dc", vec![1.0; 500])]));
        let settled = values_of(&next, 0)[400];
        assert!((settled - 1.0).abs() < 1e-6, "{settled}");
    }

    #[test]
    fn a_corner_above_nyquist_is_refused_rather_than_folded() {
        let mut stage = lowpass(900.0);
        let err = try_process(&mut stage, &frame(&[("a", tone(10.0))])).unwrap_err();
        assert!(err.to_string().contains("Nyquist"), "{err}");
    }

    #[test]
    fn an_irregular_signal_is_left_alone_with_a_warning() {
        let mut stage = lowpass(50.0);
        let input = frame_at(sp_core::Timebase::irregular(0.0), &[("a", vec![1.0, 2.0])]);
        let (next, output) = process(&mut stage, &input);
        assert_eq!(values_of(&next, 0), [1.0, 2.0]);
        assert_eq!(output.diagnostics.len(), 1);
    }

    #[test]
    fn a_corner_of_zero_is_refused_at_configure_time() {
        let mut stage = Biquad::default();
        let err = stage
            .configure(&ParamSet::new().with("cutoff_hz", 0.0))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Rejected(_)), "{err}");
    }

    #[test]
    fn the_filter_records_what_it_did_on_the_signal() {
        let mut stage = lowpass(50.0);
        let (next, _) = process(&mut stage, &frame(&[("a", tone(10.0))]));
        assert_eq!(
            next.signals[0].attributes().get_f64("filter_cutoff_hz"),
            Some(50.0)
        );
    }
}
