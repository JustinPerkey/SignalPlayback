//! Pipeline errors (`docs/DESIGN.md` §14).
//!
//! The three levels are deliberately separate. A [`ConfigError`] is the user's
//! parameters being wrong and is caught before a single group is read; a
//! [`StageError`] fails one group and leaves the rest of the run alone; a
//! [`ProcError`] is the run itself failing.

use sp_core::PropertyError;

/// A stage's parameters do not satisfy its declared contract. Raised by
/// `Stage::configure`, and by pipeline validation before a run starts.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("no parameter named '{0}'")]
    Unknown(String),

    #[error("parameter '{name}' is required")]
    Missing { name: String },

    #[error("parameter '{name}' should be {expected}, not {found}")]
    WrongType {
        name: String,
        expected: &'static str,
        found: &'static str,
    },

    #[error("parameter '{name}' is {value}, outside {min}..={max}")]
    OutOfRange {
        name: String,
        value: f64,
        min: f64,
        max: f64,
    },

    #[error("parameter '{name}' is '{value}'; expected one of {options}")]
    NotAVariant {
        name: String,
        value: String,
        options: String,
    },

    /// The combination is invalid even though each value is: a high-pass
    /// corner above a low-pass one, say.
    #[error("{0}")]
    Rejected(String),
}

impl ConfigError {
    #[must_use]
    pub fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected(message.into())
    }

    /// The parameter this is about, when it is about one.
    #[must_use]
    pub fn parameter(&self) -> Option<&str> {
        match self {
            Self::Unknown(name)
            | Self::Missing { name }
            | Self::WrongType { name, .. }
            | Self::OutOfRange { name, .. }
            | Self::NotAVariant { name, .. } => Some(name),
            Self::Rejected(_) => None,
        }
    }
}

/// A stage failed on one group. The scheduler records it, marks the group
/// failed and moves to the next one: groups are independent, so one bad group
/// does not sink the run (§9.1).
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// The stage could not work with what it was handed.
    #[error("{0}")]
    Rejected(String),

    /// A required input port had no producer at run time. Pipeline validation
    /// catches this at edit time; reaching it here means a stage dropped a
    /// signal the next one needed.
    #[error("stage input '{port}' was not produced by anything upstream")]
    MissingInput { port: String },

    /// The stage did not say what happened to one of its input signals.
    /// `Passthrough` is explicit precisely so this is an error (§9.4).
    #[error("stage did not account for input signal {ordinal} ('{name}')")]
    UnhandledSignal { ordinal: usize, name: String },

    /// The stage addressed an input signal that is not in the frame.
    #[error("stage referred to input signal {ordinal}, but the group has {count}")]
    NoSuchSignal { ordinal: usize, count: usize },

    #[error("reading samples: {0}")]
    Store(#[from] sp_store::StoreError),

    #[error("serialising an artifact: {0}")]
    Json(#[from] serde_json::Error),

    #[error("a property write-back was rejected: {0}")]
    Property(#[from] PropertyError),

    /// The caller set the cancel flag (§4.2).
    #[error("cancelled")]
    Cancelled,
}

impl StageError {
    #[must_use]
    pub fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected(message.into())
    }
}

/// The run as a whole could not proceed.
#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("no stage is registered under the kind '{0}'")]
    UnknownStage(String),

    /// The pipeline is not runnable. Every issue is reported at once so the
    /// editor can flag them all in place rather than one per attempt.
    #[error("the pipeline has {} problem(s): {}", .0.len(), crate::pipeline::describe_issues(.0))]
    Invalid(Vec<crate::pipeline::PipelineIssue>),

    #[error("a pipeline needs at least one enabled stage")]
    Empty,

    #[error("the run covers no groups")]
    NoGroups,

    #[error(transparent)]
    Store(#[from] sp_store::StoreError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The caller set the cancel flag. The run is recorded as `cancelled`
    /// with whatever it finished before then.
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T, E = ProcError> = std::result::Result<T, E>;
