//! Measurement stages (`docs/DESIGN.md` §9.8).
//!
//! A measurement stage changes no samples. It emits an artifact for the
//! results screen, metrics for the chart across groups, and — optionally —
//! property write-backs, so a measured value can be searched for in the
//! library like any other property (§6).

use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{
    ArtifactOut, PortKind, PortSpec, PropertyPatch, Stage, StageCtx, StageDescriptor, StageOutput,
};
use sp_proc::GroupFrame;

use crate::artifacts::Statistics;
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
