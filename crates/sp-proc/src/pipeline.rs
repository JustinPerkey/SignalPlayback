//! The pipeline model and its edit-time validation (`docs/DESIGN.md` §9.3).
//!
//! A pipeline is an ordered list of stages. Validation answers one question —
//! *would this run?* — before a single sample is read, and answers it for
//! every stage at once so the editor can flag them all in place rather than
//! one per attempt.

use sp_core::run::Retention;
use sp_store::regress::AssertionRow;
use sp_store::runs::PipelineStageRow;

use crate::assert::{AssertError, Assertion};
use crate::error::{ConfigError, ProcError};
use crate::param::ParamSet;
use crate::registry::StageRegistry;
use crate::stage::{PortKind, StageDescriptor};

/// One stage instance in a pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineStage {
    /// The registered kind, e.g. `dsp.filter.biquad`.
    pub kind: String,
    /// The user's name for this instance, when they gave it one.
    pub label: Option<String>,
    pub params: ParamSet,
    pub enabled: bool,
    pub retention: Retention,
}

impl PipelineStage {
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
            params: ParamSet::new(),
            enabled: true,
            retention: Retention::Always,
        }
    }

    #[must_use]
    pub fn with_params(mut self, params: ParamSet) -> Self {
        self.params = params;
        self
    }

    #[must_use]
    pub fn with_param(
        mut self,
        name: impl Into<String>,
        value: impl Into<sp_core::PropertyValue>,
    ) -> Self {
        self.params.set(name, value);
        self
    }

    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// What the stage rail shows: the user's label, else the stage's own.
    #[must_use]
    pub fn display_name(&self, registry: &StageRegistry) -> String {
        self.label.clone().unwrap_or_else(|| {
            registry
                .descriptor(&self.kind)
                .map_or_else(|| self.kind.clone(), |d| d.label.to_owned())
        })
    }
}

/// An ordered list of stages, plus the assertions that decide whether a run
/// of them passed (§9.7).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pipeline {
    pub name: String,
    pub stages: Vec<PipelineStage>,
    /// Evaluated per group after the last stage. A pipeline with none is not
    /// a test — it still runs, and every group passes by having nothing to
    /// fail.
    pub assertions: Vec<PipelineAssertion>,
}

/// One assertion of a pipeline: the text the user wrote, and what it parsed
/// to when it parsed.
///
/// The text is kept even when it does not parse, so an editor can hold a
/// half-typed line without losing it — and a run refuses to start rather than
/// silently skipping it.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineAssertion {
    pub source: String,
    pub enabled: bool,
    parsed: Result<Assertion, AssertError>,
}

impl PipelineAssertion {
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        let source = source.into();
        let parsed = Assertion::parse(&source);
        Self {
            source,
            enabled: true,
            parsed,
        }
    }

    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// The parsed assertion, or why it would not parse.
    pub fn parsed(&self) -> Result<&Assertion, &AssertError> {
        self.parsed.as_ref()
    }

    #[must_use]
    pub fn error(&self) -> Option<&AssertError> {
        self.parsed.as_ref().err()
    }

    /// Whether this assertion has nothing to say without a baseline.
    #[must_use]
    pub fn needs_baseline(&self) -> bool {
        self.parsed.as_ref().is_ok_and(Assertion::needs_baseline)
    }
}

/// Something that would stop a pipeline running, pinned to the stage it is
/// about so the editor can flag it in place.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PipelineIssue {
    #[error("stage {ordinal}: no stage is registered as '{kind}'")]
    UnknownKind { ordinal: usize, kind: String },

    #[error("stage {ordinal} ('{label}'): {source}")]
    BadParams {
        ordinal: usize,
        label: String,
        #[source]
        source: ConfigError,
    },

    /// An assertion that does not parse. A run refuses rather than skipping
    /// it: a test suite that quietly drops a test is worse than one that will
    /// not start.
    #[error("assertion {ordinal} ('{text}'): {reason}")]
    BadAssertion {
        ordinal: usize,
        /// The line as written — named `text` rather than `source` so
        /// `thiserror` does not read it as the underlying error.
        text: String,
        #[source]
        reason: AssertError,
    },

    /// The heart of §9.3: a required input nothing upstream produces, named.
    #[error(
        "stage {ordinal} ('{label}') needs '{port}' ({expects}), which nothing upstream produces"
    )]
    UnsatisfiedPort {
        ordinal: usize,
        label: String,
        port: String,
        expects: String,
    },
}

impl PipelineIssue {
    /// The stage this is about, for highlighting it.
    #[must_use]
    pub fn ordinal(&self) -> usize {
        match self {
            Self::UnknownKind { ordinal, .. }
            | Self::BadParams { ordinal, .. }
            | Self::BadAssertion { ordinal, .. }
            | Self::UnsatisfiedPort { ordinal, .. } => *ordinal,
        }
    }
}

