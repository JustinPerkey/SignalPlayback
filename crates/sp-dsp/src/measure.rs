//! Measurement stages (`docs/DESIGN.md` §9.8).
//!
//! A measurement stage changes no samples. It emits an artifact for the
//! results screen, metrics for the chart across groups, and — optionally —
//! property write-backs, so a measured value can be searched for in the
//! library like any other property (§6).
//!
//! [`StatisticsStage`] measures the samples; [`PulseMetrics`] measures what a
//! detector found in them. The second reads a `detections.v1` artifact rather
//! than a waveform, which is the point of typed ports (§9.3): pulse width, PRI
//! and duty cycle are properties of the spans, and nothing about computing
//! them needs to know how the spans were found.

use sp_core::artifact::Artifact;
use sp_core::Diagnostic;
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{
    ArtifactOut, PortKind, PortSpec, PropertyPatch, Stage, StageCtx, StageDescriptor, StageOutput,
};
use sp_proc::GroupFrame;

use crate::artifacts::{Detections, Metrics, Statistics};
use crate::SIGNALS_IN;

const PARAMS: &[ParamSpec] = &[ParamSpec::new(
    "write_back",
    "Write back as properties",
    ParamKind::Bool,
    ParamDefault::Bool(false),
)
.with_help("Store each signal's RMS as a property, so it can be searched.")];

const OUTPUTS: &[PortSpec] = &[PortSpec::required(
    "statistics",
    PortKind::Artifact("statistics.v1"),
)];

static STATISTICS: StageDescriptor =
    StageDescriptor::new("dsp.measure.statistics", 1, "Statistics")
        .describing("Min, max, mean and RMS of every signal in the group.")
        .reading(SIGNALS_IN)
        .writing(OUTPUTS)
        .taking(PARAMS);

/// Summarises every signal of a group without touching a sample.
#[derive(Debug, Default)]
pub struct StatisticsStage {
    write_back: bool,
}

impl Stage for StatisticsStage {
    fn descriptor(&self) -> &'static StageDescriptor {
        &STATISTICS
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(PARAMS)?;
        self.write_back = params.bool_or("write_back", false);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        let mut output = StageOutput::passthrough_of(input);
        let mut statistics = Statistics::default();

        for (ordinal, signal) in input.signals.iter().enumerate() {
            ctx.check()?;
            // The handle already carries the summary the store computed, so a
            // group that is only being measured never re-reads its samples.
            let stats = signal.stats();
            let (min, max) = (stats.min().unwrap_or(0.0), stats.max().unwrap_or(0.0));
            let (mean, rms) = (stats.mean().unwrap_or(0.0), stats.rms().unwrap_or(0.0));
            statistics.push(signal.name(), min, max, mean, rms);

            output.metric(format!("{}.rms", signal.name()), rms);
            output.metric(
                format!("{}.peak_to_peak", signal.name()),
                stats.peak_to_peak(),
            );
            if self.write_back {
                output
                    .properties
                    .push(PropertyPatch::signal(ordinal, "rms", rms));
            }
        }

        output
            .artifacts
            .push(ArtifactOut::publish("statistics", &statistics)?);
        Ok(output)
    }
}

const PULSE_PARAMS: &[ParamSpec] = &[ParamSpec::new(
    "write_back",
    "Write back as properties",
    ParamKind::Bool,
    ParamDefault::Bool(false),
)
.with_help("Store the group's PRF and duty cycle as properties, so they can be searched.")];

const PULSE_INPUTS: &[PortSpec] = &[
    PortSpec::required("signals", PortKind::ANY_SIGNALS),
    PortSpec::required("detections", PortKind::Artifact("detections.v1")),
];

const PULSE_OUTPUTS: &[PortSpec] = &[PortSpec::required(
    "metrics",
    PortKind::Artifact("metrics.v1"),
)];

static PULSE: StageDescriptor = StageDescriptor::new("dsp.measure.pulse", 1, "Pulse metrics")
    .describing("Width, PRI, PRF and duty cycle of the detections in a group.")
    .reading(PULSE_INPUTS)
    .writing(PULSE_OUTPUTS)
    .taking(PULSE_PARAMS);

