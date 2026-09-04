//! Parametric signal synthesis (`docs/DESIGN.md` §8).
//!
//! A generated signal is a serialisable DAG plus a seed. The samples are a
//! cache of that spec, so deleting a blob is always safe and re-rendering
//! reproduces the same numbers (goal G3).
//!
//! ```text
//! GenSpec ──validate──▶ render(range) ──▶ SampleBuffer
//!    │                                        │
//!    └── sweep ──▶ one spec per rung ─────────┴──▶ one group in the library
//! ```
//!
//! Generation exists to feed the pipeline (§8.4): a sweep produces a whole
//! group of test inputs in one action, with the swept value stored as a
//! property so a run can group by it.
//!
//! [`train`] is the other half. An import carries pulse records, not sampled
//! waveforms (§6.6), so a generated waveform cannot stand in for a captured
//! train; a `TrainSpec` emits exactly what an import does — a train of groups,
//! each a time-of-arrival column plus one column per field.

pub mod control;
pub mod error;
pub mod expr;
pub mod generate;
pub mod noise;
pub mod preset;
pub mod render;
pub mod spec;
pub mod sweep;
pub mod train;
pub mod tree;
pub mod validate;
pub mod waveform;

pub use control::{GenControl, GenProgress};
pub use error::{GenError, Result};
pub use generate::{generate, generate_train, GenReport, GenRequest, TrainRequest};
pub use preset::Preset;
pub use render::{render, render_range, render_values, render_with, SourceSignal, Sources};
pub use spec::{
    ConcatPart, EnvelopeSpec, GenSpec, ModKind, Node, NodeKind, NoiseKind, OscShape, Oscillator,
    Sweep,
};
pub use sweep::{ParamRef, ParamSweep, SweepValues};
pub use train::{FieldSpec, FieldValue, Pri, TrainSpec};
pub use tree::{NodeRef, ROOT};
pub use validate::{validate, validate_train, Issue, Issues, Severity};
