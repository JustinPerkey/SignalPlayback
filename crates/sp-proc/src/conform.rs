//! The stage conformance harness (`docs/DESIGN.md` §14, §16.1 M11).
//!
//! G9 says a user's algorithm is a first-class stage. That promise is only
//! worth something if a stage nobody here wrote can be held to the same
//! contract the built-ins are held to — so the contract is written down once,
//! as a test any [`Stage`] implementation can be run through:
//!
//! - its descriptor is coherent, so the editor can draw it,
//! - `configure` accepts what it declares and refuses what it does not,
//! - every input signal is accounted for and every required output port
//!   carries something,
//! - the same input twice gives the same output, through the same instance and
//!   through a fresh one — which is what the cache key assumes of a pure stage,
//! - a cancelled run stops it rather than being finished through,
//! - and an empty, single-sample, all-NaN or DC-only group produces a
//!   diagnostic rather than a panic.
//!
//! The harness supplies its own degenerate groups, so the cost of conforming is
//! a handful of lines:
//!
//! ```ignore
//! Conformance::of(|| Box::<Threshold>::default())
//!     .with_params(ParamSet::new().with("level", 0.5))
//!     .run()
//!     .assert_pass();
//! ```
//!
//! A finding is a statement about the stage, not about the harness: every one
//! names the check, the group it happened on and what was expected.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use sp_core::{
    Attributes, DType, Domain, GroupId, GroupMeta, RunId, SampleBuffer, Timebase, TrainId,
};

use crate::error::StageError;
use crate::frame::{GroupFrame, PortValue, SignalRef};
use crate::param::ParamSet;
use crate::registry::{DynStageFactory, StageFactory};
use crate::stage::{PortKind, RunCtx, SignalOut, Stage, StageCtx, StageDescriptor, StageOutput};

/// A parameter name no stage can have declared, used to check that unknown
/// parameters are refused rather than ignored.
const NOT_A_PARAMETER: &str = "__conformance_not_a_parameter";

/// One question the harness asks of a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Check {
    /// A namespaced kind, unique port and parameter names, and the label and
    /// summary the palette draws (§9.2).
    Declaration,
    /// `configure` takes the declared parameters and refuses an undeclared
    /// one, so a typo costs an error rather than a silently ignored setting.
    Configuration,
    /// Every input signal is accounted for exactly once and the frame the next
    /// stage sees can be built from the output (§9.4).
    Contract,
    /// Every required output port carries something.
    Ports,
    /// The same input gives the same output, through this instance and through
    /// a fresh one. Only asked of a stage that declares itself pure (§9.5).
    Determinism,
    /// A cancelled run stops the stage at a chunk boundary rather than being
    /// finished through (§4.2).
    Cancellation,
    /// No panic on a degenerate group: empty, one sample, all-NaN, DC-only
    /// (§14).
    Survival,
}

impl Check {
    /// Every check, in the order the harness runs them.
    pub const ALL: [Self; 7] = [
        Self::Declaration,
        Self::Configuration,
        Self::Contract,
        Self::Ports,
        Self::Determinism,
        Self::Cancellation,
        Self::Survival,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Declaration => "declaration",
            Self::Configuration => "configuration",
            Self::Contract => "contract",
            Self::Ports => "ports",
            Self::Determinism => "determinism",
            Self::Cancellation => "cancellation",
            Self::Survival => "survival",
        }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One way a stage failed to conform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: Check,
    /// The group it happened on, or `""` for a check that is not about one.
    pub case: String,
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.case.is_empty() {
            write!(f, "{}: {}", self.check, self.detail)
        } else {
            write!(f, "{} on '{}': {}", self.check, self.case, self.detail)
        }
    }
}

/// What the harness found.
#[derive(Debug, Clone)]
pub struct Report {
    kind: String,
    cases: Vec<String>,
    skipped: Vec<Check>,
    findings: Vec<Finding>,
}

impl Report {
    /// The stage conforms.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.findings.is_empty()
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// The groups the stage was run over.
    #[must_use]
    pub fn cases(&self) -> &[String] {
        &self.cases
    }

