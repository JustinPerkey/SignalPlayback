//! Transform stages (`docs/DESIGN.md` §9.8).
//!
//! The FFT changes no samples: it publishes a `Spectrum` artifact for the
//! results screen and peak metrics for the chart across groups, the way a
//! measurement stage does. One spectrum per group, from one named signal —
//! a port holds a single value (§9.3), so a per-signal spectrum would leave
//! only the last one visible.

use sp_core::Diagnostic;
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{
    ArtifactOut, PortKind, PortSpec, Stage, StageCtx, StageDescriptor, StageOutput,
};
use sp_proc::{GroupFrame, SignalRef};

use crate::artifacts::Spectrum;
use crate::SIGNALS_IN;

/// Below this an amplitude is reported as the floor rather than as -inf dB,
/// which no chart can scale to.
const FLOOR_DB: f64 = -300.0;

const PARAMS: &[ParamSpec] = &[
    ParamSpec::new("signal", "Signal", ParamKind::Text, ParamDefault::Text(""))
        .with_help("Which signal to transform. Empty means the first in the group."),
    ParamSpec::new(
        "window",
        "Window",
        ParamKind::Enum {
            variants: &["rectangular", "hann", "hamming", "blackman-harris"],
        },
        ParamDefault::Text("hann"),
    )
    .with_help("Rectangular leaks; Hann is the safe default for an unknown signal."),
    ParamSpec::new(
        "size",
        "Length",
        ParamKind::Int {
            min: Some(0),
            max: None,
        },
        ParamDefault::Int(0),
    )
    .with_help("Samples to transform, rounded up to a power of two. 0 uses the whole signal."),
];

const OUTPUTS: &[PortSpec] = &[
    PortSpec::optional("spectrum", PortKind::Artifact("spectrum.v1")),
    // The signals flow on untouched, so an FFT can sit mid-pipeline as an
    // inspection point rather than only at the end.
    PortSpec::required("signals", PortKind::Signals { domain: None }),
];

static FFT: StageDescriptor = StageDescriptor::new("dsp.transform.fft", 1, "FFT")
    .describing("Single-sided magnitude spectrum of one signal, in dB.")
    .reading(SIGNALS_IN)
    .writing(OUTPUTS)
    .taking(PARAMS);

/// The window applied before the transform.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Window {
    Rectangular,
    #[default]
    Hann,
    Hamming,
    BlackmanHarris,
}

impl Window {
    fn parse(token: &str) -> Self {
        match token {
            "rectangular" => Self::Rectangular,
            "hamming" => Self::Hamming,
            "blackman-harris" => Self::BlackmanHarris,
            _ => Self::Hann,
        }
    }

    /// The window's coefficients over `len` samples, periodic rather than
    /// symmetric — the convention a spectrum wants, since the frame is one
    /// period of an assumed-repeating signal.
    #[must_use]
    pub fn coefficients(self, len: usize) -> Vec<f64> {
        if matches!(self, Self::Rectangular) || len == 0 {
            return vec![1.0; len];
        }
        (0..len)
            .map(|i| {
                let t = std::f64::consts::TAU * i as f64 / len as f64;
                match self {
                    Self::Rectangular => 1.0,
                    Self::Hann => 0.5 - 0.5 * t.cos(),
                    Self::Hamming => 0.54 - 0.46 * t.cos(),
                    Self::BlackmanHarris => {
                        0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos()
                            - 0.01168 * (3.0 * t).cos()
                    }
                }
            })
            .collect()
    }
}

/// One signal of a group as a magnitude spectrum.
#[derive(Debug, Default)]
pub struct Fft {
    signal: String,
    window: Window,
    size: usize,
}

impl Stage for Fft {
    fn descriptor(&self) -> &'static StageDescriptor {
        &FFT
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(PARAMS)?;
        self.signal = params.str_or("signal", "").trim().to_owned();
        self.window = Window::parse(params.str_or("window", "hann"));
        self.size = params.i64_or("size", 0).max(0) as usize;
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        let Some((ordinal, signal)) = self.pick(input)? else {
            // An empty group is not an error: an upstream stage may drop every
            // signal, and the rail should say so rather than fail the run.
            output.diagnose(Diagnostic::warn("the group has no signals to transform"));
            return Ok(output);
        };

        let Some(rate_hz) = signal.timebase().sample_rate_hz else {
            output.diagnose(
                Diagnostic::warn(format!(
                    "'{}' has no regular sample rate, so no spectrum was computed",
                    signal.name()
                ))
                .about_signal(ordinal as u32),
            );
            return Ok(output);
        };

        let values = signal.read_values()?;
        if values.is_empty() {
            output.diagnose(
                Diagnostic::warn(format!("'{}' has no samples", signal.name()))
                    .about_signal(ordinal as u32),
            );
            return Ok(output);
        }

        ctx.check()?;
        let taken = if self.size == 0 {
            values.len()
        } else {
            self.size.min(values.len())
        };
        let length = taken.next_power_of_two();
        let spectrum = self.transform(signal.name(), &values[..taken], length, rate_hz);
        ctx.check()?;

