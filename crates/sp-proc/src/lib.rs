//! Pipeline orchestration. Knows nothing about DSP: this crate defines the
//! `Stage` contract, the registry, ports and the scheduler
//! (`docs/DESIGN.md` §9).
//!
//! The split from `sp-dsp` is deliberate. The orchestration layer must not
//! know what a Butterworth filter is, which is what makes a user's own
//! algorithm a first-class stage (G9) — and is the seam a plugin interface
//! would later slot into.
//!
//! ```text
//! Pipeline ──validate──► StageRegistry ──create──► Stage
//!    │                                               │
//!    └── run_pipeline ──► GroupFrame ──process──► StageOutput ──► run tables
//! ```

pub mod assert;
pub mod cache;
pub mod compare;
pub mod error;
pub mod frame;
pub mod param;
pub mod pipeline;
pub mod registry;
pub mod scheduler;
pub mod stage;

pub use assert::{AssertError, Assertion, GroupFacts, Outcome as AssertOutcome, Subject};
pub use compare::{check_baseline, diff_runs, BaselineReport, DiffOptions, RunDiff};
pub use error::{ConfigError, ProcError, Result, StageError};
pub use frame::{GroupFrame, PortMap, PortValue, SignalRef};
pub use param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
pub use pipeline::{Pipeline, PipelineAssertion, PipelineIssue, PipelineStage};
pub use registry::{Registration, RegistryError, StageFactory, StageRegistry};
pub use scheduler::{run_pipeline, RunControl, RunOptions, RunProgress, RunSummary};
pub use stage::{
    ArtifactOut, AttrPatch, PatchTarget, PortKind, PortSpec, PropertyPatch, RunCtx, SignalOut,
    Stage, StageCtx, StageDescriptor, StageOutput,
};