    /// Checks the caller waived, which a report should say out loud: a waived
    /// check is a promise nobody is holding the stage to.
    #[must_use]
    pub fn skipped(&self) -> &[Check] {
        &self.skipped
    }

    /// One line per finding, with a heading that says what was run.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} · {} case(s) · {} check(s)",
            self.kind,
            self.cases.len(),
            Check::ALL.len() - self.skipped.len()
        );
        if !self.skipped.is_empty() {
            let waived: Vec<&str> = self.skipped.iter().map(|c| c.label()).collect();
            text.push_str(&format!(" · waived: {}", waived.join(", ")));
        }
        if self.findings.is_empty() {
            text.push_str("\nconforms");
            return text;
        }
        for finding in &self.findings {
            text.push_str(&format!("\n  {finding}"));
        }
        text
    }

    /// Panics with the report unless the stage conforms — the shape a test
    /// wants, since the failure message is the whole point.
    #[track_caller]
    pub fn assert_pass(&self) {
        assert!(self.is_pass(), "{}", self.describe());
    }
}

/// One group the harness runs the stage over.
#[derive(Debug, Clone)]
struct Case {
    name: String,
    frame: GroupFrame,
    /// Whether this group is substantial enough for a cancellation to be
    /// noticed; an empty group has no chunk boundary to check at.
    ordinary: bool,
}

/// Runs a stage through the contract every stage is held to.
#[derive(Clone)]
pub struct Conformance {
    factory: DynStageFactory,
    params: ParamSet,
    cases: Vec<Case>,
    inbound: Vec<PortValue>,
    skipped: Vec<Check>,
}

impl fmt::Debug for Conformance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conformance")
            .field("kind", &(self.factory)().descriptor().kind)
            .field("params", &self.params)
            .field("cases", &self.cases.len())
            .field("skipped", &self.skipped)
            .finish()
    }
}

impl Conformance {
    /// A harness over the standard degenerate groups, which is what most
    /// stages need and all of them should survive.
    #[must_use]
    pub fn of(factory: StageFactory) -> Self {
        Self::from_fn(factory)
    }