/// Mean and sample standard deviation of a set, or `None` for fewer than two.
fn spread(values: &[f64]) -> (f64, Option<f64>) {
    if values.is_empty() {
        return (0.0, None);
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if values.len() < 2 {
        return (mean, None);
    }
    let variance =
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (values.len() - 1) as f64;
    (mean, Some(variance.sqrt()))
}

/// Turns a group's detections into the numbers a radar engineer names them by.
///
/// The intervals are measured **per signal**: two channels that both fired
/// interleave on the timeline, and a PRI taken across the interleaving would
/// be half the real one.
#[derive(Debug, Default)]
pub struct PulseMetrics {
    write_back: bool,
}

impl PulseMetrics {
    /// Every detection's width, and every gap between successive starts,
    /// gathered per signal.
    fn widths_and_intervals(detections: &Detections) -> (Vec<f64>, Vec<f64>) {
        let widths: Vec<f64> = detections
            .spans
            .iter()
            .map(|(start, end)| end - start)
            .collect();

        let mut intervals = Vec::new();
        let mut names: Vec<&String> = detections.signal.iter().collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let mut starts: Vec<f64> = detections
                .spans
                .iter()
                .zip(&detections.signal)
                .filter(|(_, from)| *from == name)
                .map(|((start, _), _)| *start)
                .collect();
            starts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            intervals.extend(starts.windows(2).map(|pair| pair[1] - pair[0]));
        }
        (widths, intervals)
    }

    /// How long the group covers, which is what a duty cycle is a fraction of.
    fn span_s(input: &GroupFrame) -> f64 {
        input
            .signals
            .iter()
            .filter_map(|signal| signal.timebase().duration_s(signal.sample_count()))
            .fold(0.0, f64::max)
    }

    fn measure(detections: &Detections, span_s: f64) -> Metrics {
        let mut metrics = Metrics::default();
        let (widths, intervals) = Self::widths_and_intervals(detections);
        metrics.push("pulses", detections.len() as f64, "");
        if widths.is_empty() {
            return metrics;
        }

        let (mean_width, width_jitter) = spread(&widths);
        metrics.push("width_mean", mean_width, "s");
        metrics.push(
            "width_min",
            widths.iter().copied().fold(f64::MAX, f64::min),
            "s",
        );
        metrics.push(
            "width_max",
            widths.iter().copied().fold(f64::MIN, f64::max),
            "s",
        );
        if let Some(jitter) = width_jitter {
            metrics.push("width_jitter", jitter, "s");
        }

        let (mean_pri, pri_jitter) = spread(&intervals);
        if !intervals.is_empty() {
            metrics.push("pri_mean", mean_pri, "s");
            // The reciprocal of the mean interval, not the mean of the
            // reciprocals: a PRF is how many pulses a second, and that is what
            // counting them gives.
            metrics.push("prf", 1.0 / mean_pri, "Hz");
            if let Some(jitter) = pri_jitter {
                metrics.push("pri_jitter", jitter, "s");
            }
        }
        if span_s > 0.0 {
            metrics.push("duty", widths.iter().sum::<f64>() / span_s, "");
        }
        metrics.push(
            "peak_max",
            detections.peak.iter().copied().fold(f64::MIN, f64::max),
            "",
        );
        metrics
    }
}

impl Stage for PulseMetrics {
    fn descriptor(&self) -> &'static StageDescriptor {
        &PULSE
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(PULSE_PARAMS)?;
        self.write_back = params.bool_or("write_back", false);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        ctx.check()?;
        let mut output = StageOutput::passthrough_of(input);
        let port = input
            .inbound
            .latest_of_kind(Detections::KIND)
            .ok_or_else(|| StageError::MissingInput {
                port: "detections".to_owned(),
            })?;
        let detections: Detections = port.read()?;

        let metrics = Self::measure(&detections, Self::span_s(input));
        for (name, value) in metrics.name.iter().zip(&metrics.value) {
            output.metric(name.clone(), *value);
        }
        if detections.is_empty() {
            output.diagnose(Diagnostic::info(
                "nothing was detected in this group, so there is nothing to measure",
            ));
        }
        if self.write_back {
            for key in ["prf", "duty"] {
                if let Some(value) = metrics.get(key) {
                    output.properties.push(PropertyPatch::group(
                        if key == "prf" { "prf_hz" } else { key },
                        value,
                    ));
                }
            }
        }
        output
            .artifacts
            .push(ArtifactOut::publish("metrics", &metrics)?);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{frame, process, values_of};

