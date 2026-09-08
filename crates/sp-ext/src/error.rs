//! What can go wrong between the host and a native library
//! (`docs/DESIGN.md` §9.9).
//!
//! Loading is a deliberate user action, so every variant here is worded for
//! the settings screen that offered to load the library — it names the file
//! and what about it was unacceptable.

use std::path::PathBuf;

/// A library could not be loaded, or does not conform.
#[derive(Debug, thiserror::Error)]
pub enum ExtError {
    #[error("'{}' is not on the allowed library list", .0.display())]
    NotAllowed(PathBuf),

    #[error("no library at '{}'", .0.display())]
    NotFound(PathBuf),

    #[error("loading '{path}': {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },

    #[error("'{path}' does not export {symbol}, so it is not an external stage")]
    MissingSymbol { path: PathBuf, symbol: String },

    #[error("'{path}' speaks ABI version {found}; this build speaks {expected}")]
    AbiMismatch {
        path: PathBuf,
        found: u32,
        expected: u32,
    },

    #[error("the descriptor from '{path}' is not readable JSON: {source}")]
    Descriptor {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The descriptor parsed but says something the host cannot honour — a
    /// kind outside the `ext.` namespace, a repeated parameter name, a
    /// default that does not match its own declared type.
    #[error("the descriptor from '{path}' is unusable: {reason}")]
    Rejected { path: PathBuf, reason: String },

    #[error("reading '{}': {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `sp_open` refused the parameters, or returned no handle at all.
    #[error("the library refused these parameters: {0}")]
    Open(String),
}

pub type Result<T, E = ExtError> = std::result::Result<T, E>;