    /// The same, for a stage whose constructor carries state — an external
    /// library holds its own handle, and a stage that cannot be built from a
    /// bare `fn` is exactly the one worth putting through this (§9.9, G9).
    #[must_use]
    pub fn from_fn(factory: impl Fn() -> Box<dyn Stage> + Send + Sync + 'static) -> Self {
        Self {
            factory: Arc::new(factory),
            params: ParamSet::new(),
            cases: standard_cases(),
            inbound: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// The parameters to configure the stage with. A stage with a required
    /// parameter needs them; the rest run on their declared defaults.
    #[must_use]
    pub fn with_params(mut self, params: ParamSet) -> Self {
        self.params = params;
        self
    }

    /// An extra group to run over — a realistic one for the algorithm, which
    /// is where determinism is worth checking.
    #[must_use]
    pub fn over(mut self, name: impl Into<String>, frame: GroupFrame) -> Self {
        self.cases.push(Case {
            name: name.into(),
            frame,
            ordinary: true,
        });
        self
    }

    /// An artifact on an inbound port, for a stage that reads one (§9.3).
    /// Published onto every case, since a missing required input would
    /// otherwise fail every group for the same reason.
    #[must_use]
    pub fn with_inbound(mut self, value: PortValue) -> Self {
        self.inbound.push(value);
        self
    }

    /// Waives a check. A stage that does no per-sample work has no chunk
    /// boundary to notice a cancellation at, and says so here rather than
    /// failing for it.
    #[must_use]
    pub fn skipping(mut self, check: Check) -> Self {
        if !self.skipped.contains(&check) {
            self.skipped.push(check);
            self.skipped.sort_unstable();
        }
        self
    }

    fn runs(&self, check: Check) -> bool {
        !self.skipped.contains(&check)
    }

    fn frames(&self) -> Vec<Case> {
        self.cases
            .iter()
            .map(|case| {
                let mut case = case.clone();
                for value in &self.inbound {
                    case.frame.inbound.publish(value.clone());
                }
                case
            })
            .collect()
    }

    /// Builds a configured instance, or the finding that says why it could
    /// not be built.
    fn configured(&self) -> Result<Box<dyn Stage>, Finding> {
        let mut stage = (self.factory)();
        stage.configure(&self.params).map_err(|error| Finding {
            check: Check::Configuration,
            case: String::new(),
            detail: format!("configure rejected the parameters under test: {error}"),
        })?;
        if let Err(error) = stage.begin_run(&run_ctx(None)) {
            return Err(Finding {
                check: Check::Configuration,
                case: String::new(),
                detail: format!("begin_run failed: {error}"),
            });
        }
        Ok(stage)
    }

    /// Runs every check that has not been waived.
    #[must_use]
    pub fn run(&self) -> Report {
        let descriptor = (self.factory)().descriptor();
        let cases = self.frames();
        let mut report = Report {
            kind: descriptor.kind.to_owned(),
            cases: cases.iter().map(|case| case.name.clone()).collect(),
            skipped: self.skipped.clone(),
            findings: Vec::new(),
        };

        if self.runs(Check::Declaration) {
            report.findings.extend(declaration_findings(descriptor));
        }
        if self.runs(Check::Configuration) {
            report.findings.extend(self.configuration_findings());
        }

        let mut stage = match self.configured() {
            Ok(stage) => stage,
            Err(finding) => {
                // Nothing downstream can run without a configured stage, and
                // reporting the same cause once per group would bury it.
                report.findings.push(finding);
                return report;
            }
        };

        for case in &cases {
            report
                .findings
                .extend(self.case_findings(&mut *stage, case));
        }
        if self.runs(Check::Cancellation) {
            report.findings.extend(self.cancellation_findings(&cases));
        }

        report.findings.sort_by(|a, b| {
            (a.check, a.case.as_str(), a.detail.as_str()).cmp(&(
                b.check,
                b.case.as_str(),
                b.detail.as_str(),
            ))
        });
        report
    }

    fn configuration_findings(&self) -> Vec<Finding> {
        let descriptor = (self.factory)().descriptor();
        let mut findings = Vec::new();

        if let Err(error) = self.params.validate(descriptor.params) {
            findings.push(Finding {
                check: Check::Configuration,
                case: String::new(),
                detail: format!("the parameters under test do not satisfy the descriptor: {error}"),
            });
            return findings;
        }

        // A stage that ignores what it was not declared to take gives no sign
        // that a mistyped parameter did nothing.
        let mut stage = (self.factory)();
        let undeclared = self.params.clone().with(NOT_A_PARAMETER, 1.0);
        if stage.configure(&undeclared).is_ok() {
            findings.push(Finding {
                check: Check::Configuration,
                case: String::new(),
                detail: format!(
                    "configure accepted '{NOT_A_PARAMETER}', which the descriptor does not declare"
                ),
            });
        }
        findings
    }

    /// Everything asked of one group.
    fn case_findings(&self, stage: &mut dyn Stage, case: &Case) -> Vec<Finding> {
        let descriptor = stage.descriptor();
        let ctx = StageCtx::new(run_ctx(None));
        let mut findings = Vec::new();

        let first = match attempt(stage, &ctx, &case.frame) {
            Attempt::Panicked(message) => {
                findings.push(Finding {
                    check: Check::Survival,
                    case: case.name.clone(),
                    detail: format!("panicked: {message}"),
                });
                return findings;
            }
            // An error is a legitimate answer: a stage that cannot work with
            // what it was handed says so and fails its own group (§14).
            Attempt::Failed(_) => return findings,
            Attempt::Produced(output) => output,
        };

        if self.runs(Check::Contract) {
            findings.extend(contract_findings(&first, &case.frame, &case.name));
        }
        if self.runs(Check::Ports) {
            findings.extend(port_findings(descriptor, &first, &case.name));
        }
        if self.runs(Check::Determinism) && descriptor.pure {
            findings.extend(self.determinism_findings(stage, case, &first));
        }
        findings
    }

    fn determinism_findings(
        &self,
        stage: &mut dyn Stage,
        case: &Case,
        first: &StageOutput,
    ) -> Vec<Finding> {
        let ctx = StageCtx::new(run_ctx(None));
        let mut findings = Vec::new();
        let expected = digest(first);

        if let Attempt::Produced(again) = attempt(stage, &ctx, &case.frame) {
            if digest(&again) != expected {
                findings.push(Finding {
                    check: Check::Determinism,
                    case: case.name.clone(),
                    detail: "the same group twice through one instance gave two different outputs"
                        .to_owned(),
                });
            }
        }

        match self.configured() {
            Ok(mut fresh) => {
                if let Attempt::Produced(output) = attempt(&mut *fresh, &ctx, &case.frame) {
                    if digest(&output) != expected {
                        findings.push(Finding {
                            check: Check::Determinism,
                            case: case.name.clone(),
                            detail:
                                "a fresh instance gave a different output from the one in service"
                                    .to_owned(),
                        });
                    }
                }
                if fresh.cache_salt() != stage.cache_salt() {
                    findings.push(Finding {
                        check: Check::Determinism,
                        case: case.name.clone(),
                        detail: "two instances with the same parameters salt the cache key \
                                 differently, so neither would ever hit it"
                            .to_owned(),
                    });
                }
            }
            Err(finding) => findings.push(finding),
        }
        findings
    }

    fn cancellation_findings(&self, cases: &[Case]) -> Vec<Finding> {
        let Some(case) = cases.iter().find(|case| case.ordinary) else {
            return Vec::new();
        };
        let mut stage = match self.configured() {
            Ok(stage) => stage,
            Err(finding) => return vec![finding],
        };
        let cancel = Arc::new(AtomicBool::new(true));
        let ctx = StageCtx::new(run_ctx(Some(cancel)));
        match attempt(&mut *stage, &ctx, &case.frame) {
            Attempt::Failed(StageError::Cancelled) => Vec::new(),
            Attempt::Panicked(message) => vec![Finding {
                check: Check::Survival,
                case: case.name.clone(),
                detail: format!("panicked once cancelled: {message}"),
            }],
            _ => vec![Finding {
                check: Check::Cancellation,
                case: case.name.clone(),
                detail: "a cancelled run was finished through; a stage should check \
                         `ctx.check()` at chunk boundaries"
                    .to_owned(),
            }],
        }
    }
}

/// What one call to `process` did.
enum Attempt {
    Produced(StageOutput),
    Failed(StageError),
    Panicked(String),
}

fn attempt(stage: &mut dyn Stage, ctx: &StageCtx, frame: &GroupFrame) -> Attempt {
    match panic::catch_unwind(AssertUnwindSafe(|| stage.process(ctx, frame))) {
        Ok(Ok(output)) => Attempt::Produced(output),
        Ok(Err(error)) => Attempt::Failed(error),
        Err(payload) => Attempt::Panicked(panic_message(payload.as_ref())),
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

fn declaration_findings(descriptor: &StageDescriptor) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut note = |detail: String| {
        findings.push(Finding {
            check: Check::Declaration,
            case: String::new(),
            detail,
        });
    };

    if !descriptor.is_consistent() {
        note("the descriptor repeats a port or parameter name, or has no kind".to_owned());
    }
    // `dsp.filter.biquad`, `ext.sample.gain`: a namespace is what keeps a
    // user's stage from shadowing a built-in (§9.9).
    if !descriptor.kind.contains('.') {
        note(format!(
            "'{}' is not namespaced; a kind reads 'family.group.name'",
            descriptor.kind
        ));
    }
    if descriptor.version == 0 {
        note("version 0 cannot be bumped downwards when behaviour changes".to_owned());
    }
    if descriptor.label.is_empty() {
        note("no label, so the rail would draw an unnamed chip".to_owned());
    }
    if descriptor.summary.is_empty() {
        note("no summary, so the palette would list it with no explanation".to_owned());
    }
    findings
}

fn contract_findings(output: &StageOutput, frame: &GroupFrame, case: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    if let Err(error) = output.check_covers(frame) {
        findings.push(Finding {
            check: Check::Contract,
            case: case.to_owned(),
            detail: error.to_string(),
        });
        // `apply` would fail for the same reason.
        return findings;
    }
    if let Err(error) = frame.apply(output, 0) {
        findings.push(Finding {
            check: Check::Contract,
            case: case.to_owned(),
            detail: format!("the next stage's frame could not be built: {error}"),
        });
    }
    for out in &output.signals {
        if let SignalOut::Add { name, .. } = out {
            if name.trim().is_empty() {
                findings.push(Finding {
                    check: Check::Contract,
                    case: case.to_owned(),
                    detail: "a signal was added with no name".to_owned(),
                });
            }
        }
    }
    findings
}

fn port_findings(descriptor: &StageDescriptor, output: &StageOutput, case: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for port in descriptor.outputs {
        let PortKind::Artifact(kind) = port.kind else {
            continue;
        };
        let published = output.artifacts.iter().find(|a| a.port == port.name);
        match published {
            Some(artifact) if artifact.kind != kind => findings.push(Finding {
                check: Check::Ports,
                case: case.to_owned(),
                detail: format!(
                    "port '{}' declares {kind} but carried {}",
                    port.name, artifact.kind
                ),
            }),
            None if port.required => findings.push(Finding {
                check: Check::Ports,
                case: case.to_owned(),
                detail: format!("required port '{}' carried nothing", port.name),
            }),
            _ => {}
        }
    }
    for artifact in &output.artifacts {
        if !descriptor.outputs.iter().any(|p| p.name == artifact.port) {
            findings.push(Finding {
                check: Check::Ports,
                case: case.to_owned(),
                detail: format!(
                    "'{}' was published on undeclared port '{}'",
                    artifact.kind, artifact.port
                ),
            });
        }
    }
    findings
}

/// A canonical digest of everything a stage produced, which is what "the same
/// output" means when the output holds a hundred million samples.
fn digest(output: &StageOutput) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(bytes);
        hasher.update(b"\x1e");
    };

    for signal in &output.signals {
        match signal {
            SignalOut::Replace {
                ordinal,
                samples,
                patch,
            } => {
                field(b"replace");
                field(&ordinal.to_le_bytes());
                field(&samples_digest(samples));
                field(format!("{patch:?}").as_bytes());
            }
            SignalOut::Add {
                name,
                domain,
                samples,
                attrs,
            } => {
                field(b"add");
                field(name.as_bytes());
                field(domain.as_str().as_bytes());
                field(&samples_digest(samples));
                field(serde_json::to_string(attrs).unwrap_or_default().as_bytes());
            }
            SignalOut::Passthrough { ordinal } => {
                field(b"passthrough");
                field(&ordinal.to_le_bytes());
            }
            SignalOut::Drop { ordinal } => {
                field(b"drop");
                field(&ordinal.to_le_bytes());
            }
        }
    }
    for artifact in &output.artifacts {
        field(artifact.port.as_bytes());
        field(artifact.kind.as_bytes());
        field(&artifact.kind_version.to_le_bytes());
        field(artifact.payload_json.as_bytes());
    }
    for patch in &output.properties {
        field(format!("{:?}", patch.target).as_bytes());
        field(patch.key.as_bytes());
        field(
            serde_json::to_string(&patch.value)
                .unwrap_or_default()
                .as_bytes(),
        );
    }
    for (name, value) in &output.metrics {
        field(name.as_bytes());
        field(&value.to_bits().to_le_bytes());
    }
    for diagnostic in &output.diagnostics {
        field(diagnostic.message.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn samples_digest(samples: &SampleBuffer) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[samples.dtype().code()]);
    for value in samples.values() {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn run_ctx(cancel: Option<Arc<AtomicBool>>) -> RunCtx {
    let ctx = RunCtx::new(RunId::new(1), "conformance", 0);
    match cancel {
        Some(flag) => ctx.with_cancel(flag),
        None => ctx,
    }
}

/// The groups every stage is run over: one ordinary, and the four degenerate
/// shapes §14 names.
fn standard_cases() -> Vec<Case> {
    let rate = 1000.0;
    let sine: Vec<f64> = (0..512)
        .map(|i| (std::f64::consts::TAU * 50.0 * i as f64 / rate).sin())
        .collect();
    let ramp: Vec<f64> = (0..512).map(|i| i as f64 / 512.0 - 0.5).collect();

    vec![
        case("ordinary", &[("rf", sine), ("ramp", ramp)], true),
        case("empty group", &[], false),
        case("empty signal", &[("rf", Vec::new())], false),
        case("one sample", &[("rf", vec![0.25])], false),
        case("all NaN", &[("rf", vec![f64::NAN; 32])], false),
        case("DC only", &[("rf", vec![1.0; 256])], true),
        // Not in §14's list, but the same family of trap: an infinity in the
        // middle of an otherwise ordinary signal.
        case(
            "non-finite",
            &[(
                "rf",
                vec![0.0, f64::INFINITY, -1.0, f64::NEG_INFINITY, f64::NAN, 1.0],
            )],
            false,
        ),
    ]
}

fn case(name: &str, signals: &[(&str, Vec<f64>)], ordinary: bool) -> Case {
    Case {
        name: name.to_owned(),
        frame: group(signals),
        ordinary,
    }
}

/// A one-group frame of analog signals at 1 kHz, held in memory.
fn group(signals: &[(&str, Vec<f64>)]) -> GroupFrame {
    let timebase = Timebase::regular(1000.0, 0.0);
    let refs = signals
        .iter()
        .map(|(name, values)| {
            SignalRef::in_memory(
                *name,
                Domain::Analog,
                timebase,
                SampleBuffer::from_f64(DType::F64, values),
                Attributes::new(),
            )
        })
        .collect();
    GroupFrame::new(
        GroupMeta {
            id: GroupId::new(1),
            train_id: TrainId::new(1),
            ordinal: 0,
            name: Some("conformance".to_owned()),
            toa_unit: None,
            attributes: Attributes::new(),
        },
        refs,
        RunId::new(1),
    )
}

#[cfg(test)]
mod tests {
    use sp_core::Diagnostic;

    use super::*;
    use crate::error::ConfigError;
    use crate::param::{ParamDefault, ParamKind, ParamSpec};
    use crate::stage::{ArtifactOut, PortSpec};

    const PARAMS: &[ParamSpec] = &[ParamSpec::new(
        "gain",
        "Gain",
        ParamKind::Float {
            min: None,
            max: None,
        },
        ParamDefault::Float(1.0),
    )];

    static WELL_BEHAVED: StageDescriptor = StageDescriptor::new("test.util.good", 1, "Good")
        .describing("Scales every signal.")
        .taking(PARAMS);

    /// The stage every check is meant to pass.
    #[derive(Debug, Default)]
    struct Good {
        gain: f64,
    }

    impl Stage for Good {
        fn descriptor(&self) -> &'static StageDescriptor {
            &WELL_BEHAVED
        }

        fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
            params.validate(PARAMS)?;
            self.gain = params.f64_or("gain", 1.0);
            Ok(())
        }

        fn process(
            &mut self,
            ctx: &StageCtx,
            input: &GroupFrame,
        ) -> Result<StageOutput, StageError> {
            let mut output = StageOutput::passthrough_of(input);
            for (ordinal, signal) in input.signals.iter().enumerate() {
                ctx.check()?;
                let scaled: Vec<f64> = signal
                    .read_values()?
                    .into_iter()
                    .map(|v| v * self.gain)
                    .collect();
                output.set_signal(
                    ordinal,
                    SignalOut::replace(ordinal, SampleBuffer::from_f64(DType::F64, &scaled)),
                );
            }
            Ok(output)
        }
    }

    fn report_of(factory: StageFactory) -> Report {
        Conformance::of(factory).run()
    }

    fn findings_for(report: &Report, check: Check) -> Vec<&Finding> {
        report
            .findings()
            .iter()
            .filter(|f| f.check == check)
            .collect()
    }

    #[test]
    fn a_well_behaved_stage_conforms() {
        let report = report_of(|| Box::<Good>::default());
        assert!(report.is_pass(), "{}", report.describe());
        assert_eq!(report.kind(), "test.util.good");
        assert!(report.describe().ends_with("conforms"));
        assert_eq!(report.cases().len(), 7);
    }

    #[test]
    fn a_stage_that_drops_a_signal_from_its_account_is_caught() {
        // §9.4's rule: every input is accounted for, so the UI can always say
        // what happened to signal 3.
        static SILENT: StageDescriptor = StageDescriptor::new("test.util.silent", 1, "Silent")
            .describing("Forgets the signals it was given.");

        #[derive(Debug, Default)]
        struct Silent;

        impl Stage for Silent {
            fn descriptor(&self) -> &'static StageDescriptor {
                &SILENT
            }
            fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
                Ok(())
            }
            fn process(
                &mut self,
                _ctx: &StageCtx,
                _input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                Ok(StageOutput::new())
            }
        }

        let report = report_of(|| Box::<Silent>::default());
        assert!(!report.is_pass());
        let contract = findings_for(&report, Check::Contract);
        assert!(!contract.is_empty(), "{}", report.describe());
        assert!(contract[0].detail.contains("did not account"));
    }