        if let Some((hz, db)) = spectrum.peak() {
            output.metric("peak_hz", hz);
            output.metric("peak_db", db);
        }
        output.metric("fft_size", length as f64);
        output
            .artifacts
            .push(ArtifactOut::publish("spectrum", &spectrum)?);
        Ok(output)
    }
}

impl Fft {
    /// The signal named by the parameter, or the first in the group.
    fn pick<'a>(
        &self,
        input: &'a GroupFrame,
    ) -> Result<Option<(usize, &'a SignalRef)>, StageError> {
        if self.signal.is_empty() {
            return Ok(input.signals.first().map(|signal| (0, signal)));
        }
        input
            .signals
            .iter()
            .enumerate()
            .find(|(_, signal)| signal.name() == self.signal)
            .map(Some)
            // A named signal that is missing is a configuration mistake rather
            // than a property of the data, so it fails instead of warning.
            .ok_or_else(|| {
                StageError::rejected(format!("the group has no signal named '{}'", self.signal))
            })
    }

    /// `values` windowed, zero-padded to `length` and transformed into a
    /// single-sided amplitude spectrum in dB.
    fn transform(&self, name: &str, values: &[f64], length: usize, rate_hz: f64) -> Spectrum {
        let window = self.window.coefficients(values.len());
        // Dividing by the window's sum rather than by the length is what makes
        // a full-scale sine read 0 dB under any window.
        let gain: f64 = window.iter().sum();

        let mut re = vec![0.0; length];
        let mut im = vec![0.0; length];
        for (index, (value, weight)) in values.iter().zip(&window).enumerate() {
            re[index] = value * weight;
        }
        fft_in_place(&mut re, &mut im);

        let bins = length / 2 + 1;
        let mut freq_hz = Vec::with_capacity(bins);
        let mut magnitude_db = Vec::with_capacity(bins);
        for k in 0..bins {
            // DC and Nyquist appear once in the two-sided transform; every
            // other bin has a mirror carrying half of the amplitude.
            let mirrored = k > 0 && k < length / 2;
            let scale = if mirrored { 2.0 } else { 1.0 };
            let magnitude = scale * re[k].hypot(im[k]) / gain;
            freq_hz.push(k as f64 * rate_hz / length as f64);
            magnitude_db.push(if magnitude > 0.0 {
                (20.0 * magnitude.log10()).max(FLOOR_DB)
            } else {
                FLOOR_DB
            });
        }
        Spectrum::new(name, freq_hz, magnitude_db)
    }
}