/// Joins issues into one line, for the error a run refuses with.
pub(crate) fn describe_issues(issues: &[PipelineIssue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl Pipeline {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            stages: Vec::new(),
            assertions: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_stage(mut self, stage: PipelineStage) -> Self {
        self.stages.push(stage);
        self
    }

    #[must_use]
    pub fn asserting(mut self, source: impl Into<String>) -> Self {
        self.assertions.push(PipelineAssertion::new(source));
        self
    }

    /// The assertions that would actually be evaluated, with their positions
    /// — the position is the ordinal a run records them under, so disabling
    /// one does not renumber the rest.
    pub fn enabled_assertions(&self) -> impl Iterator<Item = (usize, &PipelineAssertion)> {
        self.assertions
            .iter()
            .enumerate()
            .filter(|(_, assertion)| assertion.enabled)
    }

    /// The stages that would actually run, with their positions in the full
    /// list — the position is what a run records, so a disabled stage does not
    /// renumber the ones after it.
    pub fn enabled(&self) -> impl Iterator<Item = (usize, &PipelineStage)> {
        self.stages
            .iter()
            .enumerate()
            .filter(|(_, stage)| stage.enabled)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enabled().count() == 0
    }

    /// Everything wrong with this pipeline, in stage order. Empty means it
    /// would run.
    #[must_use]
    pub fn issues(&self, registry: &StageRegistry) -> Vec<PipelineIssue> {
        let mut issues = Vec::new();
        // What is available to a stage's inputs: the group's own signals are
        // always there, and each stage adds its declared output ports.
        let mut produced: Vec<PortKind> = vec![PortKind::ANY_SIGNALS];

        for (ordinal, stage) in self.enabled() {
            let label = stage.display_name(registry);
            let Some(descriptor) = registry.descriptor(&stage.kind) else {
                issues.push(PipelineIssue::UnknownKind {
                    ordinal,
                    kind: stage.kind.clone(),
                });
                continue;
            };

            if let Err(source) = stage.params.validate(descriptor.params) {
                issues.push(PipelineIssue::BadParams {
                    ordinal,
                    label: label.clone(),
                    source,
                });
            }

            for port in descriptor.inputs.iter().filter(|p| p.required) {
                if !produced.iter().any(|have| port.kind.accepts(*have)) {
                    issues.push(PipelineIssue::UnsatisfiedPort {
                        ordinal,
                        label: label.clone(),
                        port: port.name.to_owned(),
                        expects: port.kind.describe(),
                    });
                }
            }

            produced.extend(descriptor.outputs.iter().map(|port| port.kind));
        }

        for (ordinal, assertion) in self.enabled_assertions() {
            if let Err(reason) = assertion.parsed() {
                issues.push(PipelineIssue::BadAssertion {
                    ordinal,
                    text: assertion.source.clone(),
                    reason: reason.clone(),
                });
            }
        }

        issues
    }

    /// `Ok` when the pipeline would run.
    pub fn validate(&self, registry: &StageRegistry) -> Result<(), ProcError> {
        if self.is_empty() {
            return Err(ProcError::Empty);
        }
        let issues = self.issues(registry);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ProcError::Invalid(issues))
        }
    }

    /// The canonical hash of what this pipeline computes: kinds, versions and
    /// resolved parameters of the enabled stages, in order (§9.6).
    ///
    /// Renaming a stage, changing its retention or editing an assertion does
    /// not change the hash — none of them changes a single sample, and an
    /// assertion is a question asked of the result rather than part of it.
    pub fn hash(&self, registry: &StageRegistry) -> Result<String, ProcError> {
        let mut hasher = blake3::Hasher::new();
        for (ordinal, stage) in self.enabled() {
            let descriptor = registry
                .descriptor(&stage.kind)
                .ok_or_else(|| ProcError::UnknownStage(stage.kind.clone()))?;
            let params = resolve(stage, descriptor, ordinal)?;
            hasher.update(descriptor.kind.as_bytes());
            hasher.update(&descriptor.version.to_le_bytes());
            hasher.update(params.to_json().as_bytes());
            hasher.update(b"\x1e");
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// The rows that save this pipeline's stages.
    #[must_use]
    pub fn to_rows(&self) -> Vec<PipelineStageRow> {
        self.stages
            .iter()
            .enumerate()
            .map(|(ordinal, stage)| {
                let mut row = PipelineStageRow::new(ordinal as u32, stage.kind.clone())
                    .with_params_json(stage.params.to_json())
                    .with_retention(stage.retention);
                row.label.clone_from(&stage.label);
                row.enabled = stage.enabled;
                row
            })
            .collect()
    }

    /// The rows that save this pipeline's assertions.
    #[must_use]
    pub fn to_assertion_rows(&self) -> Vec<AssertionRow> {
        self.assertions
            .iter()
            .enumerate()
            .map(|(ordinal, assertion)| {
                let mut row = AssertionRow::new(ordinal as u32, assertion.source.clone());
                row.enabled = assertion.enabled;
                row
            })
            .collect()
    }

    /// Rebuilds a pipeline from its saved rows. Rows are taken in the order
    /// given; the store returns them by ordinal.
    pub fn from_rows(
        name: impl Into<String>,
        rows: &[PipelineStageRow],
    ) -> Result<Self, serde_json::Error> {
        let mut pipeline = Self::new(name);
        for row in rows {
            pipeline.stages.push(PipelineStage {
                kind: row.stage_kind.clone(),
                label: row.label.clone(),
                params: ParamSet::from_json(&row.params_json)?,
                enabled: row.enabled,
                retention: row.retention,
            });
        }
        Ok(pipeline)
    }

    /// Adds the saved assertions to a pipeline rebuilt by [`Self::from_rows`].
    #[must_use]
    pub fn with_assertion_rows(mut self, rows: &[AssertionRow]) -> Self {
        self.assertions = rows
            .iter()
            .map(|row| {
                let mut assertion = PipelineAssertion::new(row.expression.clone());
                assertion.enabled = row.enabled;
                assertion
            })
            .collect();
        self
    }
}

/// A stage's parameters with defaults filled in, reported against its
/// position.
pub(crate) fn resolve(
    stage: &PipelineStage,
    descriptor: &StageDescriptor,
    ordinal: usize,
) -> Result<ParamSet, ProcError> {
    stage.params.resolved(descriptor.params).map_err(|source| {
        ProcError::Invalid(vec![PipelineIssue::BadParams {
            ordinal,
            label: stage.label.clone().unwrap_or_else(|| stage.kind.clone()),
            source,
        }])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StageError;
    use crate::frame::GroupFrame;
    use crate::param::{ParamDefault, ParamKind, ParamSpec};
    use crate::stage::{PortSpec, Stage, StageCtx, StageOutput};

    // A pair of stages: one that emits a spectrum, one that needs one.
    #[derive(Debug, Default)]
    struct Fft;

    const FFT_OUTPUTS: &[PortSpec] = &[PortSpec::required(
        "spectrum",
        PortKind::Artifact("spectrum.v1"),
    )];
    const FFT_PARAMS: &[ParamSpec] = &[ParamSpec::new(
        "size",
        "FFT size",
        ParamKind::Int {
            min: Some(2),
            max: Some(65536),
        },
        ParamDefault::Int(1024),
    )];
    static FFT: StageDescriptor = StageDescriptor::new("test.fft", 2, "FFT")
        .writing(FFT_OUTPUTS)
        .taking(FFT_PARAMS);

    impl Stage for Fft {
        fn descriptor(&self) -> &'static StageDescriptor {
            &FFT
        }
        fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
            Ok(())
        }
        fn process(
            &mut self,
            _ctx: &StageCtx,
            input: &GroupFrame,
        ) -> Result<StageOutput, StageError> {
            Ok(StageOutput::passthrough_of(input))
        }
    }

    #[derive(Debug, Default)]
    struct PeakPick;

    const PEAK_INPUTS: &[PortSpec] = &[
        PortSpec::required("signals", PortKind::ANY_SIGNALS),
        PortSpec::required("spectrum", PortKind::Artifact("spectrum.v1")),
    ];
    static PEAKS: StageDescriptor =
        StageDescriptor::new("test.peaks", 1, "Peak pick").reading(PEAK_INPUTS);

    impl Stage for PeakPick {
        fn descriptor(&self) -> &'static StageDescriptor {
            &PEAKS
        }
        fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
            Ok(())
        }
        fn process(
            &mut self,
            _ctx: &StageCtx,
            input: &GroupFrame,
        ) -> Result<StageOutput, StageError> {
            Ok(StageOutput::passthrough_of(input))
        }
    }

    fn registry() -> StageRegistry {
        let mut registry = StageRegistry::new();
        registry
            .register_all([
                (|| Box::<Fft>::default()) as crate::registry::StageFactory,
                || Box::<PeakPick>::default(),
            ])
            .unwrap();
        registry
    }

    #[test]
    fn a_stage_whose_input_nothing_produces_is_flagged_in_place() {
        let pipeline = Pipeline::new("bad").with_stage(PipelineStage::new("test.peaks"));
        let issues = pipeline.issues(&registry());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].ordinal(), 0);
        assert_eq!(
            issues[0].to_string(),
            "stage 0 ('Peak pick') needs 'spectrum' (spectrum.v1), which nothing upstream produces"
        );
    }

    #[test]
    fn an_upstream_producer_satisfies_it() {
        let pipeline = Pipeline::new("good")
            .with_stage(PipelineStage::new("test.fft"))
            .with_stage(PipelineStage::new("test.peaks"));
        assert!(pipeline.issues(&registry()).is_empty());
        pipeline.validate(&registry()).unwrap();
    }

    #[test]
    fn order_matters_a_producer_after_its_consumer_does_not_count() {
        let pipeline = Pipeline::new("backwards")
            .with_stage(PipelineStage::new("test.peaks"))
            .with_stage(PipelineStage::new("test.fft"));
        assert_eq!(pipeline.issues(&registry()).len(), 1);
    }

    #[test]
    fn disabling_the_producer_breaks_the_consumer() {
        let pipeline = Pipeline::new("half off")
            .with_stage(PipelineStage::new("test.fft").disabled())
            .with_stage(PipelineStage::new("test.peaks"));
        let issues = pipeline.issues(&registry());
        assert!(matches!(
            issues.as_slice(),
            [PipelineIssue::UnsatisfiedPort { ordinal: 1, .. }]
        ));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let pipeline = Pipeline::new("messy")
            .with_stage(PipelineStage::new("test.fft").with_param("size", 1))
            .with_stage(PipelineStage::new("test.absent"))
            .with_stage(PipelineStage::new("test.peaks").with_param("nope", 1.0));
        let issues = pipeline.issues(&registry());
        assert_eq!(issues.len(), 3, "{issues:?}");
        assert!(matches!(
            issues[0],
            PipelineIssue::BadParams { ordinal: 0, .. }
        ));
        assert!(matches!(
            issues[1],
            PipelineIssue::UnknownKind { ordinal: 1, .. }
        ));
        assert!(matches!(
            issues[2],
            PipelineIssue::BadParams { ordinal: 2, .. }
        ));
    }

    #[test]
    fn an_empty_pipeline_is_refused_by_name() {
        let pipeline = Pipeline::new("nothing on");
        assert!(matches!(
            pipeline.validate(&registry()).unwrap_err(),
            ProcError::Empty
        ));
    }

    #[test]
    fn the_hash_follows_the_algorithm_not_the_labelling() {
        let registry = registry();
        let plain = Pipeline::new("a").with_stage(PipelineStage::new("test.fft"));
        let relabelled = Pipeline::new("b").with_stage(
            PipelineStage::new("test.fft")
                .labelled("Coarse FFT")
                .with_retention(Retention::Never),
        );
        assert_eq!(
            plain.hash(&registry).unwrap(),
            relabelled.hash(&registry).unwrap(),
            "a label and a retention policy change no samples"
        );

        let retuned =
            Pipeline::new("c").with_stage(PipelineStage::new("test.fft").with_param("size", 512));
        assert_ne!(
            plain.hash(&registry).unwrap(),
            retuned.hash(&registry).unwrap()
        );
    }

    #[test]
    fn setting_a_parameter_to_its_default_leaves_the_hash_alone() {
        let registry = registry();
        let implicit = Pipeline::new("a").with_stage(PipelineStage::new("test.fft"));
        let explicit =
            Pipeline::new("a").with_stage(PipelineStage::new("test.fft").with_param("size", 1024));
        assert_eq!(
            implicit.hash(&registry).unwrap(),
            explicit.hash(&registry).unwrap()
        );
    }

    #[test]
    fn a_pipeline_round_trips_through_its_saved_rows() {
        let pipeline = Pipeline::new("saved")
            .with_stage(
                PipelineStage::new("test.fft")
                    .with_param("size", 256)
                    .labelled("Fine")
                    .with_retention(Retention::OnFailure),
            )
            .with_stage(PipelineStage::new("test.peaks").disabled());

        let rows = pipeline.to_rows();
        assert_eq!(rows[0].ordinal, 0);
        assert_eq!(rows[1].ordinal, 1, "a disabled stage keeps its position");

        let back = Pipeline::from_rows("saved", &rows).unwrap();
        assert_eq!(back, pipeline);
    }

    #[test]
    fn a_disabled_stage_does_not_renumber_the_ones_after_it() {
        let pipeline = Pipeline::new("gapped")
            .with_stage(PipelineStage::new("test.fft").disabled())
            .with_stage(PipelineStage::new("test.fft"));
        let positions: Vec<_> = pipeline.enabled().map(|(ordinal, _)| ordinal).collect();
        assert_eq!(positions, [1]);
    }
}
