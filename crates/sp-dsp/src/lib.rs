//! Built-in `Stage` implementations, written against the same public trait a
//! user's own algorithm would use (`docs/DESIGN.md` §9.8).
//!
//! Nothing in here is privileged: every stage is registered through the same
//! [`sp_proc::StageRegistry`] a plugin would use, declares its parameters the
//! same way, and is subject to the same "account for every input signal" rule.
//! That is what makes G9 — a user's algorithm is a first-class stage —
//! testable rather than aspirational.
//!
//! [`builtins`] is the list; [`registry`] is the ready-made registry the app
//! starts from.

pub mod artifacts;
pub mod condition;
pub mod detect;
pub mod filter;
pub mod measure;
pub mod util;

use sp_core::artifact::{ArtifactRegistry, RegistryError as ArtifactRegistryError};
use sp_core::{DType, SampleBuffer};
use sp_proc::registry::{RegistryError, StageFactory, StageRegistry};
use sp_proc::stage::{PortKind, PortSpec};
use sp_proc::SignalRef;

pub use artifacts::{Detections, Statistics};
pub use condition::{Detrend, Gain, Normalise};
pub use detect::Threshold;
pub use filter::{Biquad, Coefficients, Response};
pub use measure::StatisticsStage;
pub use util::Passthrough;

/// The signals port every conditioning stage reads.
pub(crate) const SIGNALS_IN: &[PortSpec] = &[PortSpec::required("signals", PortKind::ANY_SIGNALS)];

/// The signals port every conditioning stage writes.
pub(crate) const SIGNALS_OUT: &[PortSpec] = &[PortSpec::required("signals", PortKind::ANY_SIGNALS)];

/// Every built-in stage, in the order the palette lists them.
#[must_use]
pub fn builtins() -> Vec<StageFactory> {
    vec![
        || Box::<Passthrough>::default(),
        || Box::<Gain>::default(),
        || Box::<Detrend>::default(),
        || Box::<Normalise>::default(),
        || Box::<Biquad>::default(),
        || Box::<StatisticsStage>::default(),
        || Box::<Threshold>::default(),
    ]
}

/// A registry holding every built-in stage.
///
/// Fails only if a built-in is mis-declared, which is a bug in this crate
/// rather than in the caller — so the app can treat it as a startup assertion.
pub fn registry() -> Result<StageRegistry, RegistryError> {
    let mut registry = StageRegistry::new();
    registry.register_all(builtins())?;
    Ok(registry)
}

/// A registry holding every artifact kind the built-in stages emit.
///
/// The results screen decodes a stored payload through this: an artifact row
/// carries a kind string, and the schema is what turns it back into something
/// drawable (§10.1).
pub fn artifact_registry() -> Result<ArtifactRegistry, ArtifactRegistryError> {
    let mut registry = ArtifactRegistry::new();
    registry.register::<Statistics>()?;
    registry.register::<Detections>()?;
    Ok(registry)
}

/// A buffer of the same dtype as `signal`, unless that dtype cannot hold the
/// result.
///
/// Processing happens in `f64` and is narrowed on write (§17.10). Narrowing
/// back to an integer dtype would quantise a filtered or normalised signal to
/// whole counts, so an integer input comes out as `f64`; a float input keeps
/// its width.
pub(crate) fn buffer_like(signal: &SignalRef, values: &[f64]) -> SampleBuffer {
    let dtype = match signal_dtype(signal) {
        DType::F32 => DType::F32,
        _ => DType::F64,
    };
    SampleBuffer::from_f64(dtype, values)
}

/// The dtype of a signal's samples, read from one sample rather than the whole
/// column.
fn signal_dtype(signal: &SignalRef) -> DType {
    signal
        .read(sp_core::time::SampleRange::first(1))
        .map_or(DType::F64, |buffer| buffer.dtype())
}

#[cfg(test)]
pub(crate) mod tests {
    use sp_core::{Attributes, Domain, GroupId, GroupMeta, RunId, SampleBuffer, Timebase, TrainId};
    use sp_proc::error::StageError;
    use sp_proc::stage::{Stage, StageCtx, StageOutput};
    use sp_proc::{GroupFrame, RunCtx, SignalRef};

