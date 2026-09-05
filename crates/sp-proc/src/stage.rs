//! The `Stage` contract (`docs/DESIGN.md` §9.2–§9.4).
//!
//! A stage is one step of a pipeline: one group in, one [`StageOutput`] out.
//! Everything about it that the editor, the scheduler and the cache need to
//! know is in its [`StageDescriptor`], which is `'static` data — the kind, the
//! version that participates in the cache key, the ports it reads and writes,
//! and the parameters it takes.
//!
//! The built-in stages in `sp-dsp` implement this trait and nothing more, so
//! there is no privileged path a user's own algorithm could not take (G9).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sp_core::artifact::Artifact;
use sp_core::{Attributes, Diagnostic, Domain, PropertyValue, RunId, SampleBuffer};

use crate::error::{ConfigError, StageError};
use crate::frame::GroupFrame;
use crate::param::{ParamSet, ParamSpec};

/// One input or output port of a stage (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpec {
    pub name: &'static str,
    pub kind: PortKind,
    pub required: bool,
}

impl PortSpec {
    #[must_use]
    pub const fn required(name: &'static str, kind: PortKind) -> Self {
        Self {
            name,
            kind,
            required: true,
        }
    }

    #[must_use]
    pub const fn optional(name: &'static str, kind: PortKind) -> Self {
        Self {
            name,
            kind,
            required: false,
        }
    }
}

/// What flows through a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    /// The group's signals. `None` accepts any domain.
    Signals { domain: Option<Domain> },
    /// A typed artifact, by kind, e.g. `spectrum.v1`.
    Artifact(&'static str),
    /// Anything at all; the escape hatch a passthrough or an inspection point
    /// wants.
    Any,
}

impl PortKind {
    /// The signals port with no domain constraint, which most stages take.
    pub const ANY_SIGNALS: Self = Self::Signals { domain: None };

    /// Whether a producer of `other` satisfies a consumer of `self`.
    #[must_use]
    pub fn accepts(self, other: Self) -> bool {
        match (self, other) {
            (Self::Any, _) | (_, Self::Any) => true,
            (Self::Signals { domain: None }, Self::Signals { .. }) => true,
            (Self::Signals { domain: Some(want) }, Self::Signals { domain: Some(got) }) => {
                want == got
            }
            // A producer that does not say which domain it emits could emit
            // the one that is wanted; the run-time port map settles it.
            (Self::Signals { domain: Some(_) }, Self::Signals { domain: None }) => true,
            (Self::Artifact(want), Self::Artifact(got)) => want == got,
            _ => false,
        }
    }

    /// A one-word description for an error message.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Signals { domain: None } => "signals".to_owned(),
            Self::Signals { domain: Some(d) } => format!("{} signals", d.label().to_lowercase()),
            Self::Artifact(kind) => kind.to_owned(),
            Self::Any => "anything".to_owned(),
        }
    }
}

/// A stage's static identity and contract. Drives the pipeline editor's
/// generated UI, port validation and the cache key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageDescriptor {
    /// Stable identity, e.g. `dsp.filter.biquad`.
    pub kind: &'static str,
    /// Bumped when behaviour changes. Part of the cache key, and recorded in
    /// every run, so "this result came from an older algorithm" is always
    /// answerable (§9.2).
    pub version: u32,
    pub label: &'static str,
    /// One line for the stage palette.
    pub summary: &'static str,
    pub inputs: &'static [PortSpec],
    pub outputs: &'static [PortSpec],
    pub params: &'static [ParamSpec],
    /// Whether the same inputs and parameters always give the same output. An
    /// impure stage opts out of caching (§9.5).
    pub pure: bool,
}

impl StageDescriptor {
    /// A descriptor with the common defaults: pure, no ports, no parameters.
    #[must_use]
    pub const fn new(kind: &'static str, version: u32, label: &'static str) -> Self {
        Self {
            kind,
            version,
            label,
            summary: "",
            inputs: &[],
            outputs: &[],
            params: &[],
            pure: true,
        }
    }

    #[must_use]
    pub const fn describing(mut self, summary: &'static str) -> Self {
        self.summary = summary;
        self
    }

    #[must_use]
    pub const fn reading(mut self, inputs: &'static [PortSpec]) -> Self {
        self.inputs = inputs;
        self
    }