    #[test]
    fn statistics_summarise_every_signal_without_changing_them() {
        let mut stage = StatisticsStage::default();
        stage.configure(&ParamSet::new()).unwrap();
        let input = frame(&[("a", vec![-1.0, 1.0]), ("b", vec![0.0, 4.0])]);
        let (next, output) = process(&mut stage, &input);

        assert_eq!(values_of(&next, 0), [-1.0, 1.0], "samples are untouched");
        let statistics: Statistics =
            serde_json::from_str(&output.artifacts[0].payload_json).unwrap();
        assert_eq!(statistics.names, ["a", "b"]);
        assert_eq!(statistics.min, [-1.0, 0.0]);
        assert_eq!(statistics.max, [1.0, 4.0]);
        assert_eq!(statistics.mean, [0.0, 2.0]);
    }

    #[test]
    fn the_artifact_carries_its_kind_and_summary() {
        let mut stage = StatisticsStage::default();
        stage.configure(&ParamSet::new()).unwrap();
        let (_, output) = process(&mut stage, &frame(&[("a", vec![-1.0, 1.0])]));
        let artifact = &output.artifacts[0];
        assert_eq!(artifact.kind, "statistics.v1");
        assert_eq!(artifact.port, "statistics");
        assert_eq!(artifact.summary.as_deref(), Some("a: rms 1.000"));
    }

    #[test]
    fn metrics_are_named_per_signal_so_they_chart_across_groups() {
        let mut stage = StatisticsStage::default();
        stage.configure(&ParamSet::new()).unwrap();
        let (_, output) = process(&mut stage, &frame(&[("rf", vec![3.0, -3.0])]));
        assert_eq!(output.metrics.get("rf.rms"), Some(&3.0));
        assert_eq!(output.metrics.get("rf.peak_to_peak"), Some(&6.0));
    }

    #[test]
    fn write_back_puts_the_measurement_on_the_signal() {
        let mut stage = StatisticsStage::default();
        stage
            .configure(&ParamSet::new().with("write_back", true))
            .unwrap();
        let (next, output) = process(&mut stage, &frame(&[("rf", vec![3.0, -3.0])]));
        assert_eq!(output.properties.len(), 1);
        assert_eq!(next.signals[0].attributes().get_f64("rms"), Some(3.0));
    }

    #[test]
    fn without_write_back_nothing_is_written_onto_the_signal() {
        let mut stage = StatisticsStage::default();
        stage.configure(&ParamSet::new()).unwrap();
        let (next, output) = process(&mut stage, &frame(&[("rf", vec![1.0])]));
        assert!(output.properties.is_empty());
        assert!(next.signals[0].attributes().get("rms").is_none());
    }
}

#[cfg(test)]
mod pulse_tests {
    use sp_proc::frame::PortValue;

    use super::*;
    use crate::tests::{frame, process, try_process, values_of};

    /// A group 0.4 s long, with the detections an upstream threshold would
    /// have published on its port.
    fn group_with(detections: &Detections) -> GroupFrame {
        let mut input = frame(&[("rf", vec![0.0; 400])]);
        input.inbound.publish(PortValue {
            port: "detections".to_owned(),
            kind: Detections::KIND.to_owned(),
            kind_version: Detections::VERSION,
            payload_json: serde_json::to_string(detections).unwrap(),
            summary: Some(detections.summary()),
            stage_ordinal: 0,
        });
        input
    }

    fn pulses(signal: &str, starts: &[f64], width: f64) -> Detections {
        let mut detections = Detections::default();
        for &start in starts {
            detections.push(signal, (start, start + width), 1.0);
        }
        detections
    }

    fn measured(output: &StageOutput) -> Metrics {
        serde_json::from_str(&output.artifacts[0].payload_json).unwrap()
    }

    fn close(value: Option<f64>, expected: f64) -> bool {
        value.is_some_and(|value| (value - expected).abs() < 1e-9)
    }

    fn stage(params: ParamSet) -> PulseMetrics {
        let mut stage = PulseMetrics::default();
        stage.configure(&params).unwrap();
        stage
    }

    #[test]
    fn a_pulse_train_is_reported_by_width_interval_rate_and_duty() {
        let input = group_with(&pulses("rf", &[0.0, 0.1, 0.2, 0.3], 0.02));
        let (next, output) = process(&mut stage(ParamSet::new()), &input);
        let metrics = measured(&output);

        assert_eq!(metrics.get("pulses"), Some(4.0));
        assert!(close(metrics.get("width_mean"), 0.02));
        assert!(close(metrics.get("pri_mean"), 0.1));
        assert!(close(metrics.get("prf"), 10.0));
        // 4 × 20 ms of pulse in 400 ms of group.
        assert!(close(metrics.get("duty"), 0.2));
        assert_eq!(metrics.get("peak_max"), Some(1.0));
        assert_eq!(values_of(&next, 0).len(), 400, "samples are untouched");
    }

