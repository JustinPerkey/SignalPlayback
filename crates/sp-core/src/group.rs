//! Datasets and signal groups.
//!
//! A group is a block from an imported CSV file or a bundle of generated
//! signals, and it is also the unit of processing: one group enters stage 1 of
//! a pipeline and the whole sequence is recorded (`docs/DESIGN.md` §9.1).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::props::Attributes;
use crate::signal::ParseTokenError;
use crate::time::Timestamp;

crate::id_newtype! {
    /// Identifies a dataset: one import run, one generation batch, or one
    /// derived collection.
    DatasetId
}

crate::id_newtype! {
    /// Identifies a signal group.
    GroupId
}

/// How a dataset came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    CsvImport,
    Generated,
    Derived,
}

impl SourceKind {
    pub const ALL: [Self; 3] = [Self::CsvImport, Self::Generated, Self::Derived];

    /// The token stored in `dataset.source_kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CsvImport => "csv_import",
            Self::Generated => "generated",
            Self::Derived => "derived",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CsvImport => "Imported",
            Self::Generated => "Generated",
            Self::Derived => "Derived",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SourceKind {
    type Err = ParseTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| ParseTokenError {
                kind: "source kind",
                token: s.to_owned(),
            })
    }
}

/// One import run, one generation batch, or one derived collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dataset {
    pub id: DatasetId,
    pub name: String,
    pub source_kind: SourceKind,
    /// Original file path or generator description, when there is one.
    pub source_uri: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_utc: Timestamp,
    pub notes: Option<String>,
    pub attributes: Attributes,
}

/// A group block from a CSV file, or a bundle of generated signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalGroup {
    pub id: GroupId,
    pub dataset_id: DatasetId,
    /// Position within the dataset; matches block order in the source file.
    pub ordinal: u32,
    pub name: Option<String>,
    /// The `count` field from the group header row.
    pub declared_count: u32,
    /// Signal rows actually read. Differs from `declared_count` only after a
    /// tolerant-mode import warning (§7.3).
    pub actual_count: u32,
    pub attributes: Attributes,
}

impl SignalGroup {
    /// Whether the block's declared signal count matched what was read.
    #[must_use]
    pub fn count_matches(&self) -> bool {
        self.declared_count == self.actual_count
    }

    /// The group's name, falling back to its position for unnamed blocks.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("Group {}", self.ordinal))
    }

    /// The identity a stage sees.
    #[must_use]
    pub fn meta(&self) -> GroupMeta {
        GroupMeta {
            id: self.id,
            ordinal: self.ordinal,
            name: self.name.clone(),
            attributes: self.attributes.clone(),
        }
    }
}

/// The group identity and properties carried through a pipeline run.
///
/// This is the metadata half of the `GroupFrame` that `sp-proc` will flow
/// between stages (§9.3); the signal half stays lazily memory-mapped and so
/// cannot live in this crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMeta {
    pub id: GroupId,
    pub ordinal: u32,
    pub name: Option<String>,
    pub attributes: Attributes,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(declared: u32, actual: u32) -> SignalGroup {
        SignalGroup {
            id: GroupId::new(1),
            dataset_id: DatasetId::new(1),
            ordinal: 3,
            name: None,
            declared_count: declared,
            actual_count: actual,
            attributes: Attributes::new(),
        }
    }

    #[test]
    fn source_kind_tokens_match_the_schema_check_constraint() {
        let tokens: Vec<_> = SourceKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(tokens, ["csv_import", "generated", "derived"]);
        for kind in SourceKind::ALL {
            assert_eq!(kind.as_str().parse::<SourceKind>().unwrap(), kind);
        }
    }

    #[test]
    fn count_mismatch_is_visible_on_the_group() {
        assert!(group(3, 3).count_matches());
        assert!(!group(3, 2).count_matches());
    }

    #[test]
    fn unnamed_groups_display_by_position() {
        assert_eq!(group(1, 1).display_name(), "Group 3");
        let mut named = group(1, 1);
        named.name = Some("ANTENNA_A".into());
        assert_eq!(named.display_name(), "ANTENNA_A");
    }
}
