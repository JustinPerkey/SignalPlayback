//! Store errors. `sp-app` maps these to user-facing text (`docs/DESIGN.md` §14).

use std::path::PathBuf;

/// Anything that can go wrong talking to a library.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The library was written by a newer application. Downgrades are
    /// refused rather than attempted (§5.2).
    #[error(
        "library schema version {found} is newer than this application supports (up to {supported})"
    )]
    SchemaTooNew { found: u32, supported: u32 },

    #[error("library `app_meta.schema_version` is not a number: {0:?}")]
    SchemaVersionInvalid(String),

    /// Bytes in the library do not describe what their row says they do.
    #[error("corrupt library data: {0}")]
    Corrupt(String),

    #[error("{what} {id} not found")]
    NotFound { what: &'static str, id: i64 },

    #[error("invalid JSON in the library: {0}")]
    Json(#[from] serde_json::Error),

    #[error("stored token is not valid: {0}")]
    Token(#[from] sp_core::signal::ParseTokenError),

    #[error("invalid input: {0}")]
    Invalid(String),

    /// The writer thread is gone; nothing can be written any more.
    #[error("the store's writer thread has stopped")]
    WriterStopped,

    /// A job panicked on the writer thread. The thread survives; the
    /// transaction that was open, if any, has been rolled back.
    #[error("a store job panicked")]
    Panicked,
}

impl StoreError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt(message.into())
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn not_found(what: &'static str, id: i64) -> Self {
        Self::NotFound { what, id }
    }

    /// Whether the error is a row lookup miss, as opposed to a fault.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
            || matches!(self, Self::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }
}

/// Result alias used throughout the crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;