    use super::*;

    /// A one-group frame of analog signals sampled at 1 kHz.
    pub(crate) fn frame(signals: &[(&str, Vec<f64>)]) -> GroupFrame {
        frame_at(Timebase::regular(1000.0, 0.0), signals)
    }

    pub(crate) fn frame_at(timebase: Timebase, signals: &[(&str, Vec<f64>)]) -> GroupFrame {
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
                name: Some("test group".into()),
                toa_unit: None,
                attributes: Attributes::new(),
            },
            refs,
            RunId::new(1),
        )
    }

    /// Runs one stage over a frame and applies the result, which is what the
    /// scheduler does — so a stage that fails the §9.4 contract fails here.
    pub(crate) fn process(stage: &mut dyn Stage, input: &GroupFrame) -> (GroupFrame, StageOutput) {
        let (next, output) = try_process(stage, input).expect("the stage should have run");
        (next, output)
    }

    pub(crate) fn try_process(
        stage: &mut dyn Stage,
        input: &GroupFrame,
    ) -> Result<(GroupFrame, StageOutput), StageError> {
        let ctx = StageCtx::new(RunCtx::new(RunId::new(1), "test-hash", 0));
        let output = stage.process(&ctx, input)?;
        let next = input.apply(&output, 0)?;
        Ok((next, output))
    }

    pub(crate) fn values_of(frame: &GroupFrame, ordinal: usize) -> Vec<f64> {
        frame.signals[ordinal]
            .read_values()
            .expect("in-memory signals always read")
    }

    #[test]
    fn every_builtin_registers() {
        let registry = registry().unwrap();
        assert_eq!(registry.len(), builtins().len());
        assert!(registry.contains("dsp.filter.biquad"));
    }

    #[test]
    fn every_builtin_kind_is_namespaced_and_declared_consistently() {
        for descriptor in registry().unwrap().descriptors() {
            assert!(
                descriptor.kind.starts_with("dsp."),
                "{} should be namespaced",
                descriptor.kind
            );
            assert!(descriptor.is_consistent(), "{}", descriptor.kind);
            assert!(!descriptor.summary.is_empty(), "{}", descriptor.kind);
        }
    }

    #[test]
    fn every_builtin_is_pure_so_its_output_can_be_reused() {
        for descriptor in registry().unwrap().descriptors() {
            assert!(descriptor.pure, "{}", descriptor.kind);
        }
    }

    #[test]
    fn every_artifact_a_builtin_declares_is_registered() {
        // A stage that writes an artifact kind nothing can decode would show
        // as a JSON tree, which is the fallback rather than the intent.
        let artifacts = artifact_registry().unwrap();
        for descriptor in registry().unwrap().descriptors() {
            for port in descriptor.outputs {
                if let PortKind::Artifact(kind) = port.kind {
                    assert!(artifacts.contains(kind), "{kind} is not registered");
                }
            }
        }
        assert_eq!(artifacts.len(), 2);
    }

    #[test]
    fn an_integer_signal_comes_out_as_floats() {
        // Narrowing a filtered signal back to counts would quantise it.
        let integer = SignalRef::in_memory(
            "counts",
            Domain::Analog,
            Timebase::regular(1000.0, 0.0),
            SampleBuffer::from_f64(DType::I16, &[1.0, 2.0]),
            Attributes::new(),
        );
        assert_eq!(buffer_like(&integer, &[0.5, 1.5]).dtype(), DType::F64);
    }

    #[test]
    fn a_float_signal_keeps_its_width() {
        let narrow = SignalRef::in_memory(
            "f32",
            Domain::Analog,
            Timebase::regular(1000.0, 0.0),
            SampleBuffer::from_f64(DType::F32, &[1.0]),
            Attributes::new(),
        );
        assert_eq!(buffer_like(&narrow, &[0.5]).dtype(), DType::F32);
    }
}