/// Iterative radix-2 Cooley-Tukey, decimation in time.
///
/// `re` and `im` must share a power-of-two length. Written here rather than
/// pulled in: a stage has to be deterministic across machines to be cacheable
/// and to hold a baseline (§9.5), and this is a page of arithmetic with no
/// such question about it.
fn fft_in_place(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    debug_assert_eq!(n, im.len());
    debug_assert!(n.is_power_of_two());
    if n < 2 {
        return;
    }

    // Bit-reversal permutation.
    let mut target = 0usize;
    for source in 1..n {
        let mut bit = n >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target |= bit;
        if source < target {
            re.swap(source, target);
            im.swap(source, target);
        }
    }

    let mut span = 2;
    while span <= n {
        let step = -std::f64::consts::TAU / span as f64;
        for start in (0..n).step_by(span) {
            for k in 0..span / 2 {
                let (sin, cos) = (step * k as f64).sin_cos();
                let (a, b) = (start + k, start + k + span / 2);
                let tre = cos * re[b] - sin * im[b];
                let tim = sin * re[b] + cos * im[b];
                re[b] = re[a] - tre;
                im[b] = im[a] - tim;
                re[a] += tre;
                im[a] += tim;
            }
        }
        span <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{frame, frame_at, process, try_process, values_of};

    /// A sine of unit amplitude at `hz`, sampled at 1 kHz for `len` samples.
    fn tone(hz: f64, len: usize) -> Vec<f64> {
        (0..len)
            .map(|i| (std::f64::consts::TAU * hz * i as f64 / 1000.0).sin())
            .collect()
    }

    fn fft(params: ParamSet) -> Fft {
        let mut stage = Fft::default();
        stage.configure(&params).unwrap();
        stage
    }

    fn spectrum_of(output: &StageOutput) -> Spectrum {
        serde_json::from_str(&output.artifacts[0].payload_json).unwrap()
    }

    #[test]
    fn a_tone_peaks_in_its_own_bin_at_its_own_amplitude() {
        // 125 Hz at 1 kHz over 512 samples falls exactly on bin 64, so the
        // window spreads nothing and the amplitude is readable to a fraction
        // of a dB.
        let mut stage = fft(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("rf", tone(125.0, 512))]));
        let (hz, db) = spectrum_of(&output).peak().unwrap();
        assert!((hz - 125.0).abs() < 1e-9, "{hz}");
        assert!(db.abs() < 0.1, "{db} dB should be full scale");
    }

    #[test]
    fn every_window_reads_the_same_amplitude() {
        for window in ["rectangular", "hann", "hamming", "blackman-harris"] {
            let mut stage = fft(ParamSet::new().with("window", window));
            let (_, output) = process(&mut stage, &frame(&[("rf", tone(125.0, 512))]));
            let (_, db) = spectrum_of(&output).peak().unwrap();
            assert!(db.abs() < 0.3, "{window} read {db} dB");
        }
    }

    #[test]
    fn dc_lands_in_bin_zero_without_the_mirror_factor() {
        let mut stage = fft(ParamSet::new().with("window", "rectangular"));
        let (_, output) = process(&mut stage, &frame(&[("dc", vec![1.0; 256])]));
        let spectrum = spectrum_of(&output);
        assert_eq!(spectrum.freq_hz[0], 0.0);
        assert!(
            spectrum.magnitude_db[0].abs() < 1e-9,
            "{:?}",
            spectrum.peak()
        );
    }

    #[test]
    fn the_bins_run_from_dc_to_nyquist() {
        let mut stage = fft(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("rf", tone(125.0, 512))]));
        let spectrum = spectrum_of(&output);
        assert_eq!(spectrum.len(), 512 / 2 + 1);
        assert_eq!(spectrum.freq_hz.last().copied(), Some(500.0));
    }

    #[test]
    fn a_length_that_is_not_a_power_of_two_is_padded_up_to_one() {
        let mut stage = fft(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("rf", tone(125.0, 300))]));
        assert_eq!(output.metrics.get("fft_size"), Some(&512.0));
        assert_eq!(spectrum_of(&output).len(), 512 / 2 + 1);
    }

    #[test]
    fn a_shorter_length_transforms_only_the_start_of_the_signal() {
        let mut stage = fft(ParamSet::new().with("size", 128));
        let (_, output) = process(&mut stage, &frame(&[("rf", tone(125.0, 1024))]));
        assert_eq!(output.metrics.get("fft_size"), Some(&128.0));
        let (hz, _) = spectrum_of(&output).peak().unwrap();
        assert!((hz - 125.0).abs() < 1e-9, "{hz}");
    }

    #[test]
    fn the_samples_flow_on_untouched() {
        let mut stage = fft(ParamSet::new());
        let input = frame(&[("rf", tone(125.0, 256)), ("other", vec![1.0; 256])]);
        let (next, _) = process(&mut stage, &input);
        assert_eq!(next.signals.len(), 2);
        assert_eq!(values_of(&next, 0), values_of(&input, 0));
    }

    #[test]
    fn a_named_signal_is_transformed_instead_of_the_first() {
        let mut stage = fft(ParamSet::new().with("signal", "second"));
        let input = frame(&[("first", tone(125.0, 512)), ("second", tone(250.0, 512))]);
        let (_, output) = process(&mut stage, &input);
        let spectrum = spectrum_of(&output);
        assert_eq!(spectrum.signal, "second");
        assert!((spectrum.peak().unwrap().0 - 250.0).abs() < 1e-9);
    }

    #[test]
    fn a_name_that_is_not_in_the_group_is_refused() {
        let mut stage = fft(ParamSet::new().with("signal", "missing"));
        let err = try_process(&mut stage, &frame(&[("rf", tone(125.0, 64))])).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn an_irregular_signal_is_diagnosed_rather_than_guessed_at() {
        let mut stage = fft(ParamSet::new());
        let input = frame_at(
            sp_core::Timebase::irregular(0.0),
            &[("toa", vec![1.0, 2.0])],
        );
        let (_, output) = process(&mut stage, &input);
        assert!(output.artifacts.is_empty());
        assert_eq!(output.diagnostics.len(), 1);
    }

    #[test]
    fn the_peak_metrics_are_what_charts_across_groups() {
        let mut stage = fft(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("rf", tone(125.0, 512))]));
        assert_eq!(output.metrics.get("peak_hz"), Some(&125.0));
        assert!(output.metrics.contains_key("peak_db"));
    }

    #[test]
    fn a_silent_signal_reads_the_floor_rather_than_negative_infinity() {
        let mut stage = fft(ParamSet::new());
        let (_, output) = process(&mut stage, &frame(&[("quiet", vec![0.0; 256])]));
        let spectrum = spectrum_of(&output);
        assert!(spectrum.magnitude_db.iter().all(|db| *db == FLOOR_DB));
    }

    #[test]
    fn the_transform_inverts_to_what_went_in() {
        // The forward transform is the whole risk in this file; running it
        // against its own inverse pins the arithmetic rather than one tone.
        let values = tone(37.0, 64);
        let mut re = values.clone();
        let mut im = vec![0.0; 64];
        fft_in_place(&mut re, &mut im);
        // The inverse is the forward transform of the conjugate, conjugated
        // and scaled.
        for value in &mut im {
            *value = -*value;
        }
        fft_in_place(&mut re, &mut im);
        for (index, expected) in values.iter().enumerate() {
            let back = re[index] / 64.0;
            assert!(
                (back - expected).abs() < 1e-9,
                "{index}: {back} vs {expected}"
            );
        }
    }
}