    #[must_use]
    pub const fn writing(mut self, outputs: &'static [PortSpec]) -> Self {
        self.outputs = outputs;
        self
    }

    #[must_use]
    pub const fn taking(mut self, params: &'static [ParamSpec]) -> Self {
        self.params = params;
        self
    }

    /// Marks the stage impure, so its output is never cached or reused.
    #[must_use]
    pub const fn impure(mut self) -> Self {
        self.pure = false;
        self
    }

    #[must_use]
    pub fn param(&self, name: &str) -> Option<&ParamSpec> {
        self.params.iter().find(|p| p.name == name)
    }

    /// Whether the declaration is coherent: no repeated port or parameter
    /// name. The registry checks this at startup, so a mis-declared stage
    /// fails loudly rather than behaving oddly at run time.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        fn unique(names: impl Iterator<Item = &'static str>) -> bool {
            let mut seen = Vec::new();
            for name in names {
                if seen.contains(&name) {
                    return false;
                }
                seen.push(name);
            }
            true
        }
        !self.kind.is_empty()
            && unique(self.inputs.iter().map(|p| p.name))
            && unique(self.outputs.iter().map(|p| p.name))
            && unique(self.params.iter().map(|p| p.name))
    }
}

/// What a stage knows about the run it is part of.
#[derive(Debug, Clone)]
pub struct RunCtx {
    pub run: RunId,
    /// Kinds, versions and parameters of the whole pipeline, canonicalised.
    pub pipeline_hash: String,
    /// This stage's position in the pipeline.
    pub stage_ordinal: i32,
    cancel: Arc<AtomicBool>,
}

impl RunCtx {
    #[must_use]
    pub fn new(run: RunId, pipeline_hash: impl Into<String>, stage_ordinal: i32) -> Self {
        Self {
            run,
            pipeline_hash: pipeline_hash.into(),
            stage_ordinal,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Whether the run has been cancelled. A long stage should check this at
    /// chunk boundaries so cancel latency stays under ~50 ms (§4.2).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// `Err(StageError::Cancelled)` once the run is cancelled, for use with
    /// `?` inside a loop.
    pub fn check(&self) -> Result<(), StageError> {
        if self.is_cancelled() {
            return Err(StageError::Cancelled);
        }
        Ok(())
    }
}

/// What a stage knows while processing one group.
#[derive(Debug, Clone)]
pub struct StageCtx {
    pub run: RunCtx,
    /// The user's name for this stage instance, when they gave it one.
    pub label: Option<String>,
}

impl StageCtx {
    #[must_use]
    pub fn new(run: RunCtx) -> Self {
        Self { run, label: None }
    }

    #[must_use]
    pub fn labelled(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }

    pub fn check(&self) -> Result<(), StageError> {
        self.run.check()
    }
}

/// A change to a signal's attributes: values to set, keys to remove.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttrPatch {
    set: Attributes,
    remove: Vec<String>,
}

impl AttrPatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn set(mut self, key: impl Into<String>, value: impl Into<PropertyValue>) -> Self {
        self.set.insert(key, value);
        self
    }

    #[must_use]
    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.remove.push(key.into());
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }

    /// Applies the patch over a signal's existing attributes.
    pub fn apply_to(&self, attributes: &mut Attributes) {
        for key in &self.remove {
            attributes.remove(key);
        }
        for (key, value) in self.set.iter() {
            attributes.insert(key.clone(), value.clone());
        }
    }
}

/// What a stage did to one signal, or a signal it produced (§9.4).
#[derive(Debug, Clone, PartialEq)]
pub enum SignalOut {
    /// Same slot, new samples — a filter, a normaliser.
    Replace {
        ordinal: usize,
        samples: SampleBuffer,
        patch: AttrPatch,
    },
    /// A new signal in the group — an envelope, a demodulated baseband.
    Add {
        name: String,
        domain: Domain,
        samples: SampleBuffer,
        attrs: Attributes,
    },
    /// Unchanged. Costs nothing: same content hash, same blob (§5.3).
    Passthrough { ordinal: usize },
    /// Removed from downstream stages, though still recorded here.
    Drop { ordinal: usize },
}

