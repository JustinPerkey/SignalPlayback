//! Persistence for SignalPlayback: one SQLite file holding metadata, sample and
//! pulse-field columns, render pyramids and artifact payloads
//! (`docs/DESIGN.md` §5).
//!
//! The crate is organised in two layers:
//!
//! - **Row-level functions** in [`blob`], [`library`], [`profiles`], [`props`],
//!   [`pulses`] and [`verify`] take a `&rusqlite::Connection` (a `Transaction` derefs to
//!   one) and do exactly one thing. They are what tests exercise, and what a
//!   job composes inside a transaction.
//! - **[`Store`]** owns the connections: a single writer connection on its own
//!   thread plus a pool of read-only connections. Jobs are closures shipped to
//!   the writer; reads run on the caller's thread against a pooled reader.
//!
//! No SQL escapes this crate: every caller goes through a typed function.

pub mod blob;
pub mod db;
pub mod error;
pub mod library;
pub mod profiles;
pub mod props;
pub mod pulses;
pub mod pyramid;
pub mod runs;
pub mod trains;
pub mod verify;

pub use blob::{BlobId, BlobInfo, BlobKind, BlobWriter, ColumnHeader, DEFAULT_CHUNK_SIZE};
pub use db::{Store, SCHEMA_VERSION};
pub use error::{Result, StoreError};
pub use library::{LibrarySummary, NewDataset, NewGroup, NewSignal};
pub use profiles::SavedProfile;
pub use props::PropertyQuery;
pub use pulses::{NewPulseField, NewPulseGroup, PulsePredicate};
pub use pyramid::PyramidRef;
pub use runs::{
    ArtifactRow, CacheHit, NewArtifact, NewPipeline, NewRun, NewRunSignal, PipelineRow,
    PipelineStageRow, RunGroupRow, RunRow, RunSignalRow, RunStageRow, SOURCE_STAGE,
};
pub use trains::NewTrain;
pub use verify::VerifyReport;

/// Re-exported so callers can compose jobs without depending on `rusqlite`
/// directly.
pub use rusqlite::Connection;
