//! What generation can fail with.

use crate::validate::Issues;

pub type Result<T> = std::result::Result<T, GenError>;

#[derive(Debug, thiserror::Error)]
pub enum GenError {
    /// The spec did not pass [`crate::validate`]; nothing was rendered.
    #[error("the spec is not renderable: {0}")]
    Invalid(Issues),

    /// A [`crate::spec::Node::FromSignal`] pointed at a signal the store could
    /// not produce.
    #[error("source signal {id}: {reason}")]
    Source { id: i64, reason: String },

    #[error("generation was cancelled")]
    Cancelled,

    #[error(transparent)]
    Store(#[from] sp_store::StoreError),

    #[error("gen spec: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl GenError {
    /// The validation issues, when that is what went wrong.
    #[must_use]
    pub fn issues(&self) -> Option<&Issues> {
        match self {
            Self::Invalid(issues) => Some(issues),
            _ => None,
        }
    }
}
