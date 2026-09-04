//! Errors that stop an import outright (`docs/DESIGN.md` §14).
//!
//! Anything a file can get wrong *locally* — a bad cell, a short group — is a
//! [`crate::Diagnostic`], not one of these. A `CsvError` means the file could
//! not be read as this format at all, or the library refused the write.

use std::path::PathBuf;

/// Anything that can stop an import or an export.
#[derive(Debug, thiserror::Error)]
pub enum CsvError {
    #[error("could not read the file: {0}")]
    Io(#[source] std::io::Error),

    #[error("could not open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{detail}")]
    NotUtf8 { byte_offset: u64, detail: String },

    /// The preamble and the two header rows were not all there.
    #[error(
        "the file ended before its header rows were read (expected {expected}, found {found})"
    )]
    Truncated {
        expected: &'static str,
        found: usize,
    },

    /// No column of the group header could be the count column.
    #[error("no count column in the group header: looked for {looked_for}")]
    NoCountColumn { looked_for: String },

    /// No column of the pulse header could be the time column.
    #[error("no time column in the pulse header: looked for {looked_for}")]
    NoTimeColumn { looked_for: String },

    #[error("column '{column}' named in the profile is not in the {which} header")]
    UnknownColumn { column: String, which: &'static str },

    /// Strict mode found something a tolerant import would have warned about.
    #[error("{0}")]
    Strict(String),

    #[error("the import was cancelled")]
    Cancelled,

    #[error("this dataset was not imported from a CSV file, so it cannot be written back")]
    NotImported,

    #[error(transparent)]
    Store(#[from] sp_store::StoreError),

    #[error("invalid import profile: {0}")]
    Profile(String),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl CsvError {
    pub(crate) fn open(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Open {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn profile(message: impl Into<String>) -> Self {
        Self::Profile(message.into())
    }
}

/// Result alias used throughout the crate.
pub type Result<T, E = CsvError> = std::result::Result<T, E>;
