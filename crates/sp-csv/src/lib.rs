//! The project's grouped-block CSV format: block framer, parser, import
//! profiles and the round-tripping writer (`docs/DESIGN.md` §7).
//!
//! The file carries **pulse records**, not sampled waveforms: a preamble, two
//! header rows once at file level, then group rows each followed by that
//! group's pulse rows, with the group row's count field as the sole
//! block-termination rule. Nothing looks ahead, so a multi-gigabyte file
//! previews instantly ([`sniff`]) and ingests in one streaming pass
//! ([`ingest`]) with one group in memory at a time.
//!
//! ```text
//! pick file → sniff → preview & column mapping → confirm
//!     → streaming ingest → column blob per field → dataset in the library
//! ```
//!
//! [`export`] reverses the grammar exactly, which is goal G1.

pub mod control;
pub mod diag;
pub mod error;
pub mod export;
pub mod framer;
pub mod ingest;
pub mod parse;
pub mod profile;
pub mod sniff;

pub use control::{ImportControl, ImportProgress};
pub use diag::{Diagnostic, Diagnostics, Severity};
pub use error::{CsvError, Result};
pub use export::{export_dataset, export_dataset_to_file, layout_of};
pub use framer::{ColumnData, Framer, GroupBlock};
pub use ingest::{import_file, import_reader, ImportReport, ImportRequest};
pub use profile::{
    ColumnRule, CountMode, ImportProfile, Layout, StoredColumn, COUNT_CANDIDATES, NAME_CANDIDATES,
    TIME_CANDIDATES,
};
pub use sniff::{sniff, sniff_file, ColumnHint, Preview};