    #[test]
    fn a_stage_whose_answer_changes_between_groups_is_caught() {
        static COUNTER: StageDescriptor = StageDescriptor::new("test.util.counter", 1, "Counter")
            .describing("Counts the groups it has seen, and calls itself pure.");

        #[derive(Debug, Default)]
        struct Counter(u32);

        impl Stage for Counter {
            fn descriptor(&self) -> &'static StageDescriptor {
                &COUNTER
            }
            fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
                Ok(())
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                self.0 += 1;
                let mut output = StageOutput::passthrough_of(input);
                output.metric("seen", f64::from(self.0));
                Ok(output)
            }
        }

        let report = report_of(|| Box::<Counter>::default());
        let determinism = findings_for(&report, Check::Determinism);
        assert!(!determinism.is_empty(), "{}", report.describe());
        assert!(determinism
            .iter()
            .any(|f| f.detail.contains("two different outputs")));
    }

    #[test]
    fn an_impure_stage_is_not_asked_to_be_deterministic() {
        static IMPURE: StageDescriptor = StageDescriptor::new("test.util.impure", 1, "Impure")
            .describing("Reads the wall clock, and says so.")
            .impure();

        #[derive(Debug, Default)]
        struct Impure(u32);

        impl Stage for Impure {
            fn descriptor(&self) -> &'static StageDescriptor {
                &IMPURE
            }
            fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
                params.validate(&[])
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                self.0 += 1;
                let mut output = StageOutput::passthrough_of(input);
                output.metric("seen", f64::from(self.0));
                Ok(output)
            }
        }

        let report = report_of(|| Box::<Impure>::default());
        assert!(report.is_pass(), "{}", report.describe());
    }

    #[test]
    fn a_stage_that_never_looks_at_the_cancel_flag_is_caught() {
        static DEAF: StageDescriptor = StageDescriptor::new("test.util.deaf", 1, "Deaf")
            .describing("Finishes whatever the caller wanted.");

        #[derive(Debug, Default)]
        struct Deaf;

        impl Stage for Deaf {
            fn descriptor(&self) -> &'static StageDescriptor {
                &DEAF
            }
            fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
                params.validate(&[])
            }
            fn process(
                &mut self,
                _ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                Ok(StageOutput::passthrough_of(input))
            }
        }

        let report = report_of(|| Box::<Deaf>::default());
        assert_eq!(findings_for(&report, Check::Cancellation).len(), 1);

        // And a stage that genuinely cannot be interrupted says so rather
        // than failing for it.
        let waived = Conformance::of(|| Box::<Deaf>::default())
            .skipping(Check::Cancellation)
            .run();
        assert!(waived.is_pass(), "{}", waived.describe());
        assert!(waived.describe().contains("waived: cancellation"));
    }

    #[test]
    fn a_panic_on_a_degenerate_group_is_a_finding_rather_than_a_crashed_test() {
        static BRITTLE: StageDescriptor = StageDescriptor::new("test.util.brittle", 1, "Brittle")
            .describing("Assumes every group has samples.");

        #[derive(Debug, Default)]
        struct Brittle;

        impl Stage for Brittle {
            fn descriptor(&self) -> &'static StageDescriptor {
                &BRITTLE
            }
            fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
                Ok(())
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                let mut output = StageOutput::passthrough_of(input);
                for signal in &input.signals {
                    let values = signal.read_values()?;
                    // The bug the check exists for.
                    output.metric(signal.name(), values[0]);
                }
                Ok(output)
            }
        }

        let report = report_of(|| Box::<Brittle>::default());
        let survival = findings_for(&report, Check::Survival);
        assert_eq!(survival.len(), 1, "{}", report.describe());
        assert_eq!(survival[0].case, "empty signal");
    }

    #[test]
    fn a_mis_declared_descriptor_is_reported_before_anything_runs() {
        static BAD: StageDescriptor = StageDescriptor::new("nonamespace", 0, "");

        #[derive(Debug, Default)]
        struct Bad;

        impl Stage for Bad {
            fn descriptor(&self) -> &'static StageDescriptor {
                &BAD
            }
            fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
                Ok(())
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                Ok(StageOutput::passthrough_of(input))
            }
        }

        let report = report_of(|| Box::<Bad>::default());
        let declaration = findings_for(&report, Check::Declaration);
        assert_eq!(declaration.len(), 4, "{}", report.describe());
    }

    #[test]
    fn a_parameter_the_descriptor_never_declared_has_to_be_refused() {
        static LAX: StageDescriptor = StageDescriptor::new("test.util.lax", 1, "Lax")
            .describing("Takes whatever it is handed.");

        #[derive(Debug, Default)]
        struct Lax;

        impl Stage for Lax {
            fn descriptor(&self) -> &'static StageDescriptor {
                &LAX
            }
            fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
                Ok(())
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                Ok(StageOutput::passthrough_of(input))
            }
        }

        let report = report_of(|| Box::<Lax>::default());
        let configuration = findings_for(&report, Check::Configuration);
        assert_eq!(configuration.len(), 1, "{}", report.describe());
        assert!(configuration[0].detail.contains("does not declare"));
    }

    #[test]
    fn an_artifact_on_a_port_nobody_declared_is_caught_both_ways() {
        const OUTPUTS: &[PortSpec] = &[PortSpec::required(
            "spectrum",
            PortKind::Artifact("spectrum.v1"),
        )];
        static WRONG: StageDescriptor = StageDescriptor::new("test.util.wrong", 1, "Wrong")
            .describing("Promises a spectrum and publishes something else.")
            .writing(OUTPUTS);

        #[derive(Debug, Default)]
        struct Wrong;

        impl Stage for Wrong {
            fn descriptor(&self) -> &'static StageDescriptor {
                &WRONG
            }
            fn configure(&mut self, _params: &ParamSet) -> Result<(), ConfigError> {
                Ok(())
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                let mut output = StageOutput::passthrough_of(input);
                output.artifacts.push(ArtifactOut {
                    port: "elsewhere".to_owned(),
                    kind: "detections.v1".to_owned(),
                    kind_version: 1,
                    payload_json: "{}".to_owned(),
                    summary: None,
                });
                Ok(output)
            }
        }

        let report = report_of(|| Box::<Wrong>::default());
        let ports = findings_for(&report, Check::Ports);
        // One for the promise it broke, one for the port it invented — on
        // every group.
        assert_eq!(ports.len(), 14, "{}", report.describe());
        assert!(ports.iter().any(|f| f.detail.contains("carried nothing")));
        assert!(ports.iter().any(|f| f.detail.contains("undeclared port")));
    }

    #[test]
    fn a_diagnostic_is_a_legitimate_answer_to_a_degenerate_group() {
        // The harness must not push a stage into pretending it can work with
        // what it was handed: an error fails one group and no more (§14).
        static FUSSY: StageDescriptor = StageDescriptor::new("test.util.fussy", 1, "Fussy")
            .describing("Refuses an empty group rather than inventing an answer.");

        #[derive(Debug, Default)]
        struct Fussy;

        impl Stage for Fussy {
            fn descriptor(&self) -> &'static StageDescriptor {
                &FUSSY
            }
            fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
                params.validate(&[])
            }
            fn process(
                &mut self,
                ctx: &StageCtx,
                input: &GroupFrame,
            ) -> Result<StageOutput, StageError> {
                ctx.check()?;
                if input.signals.iter().any(|s| s.is_empty()) {
                    return Err(StageError::rejected("a signal with no samples"));
                }
                let mut output = StageOutput::passthrough_of(input);
                output.diagnose(Diagnostic::info("looked at every signal"));
                Ok(output)
            }
        }

        let report = report_of(|| Box::<Fussy>::default());
        assert!(report.is_pass(), "{}", report.describe());
    }
}
