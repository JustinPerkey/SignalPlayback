//! Domain vocabulary for SignalPlayback.
//!
//! This crate is the workspace root of the dependency graph: it performs no IO,
//! knows nothing about SQLite, CSV or pixels, and is therefore testable without
//! a window or a library on disk (see `docs/DESIGN.md` §4.1).

pub mod artifact;
pub mod group;
pub mod props;
pub mod pulse;
pub mod run;
pub mod signal;
pub mod stats;
pub mod time;

pub use artifact::{
    Artifact, ArtifactSchema, ColumnSpec, FieldKind, FieldRef, FieldSpec, OverlayForm, ViewHint,
};
pub use group::{
    Dataset, DatasetId, GroupId, GroupMeta, SignalGroup, SignalTrain, SourceKind, TrainId,
};
pub use props::{
    Attributes, PropKind, PropScope, PropertyDef, PropertyError, PropertySet, PropertyValue,
};
pub use pulse::{FieldRange, PulseField, PulseRef, TimeUnit};
pub use run::{
    ArtifactId, Diagnostic, Disposition, PipelineId, Retention, RunStatus, Severity, StageStatus,
};
pub use signal::{
    DType, Domain, Provenance, RunId, SampleBuffer, Samples, Scaling, Signal, SignalId, C64,
};
pub use stats::{MinMax, SignalStats};
pub use time::{SampleIndex, SampleRange, TimeRange, Timebase, Timestamp};

/// Declares an `i64` newtype for a database-backed identifier.
///
/// Ids are `i64` because SQLite `INTEGER PRIMARY KEY` is a signed 64-bit rowid;
/// keeping the same width avoids a lossy conversion at the store boundary.
#[macro_export]
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[derive(::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            /// Wraps a raw rowid.
            #[must_use]
            pub const fn new(raw: i64) -> Self {
                Self(raw)
            }

            /// The underlying rowid.
            #[must_use]
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}#{}", stringify!($name), self.0)
            }
        }

        impl From<i64> for $name {
            fn from(raw: i64) -> Self {
                Self(raw)
            }
        }

        impl From<$name> for i64 {
            fn from(id: $name) -> Self {
                id.0
            }
        }
    };
}
