//! Datasets, signal trains and signal groups.
//!
//! A **train** is one capture: everything one source file carries. A **group**
//! is one block within it — a dwell, a scan, a generation batch. Groups are
//! segments of a train, not independent captures, so the train is what a
//! signal is stored and reasoned about under (`docs/DESIGN.md` §6.6).
//!
//! The group remains the unit of processing: one group enters stage 1 of a
//! pipeline and the whole sequence is recorded (§9.1).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::props::Attributes;
use crate::pulse::TimeUnit;
use crate::signal::ParseTokenError;
use crate::time::Timestamp;

crate::id_newtype! {
    /// Identifies a dataset: one import run, one generation batch, or one
    /// derived collection.
    DatasetId
}

crate::id_newtype! {
    /// Identifies a signal train: one capture, holding one or more groups.
    TrainId
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

/// One signal train: everything a single source file carries.
///
/// A train exists because groups are *not* independent — a train resolves to
/// several of them, and the pulses in those groups are one capture (§6.6). It
/// is the level a signal is named, tagged and searched at; the groups under it
/// are its segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalTrain {
    pub id: TrainId,
    pub dataset_id: DatasetId,
    /// Position within the dataset. One imported file is one train, so this is
    /// zero for a single-file import.
    pub ordinal: u32,
    pub name: Option<String>,
    /// Source unit of the train's time-of-arrival columns, for a train of
    /// pulse records. `None` for a train of sampled signals.
    pub toa_unit: Option<TimeUnit>,
    pub attributes: Attributes,
}

impl SignalTrain {
    /// Whether this train holds pulse records rather than sampled signals.
    #[must_use]
    pub fn is_pulse_train(&self) -> bool {
        self.toa_unit.is_some()
    }

    /// The train's name, falling back to its position for an unnamed one.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("Train {}", self.ordinal))
    }
}

/// One block within a train: a group row and the pulses it declares, or a
/// bundle of generated signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalGroup {
    pub id: GroupId,
    pub train_id: TrainId,
    /// Position within the train; matches block order in the source file.
    pub ordinal: u32,
    pub name: Option<String>,
    /// The `count` field from the group header row.
    pub declared_count: u32,
    /// Pulse rows actually read. Differs from `declared_count` only after a
    /// tolerant-mode import warning (§7.3).
    pub actual_count: u32,
    /// Source unit of the group's time-of-arrival column, for a group of pulse
    /// records (§6.6). `None` for a group of sampled signals, which carries its
    /// timebase on each signal instead.
    pub toa_unit: Option<TimeUnit>,
    pub attributes: Attributes,
}

impl SignalGroup {
    /// Whether the block's declared count matched what was read.
    #[must_use]
    pub fn count_matches(&self) -> bool {
        self.declared_count == self.actual_count
    }

    /// Whether this group holds pulse records rather than sampled signals.
    /// Decides which viewer the library opens and how a stage reads the group.
    #[must_use]
    pub fn is_pulse_group(&self) -> bool {
        self.toa_unit.is_some()
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
            train_id: self.train_id,
            ordinal: self.ordinal,
            name: self.name.clone(),
            toa_unit: self.toa_unit,
            attributes: self.attributes.clone(),
        }
    }
}

/// The group identity and properties carried through a pipeline run.
///
/// This is the metadata half of the `GroupFrame` that `sp-proc` will flow
/// between stages (§9.3); the signal half is read lazily from the store, by
/// span, and so cannot live in this crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMeta {
    pub id: GroupId,
    /// The train the group is a segment of; a stage that needs the whole
    /// capture reaches the sibling groups through it (§6.6).
    pub train_id: TrainId,
    pub ordinal: u32,
    pub name: Option<String>,
    /// Set when the group holds pulse records; see [`SignalGroup::toa_unit`].
    pub toa_unit: Option<TimeUnit>,
    pub attributes: Attributes,
}

impl GroupMeta {
    /// Whether this group holds pulse records rather than sampled signals.
    #[must_use]
    pub fn is_pulse_group(&self) -> bool {
        self.toa_unit.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(declared: u32, actual: u32) -> SignalGroup {
        SignalGroup {
            id: GroupId::new(1),
            train_id: TrainId::new(1),
            ordinal: 3,
            name: None,
            declared_count: declared,
            actual_count: actual,
            toa_unit: None,
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
    fn a_toa_unit_is_what_marks_a_group_as_pulse_records() {
        let mut sampled = group(3, 3);
        assert!(!sampled.is_pulse_group());
        assert!(!sampled.meta().is_pulse_group());

        sampled.toa_unit = Some(TimeUnit::Microseconds);
        assert!(sampled.is_pulse_group());
        // The marker has to reach a stage, which only ever sees the metadata.
        assert!(sampled.meta().is_pulse_group());
        assert_eq!(sampled.meta().toa_unit, Some(TimeUnit::Microseconds));
    }

    #[test]
    fn unnamed_groups_display_by_position() {
        assert_eq!(group(1, 1).display_name(), "Group 3");
        let mut named = group(1, 1);
        named.name = Some("ANTENNA_A".into());
        assert_eq!(named.display_name(), "ANTENNA_A");
    }

    #[test]
    fn a_group_carries_its_train_through_to_a_stage() {
        // Groups are segments of one capture, so a stage that needs the rest
        // of the train has to be able to find it.
        let group = group(2, 2);
        assert_eq!(group.meta().train_id, TrainId::new(1));
    }

    #[test]
    fn a_toa_unit_is_what_marks_a_train_as_pulse_records() {
        let mut train = SignalTrain {
            id: TrainId::new(1),
            dataset_id: DatasetId::new(1),
            ordinal: 0,
            name: None,
            toa_unit: None,
            attributes: Attributes::new(),
        };
        assert!(!train.is_pulse_train());
        assert_eq!(train.display_name(), "Train 0");

        train.toa_unit = Some(TimeUnit::Microseconds);
        train.name = Some("capture-01".into());
        assert!(train.is_pulse_train());
        assert_eq!(train.display_name(), "capture-01");
    }
}