impl SignalOut {
    /// The input signal this refers to, for the three variants that address
    /// one.
    #[must_use]
    pub fn input_ordinal(&self) -> Option<usize> {
        match self {
            Self::Replace { ordinal, .. }
            | Self::Passthrough { ordinal }
            | Self::Drop { ordinal } => Some(*ordinal),
            Self::Add { .. } => None,
        }
    }

    /// A new signal with no attributes, in the analog domain.
    #[must_use]
    pub fn add(name: impl Into<String>, samples: SampleBuffer) -> Self {
        Self::Add {
            name: name.into(),
            domain: Domain::Analog,
            samples,
            attrs: Attributes::new(),
        }
    }

    /// Replacement samples for an input signal, leaving its attributes alone.
    #[must_use]
    pub fn replace(ordinal: usize, samples: SampleBuffer) -> Self {
        Self::Replace {
            ordinal,
            samples,
            patch: AttrPatch::new(),
        }
    }
}

/// A typed non-signal output, serialised and ready to record (§10.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactOut {
    pub port: String,
    pub kind: String,
    pub kind_version: u32,
    pub payload_json: String,
    pub summary: Option<String>,
}

impl ArtifactOut {
    /// Serialises an [`Artifact`] onto a port, taking its kind, version and
    /// one-line summary from the implementation.
    pub fn publish<A: Artifact>(port: impl Into<String>, value: &A) -> Result<Self, StageError> {
        Ok(Self {
            port: port.into(),
            kind: A::KIND.to_owned(),
            kind_version: A::VERSION,
            payload_json: serde_json::to_string(value)?,
            summary: Some(value.summary()),
        })
    }
}

/// Where a [`PropertyPatch`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchTarget {
    Group,
    Signal { ordinal: usize },
}

/// A property written back onto the group or one of its signals (§9.4).
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyPatch {
    pub target: PatchTarget,
    pub key: String,
    pub value: PropertyValue,
}

impl PropertyPatch {
    #[must_use]
    pub fn group(key: impl Into<String>, value: impl Into<PropertyValue>) -> Self {
        Self {
            target: PatchTarget::Group,
            key: key.into(),
            value: value.into(),
        }
    }

    #[must_use]
    pub fn signal(ordinal: usize, key: impl Into<String>, value: impl Into<PropertyValue>) -> Self {
        Self {
            target: PatchTarget::Signal { ordinal },
            key: key.into(),
            value: value.into(),
        }
    }
}

/// Everything one stage produced for one group.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageOutput {
    pub signals: Vec<SignalOut>,
    pub artifacts: Vec<ArtifactOut>,
    pub properties: Vec<PropertyPatch>,
    /// Scalar summaries, charted across groups.
    pub metrics: BTreeMap<String, f64>,
    pub diagnostics: Vec<Diagnostic>,
}

impl StageOutput {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An output that leaves every signal of `frame` alone. The starting point
    /// for a stage that only measures, and for one that touches some signals:
    /// replacing an entry afterwards keeps the "account for everything" rule
    /// satisfied without listing the rest by hand.
    #[must_use]
    pub fn passthrough_of(frame: &GroupFrame) -> Self {
        Self {
            signals: (0..frame.signals.len())
                .map(|ordinal| SignalOut::Passthrough { ordinal })
                .collect(),
            ..Self::default()
        }
    }

    /// Replaces whatever was recorded for input signal `ordinal`.
    pub fn set_signal(&mut self, ordinal: usize, out: SignalOut) {
        match self
            .signals
            .iter()
            .position(|s| s.input_ordinal() == Some(ordinal))
        {
            Some(at) => self.signals[at] = out,
            None => self.signals.push(out),
        }
    }

    #[must_use]
    pub fn with_signal(mut self, out: SignalOut) -> Self {
        self.signals.push(out);
        self
    }

    #[must_use]
    pub fn with_artifact(mut self, artifact: ArtifactOut) -> Self {
        self.artifacts.push(artifact);
        self
    }

    #[must_use]
    pub fn with_metric(mut self, name: impl Into<String>, value: f64) -> Self {
        self.metrics.insert(name.into(), value);
        self
    }

    #[must_use]
    pub fn with_diagnostic(mut self, diagnostic: Diagnostic) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    pub fn metric(&mut self, name: impl Into<String>, value: f64) {
        self.metrics.insert(name.into(), value);
    }