    #[test]
    fn the_interval_is_measured_per_signal_rather_than_across_the_interleaving() {
        // Two channels firing alternately every 200 ms look like one train at
        // 100 ms if the spans are read as one list. They are not one train.
        let mut detections = pulses("a", &[0.0, 0.2], 0.01);
        for span in pulses("b", &[0.1, 0.3], 0.01).spans {
            detections.push("b", span, 1.0);
        }
        let (_, output) = process(&mut stage(ParamSet::new()), &group_with(&detections));
        let metrics = measured(&output);
        assert_eq!(metrics.get("pulses"), Some(4.0));
        assert!(close(metrics.get("pri_mean"), 0.2), "{:?}", metrics.value);
        assert!(close(metrics.get("prf"), 5.0));
    }

    #[test]
    fn jitter_is_reported_only_once_there_is_something_to_compare() {
        let steady = pulses("rf", &[0.0, 0.1, 0.2], 0.02);
        let (_, output) = process(&mut stage(ParamSet::new()), &group_with(&steady));
        assert!(close(measured(&output).get("pri_jitter"), 0.0));

        let one = pulses("rf", &[0.05], 0.02);
        let (_, single) = process(&mut stage(ParamSet::new()), &group_with(&one));
        let metrics = measured(&single);
        assert_eq!(metrics.get("width_jitter"), None, "one width has no spread");
        assert_eq!(metrics.get("pri_mean"), None, "and no interval at all");
        assert!(close(metrics.get("width_mean"), 0.02));
    }

    #[test]
    fn a_wandering_train_reports_the_wander() {
        let wandering = pulses("rf", &[0.0, 0.1, 0.25], 0.02);
        let (_, output) = process(&mut stage(ParamSet::new()), &group_with(&wandering));
        let jitter = measured(&output).get("pri_jitter").unwrap();
        assert!(jitter > 0.03, "{jitter}");
    }

    #[test]
    fn a_group_with_nothing_detected_says_so_and_still_publishes_a_table() {
        let (_, output) = process(
            &mut stage(ParamSet::new()),
            &group_with(&Detections::default()),
        );
        let metrics = measured(&output);
        assert_eq!(metrics.get("pulses"), Some(0.0));
        assert_eq!(metrics.get("prf"), None, "there is no rate to report");
        assert_eq!(output.diagnostics.len(), 1);
    }

    #[test]
    fn every_measurement_is_also_a_metric_so_it_charts_across_groups() {
        let input = group_with(&pulses("rf", &[0.0, 0.1], 0.02));
        let (_, output) = process(&mut stage(ParamSet::new()), &input);
        assert!(close(output.metrics.get("prf").copied(), 10.0));
        assert!(close(output.metrics.get("width_mean").copied(), 0.02));
        assert_eq!(output.metrics.get("pulses"), Some(&2.0));
    }

    #[test]
    fn write_back_puts_the_rate_and_the_duty_on_the_group() {
        let input = group_with(&pulses("rf", &[0.0, 0.1, 0.2], 0.02));
        let (next, output) = process(&mut stage(ParamSet::new().with("write_back", true)), &input);
        assert_eq!(output.properties.len(), 2);
        assert!(close(next.group.attributes.get_f64("prf_hz"), 10.0));
        assert!(close(next.group.attributes.get_f64("duty"), 0.15));
    }

    #[test]
    fn without_a_detector_upstream_the_group_fails_by_the_name_of_the_port() {
        let mut stage = stage(ParamSet::new());
        let error = try_process(&mut stage, &frame(&[("rf", vec![0.0; 10])])).unwrap_err();
        assert!(matches!(
            error,
            StageError::MissingInput { ref port } if port == "detections"
        ));
    }

    #[test]
    fn the_artifact_carries_a_unit_per_row_because_the_rows_disagree() {
        let input = group_with(&pulses("rf", &[0.0, 0.1], 0.02));
        let (_, output) = process(&mut stage(ParamSet::new()), &input);
        let metrics = measured(&output);
        let unit_of = |name: &str| {
            let at = metrics.name.iter().position(|n| n == name).unwrap();
            metrics.unit[at].clone()
        };
        assert_eq!(unit_of("width_mean"), "s");
        assert_eq!(unit_of("prf"), "Hz");
        assert_eq!(unit_of("duty"), "");
        assert_eq!(output.artifacts[0].kind, "metrics.v1");
        assert_eq!(output.artifacts[0].port, "metrics");
    }
}
