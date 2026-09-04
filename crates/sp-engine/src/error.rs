//! Engine errors (`docs/DESIGN.md` §14).

/// Anything that can go wrong building a pyramid or reducing a viewport.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] sp_store::StoreError),

    /// Pyramid bytes do not describe a pyramid. Derived data, so the caller's
    /// remedy is always to drop it and rebuild.
    #[error("corrupt render pyramid: {0}")]
    Corrupt(String),

    /// The caller set the cancel flag; nothing was written (G5).
    #[error("cancelled")]
    Cancelled,
}

impl EngineError {
    pub(crate) fn corrupt(what: impl Into<String>) -> Self {
        Self::Corrupt(what.into())
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
