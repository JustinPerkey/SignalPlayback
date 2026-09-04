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

pub mod control;
pub mod error;
pub mod expr;
pub mod generate;
pub mod noise;
pub mod preset;
pub mod render;
pub mod spec;
pub mod sweep;
pub mod tree;
pub mod validate;
pub mod waveform;

pub use control::{GenControl, GenProgress};
pub use error::{GenError, Result};
pub use generate::{generate, GenReport, GenRequest};
pub use preset::Preset;
pub use render::{render, render_range, render_values, render_with, SourceSignal, Sources};
pub use spec::{
    ConcatPart, EnvelopeSpec, GenSpec, ModKind, Node, NodeKind, NoiseKind, OscShape, Oscillator,
    Sweep,
};
pub use sweep::{ParamRef, ParamSweep, SweepValues};
pub use tree::{NodeRef, ROOT};
pub use validate::{validate, Issue, Issues, Severity};