    pub fn diagnose(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Checks that every input signal was accounted for exactly once, and
    /// that no output addresses a signal the group does not have.
    ///
    /// This is what makes `Passthrough` explicit rather than implicit: the UI
    /// can always say what happened to signal 3 (§9.4).
    pub fn check_covers(&self, frame: &GroupFrame) -> Result<(), StageError> {
        let count = frame.signals.len();
        let mut seen = vec![0usize; count];
        for out in &self.signals {
            let Some(ordinal) = out.input_ordinal() else {
                continue;
            };
            if ordinal >= count {
                return Err(StageError::NoSuchSignal { ordinal, count });
            }
            seen[ordinal] += 1;
        }
        if let Some(ordinal) = seen.iter().position(|&n| n != 1) {
            return Err(StageError::UnhandledSignal {
                ordinal,
                name: frame.signals[ordinal].name().to_owned(),
            });
        }
        Ok(())
    }
}

/// One step of a pipeline.
///
/// `process` must be deterministic given its parameters and input: that is
/// what makes a run comparable with a baseline (G8) and what makes the cache
/// key meaningful. A stage that cannot promise it declares itself
/// [`StageDescriptor::impure`].
pub trait Stage: Send + Sync {
    /// Static identity and contract. Drives the pipeline editor's generated
    /// UI, port validation and the cache key.
    fn descriptor(&self) -> &'static StageDescriptor;

    /// Validates and applies parameters. Called before any group is
    /// processed, so a typo costs nothing but the error.
    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError>;

    /// Called once before this instance's first group.
    fn begin_run(&mut self, _ctx: &RunCtx) -> Result<(), StageError> {
        Ok(())
    }

    /// The work: one group in, one result out.
    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError>;

    /// Called after the last group; may emit run-level artifacts, such as an
    /// ROC curve accumulated across every group.
    fn end_run(&mut self, _ctx: &RunCtx) -> Result<StageOutput, StageError> {
        Ok(StageOutput::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_domain_typed_port_refuses_the_wrong_domain() {
        let iq = PortKind::Signals {
            domain: Some(Domain::BasebandIq),
        };
        let logic = PortKind::Signals {
            domain: Some(Domain::DigitalLogic),
        };
        assert!(iq.accepts(iq));
        assert!(!iq.accepts(logic));
        assert!(PortKind::ANY_SIGNALS.accepts(iq));
        assert!(PortKind::Any.accepts(PortKind::Artifact("spectrum.v1")));
    }

    #[test]
    fn artifact_ports_match_by_kind() {
        let spectrum = PortKind::Artifact("spectrum.v1");
        assert!(spectrum.accepts(PortKind::Artifact("spectrum.v1")));
        assert!(!spectrum.accepts(PortKind::Artifact("detections.v1")));
        assert!(!spectrum.accepts(PortKind::ANY_SIGNALS));
    }

    #[test]
    fn a_descriptor_with_a_repeated_name_is_caught() {
        const DUPLICATE: &[PortSpec] = &[
            PortSpec::required("signals", PortKind::ANY_SIGNALS),
            PortSpec::required("signals", PortKind::Any),
        ];
        let good = StageDescriptor::new("test.ok", 1, "Ok");
        assert!(good.is_consistent());
        assert!(!good.reading(DUPLICATE).is_consistent());
    }

    #[test]
    fn an_attribute_patch_sets_and_removes_over_the_original() {
        let mut attributes = Attributes::new();
        attributes.insert("units", "V");
        attributes.insert("stale", 1.0);

        AttrPatch::new()
            .set("units", "dB")
            .remove("stale")
            .apply_to(&mut attributes);

        assert_eq!(attributes.get_str("units"), Some("dB"));
        assert!(!attributes.contains_key("stale"));
    }

    #[test]
    fn setting_a_signal_replaces_rather_than_appends() {
        let mut output = StageOutput::new();
        output.signals.push(SignalOut::Passthrough { ordinal: 0 });
        output.signals.push(SignalOut::Passthrough { ordinal: 1 });
        output.set_signal(1, SignalOut::Drop { ordinal: 1 });
        assert_eq!(output.signals.len(), 2);
        assert_eq!(output.signals[1], SignalOut::Drop { ordinal: 1 });
    }
}
