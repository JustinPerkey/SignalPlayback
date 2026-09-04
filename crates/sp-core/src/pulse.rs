//! Pulse records (`docs/DESIGN.md` §6.6).
//!
//! A group imported from the project's CSV format is a collection of pulses.
//! The same bytes are seen two ways: stored as one column per field (a
//! [`PulseField`], indistinguishable from a signal to the renderer and to
//! stages), and addressed as records by [`PulseRef`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::group::GroupId;
use crate::signal::{DType, ParseTokenError};
use crate::stats::SignalStats;

/// Identifies one pulse: its group, and its row position within that group.
///
/// The index is the source file's own ordering, so the reference is stable
/// across re-import and costs no storage — an unannotated pulse has no row
/// anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PulseRef {
    pub group: GroupId,
    pub index: u32,
}

impl PulseRef {
    #[must_use]
    pub const fn new(group: GroupId, index: u32) -> Self {
        Self { group, index }
    }
}

impl fmt::Display for PulseRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}]", self.group, self.index)
    }
}

/// The unit a source file expressed times of arrival in. The library timeline
/// is always seconds; this records what to scale by on the way in, and what to
/// restore on export (G1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeUnit {
    Seconds,
    Milliseconds,
    /// The default for pulse data.
    #[default]
    Microseconds,
    Nanoseconds,
}

impl TimeUnit {
    pub const ALL: [Self; 4] = [
        Self::Seconds,
        Self::Milliseconds,
        Self::Microseconds,
        Self::Nanoseconds,
    ];

    /// The token stored in `signal_group.toa_unit`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Seconds => "s",
            Self::Milliseconds => "ms",
            Self::Microseconds => "us",
            Self::Nanoseconds => "ns",
        }
    }

    /// Seconds per unit.
    #[must_use]
    pub const fn to_seconds_factor(self) -> f64 {
        match self {
            Self::Seconds => 1.0,
            Self::Milliseconds => 1e-3,
            Self::Microseconds => 1e-6,
            Self::Nanoseconds => 1e-9,
        }
    }

    /// Converts a value in this unit to seconds on the absolute timeline.
    #[must_use]
    pub fn to_seconds(self, value: f64) -> f64 {
        value * self.to_seconds_factor()
    }

    /// Converts seconds back to this unit, for export.
    #[must_use]
    pub fn from_seconds(self, seconds: f64) -> f64 {
        seconds / self.to_seconds_factor()
    }
}

impl fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TimeUnit {
    type Err = ParseTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|unit| unit.as_str() == s)
            .ok_or_else(|| ParseTokenError {
                kind: "time unit",
                token: s.to_owned(),
            })
    }
}

/// One field of a group's pulse records, stored as a column.
///
/// The cached [`SignalStats`] double as the zone map: cross-group search
/// eliminates a whole group in SQL when its `[min, max]` cannot satisfy the
/// predicate, and only then reads the column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PulseField {
    pub group_id: GroupId,
    /// Column order in the source file.
    pub ordinal: u32,
    /// The header text as written, e.g. `pulse width`.
    pub name: String,
    /// Normalised key, e.g. `pulse_width`; matches a `PropertyDef` key when the
    /// column has been bound to one.
    pub key: String,
    pub unit: Option<String>,
    pub dtype: DType,
    pub stats: Option<SignalStats>,
}

impl PulseField {
    #[must_use]
    pub fn new(group_id: GroupId, ordinal: u32, name: impl Into<String>, dtype: DType) -> Self {
        let name = name.into();
        Self {
            group_id,
            ordinal,
            key: normalise_key(&name),
            name,
            unit: None,
            dtype,
            stats: None,
        }
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    #[must_use]
    pub fn with_stats(mut self, stats: SignalStats) -> Self {
        self.stats = Some(stats);
        self
    }

    /// Whether any value in this column could satisfy `range`, judged from the
    /// zone map alone. `true` when the statistics are not yet known, so an
    /// unsummarised column is scanned rather than silently skipped.
    #[must_use]
    pub fn may_contain(&self, range: FieldRange) -> bool {
        let Some(stats) = self.stats else {
            return true;
        };
        let (Some(min), Some(max)) = (stats.min(), stats.max()) else {
            // No finite values at all.
            return false;
        };
        range.overlaps(min, max)
    }
}

/// A closed numeric predicate over one pulse field. Either bound may be open.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct FieldRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl FieldRange {
    /// Matches everything.
    pub const ANY: Self = Self {
        min: None,
        max: None,
    };

    #[must_use]
    pub const fn new(min: Option<f64>, max: Option<f64>) -> Self {
        Self { min, max }
    }

    #[must_use]
    pub const fn at_least(min: f64) -> Self {
        Self {
            min: Some(min),
            max: None,
        }
    }

    #[must_use]
    pub const fn at_most(max: f64) -> Self {
        Self {
            min: None,
            max: Some(max),
        }
    }

    #[must_use]
    pub const fn between(min: f64, max: f64) -> Self {
        Self {
            min: Some(min),
            max: Some(max),
        }
    }

    /// Whether one value satisfies the predicate. Non-finite values never do,
    /// so a missing field never matches.
    #[must_use]
    pub fn contains(self, value: f64) -> bool {
        value.is_finite()
            && self.min.is_none_or(|min| value >= min)
            && self.max.is_none_or(|max| value <= max)
    }

    /// Whether the predicate could be satisfied somewhere in `[min, max]`.
    #[must_use]
    pub fn overlaps(self, min: f64, max: f64) -> bool {
        self.min.is_none_or(|lo| max >= lo) && self.max.is_none_or(|hi| min <= hi)
    }
}

/// Normalises a header label into a property key: lower case, non-alphanumeric
/// runs collapsed to a single underscore.
#[must_use]
pub fn normalise_key(label: &str) -> String {
    let mut key = String::with_capacity(label.len());
    let mut pending_separator = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !key.is_empty() {
                key.push('_');
            }
            pending_separator = false;
            key.push(ch.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_with(min: f64, max: f64) -> PulseField {
        let stats: SignalStats = [min, max].into_iter().collect();
        PulseField::new(GroupId::new(7), 1, "pulse width", DType::F32).with_stats(stats)
    }

    #[test]
    fn header_labels_normalise_to_property_keys() {
        assert_eq!(normalise_key("pulse width"), "pulse_width");
        assert_eq!(normalise_key("  Total Time  "), "total_time");
        assert_eq!(normalise_key("PRF (Hz)"), "prf_hz");
        assert_eq!(normalise_key("groupID"), "groupid");
        assert_eq!(normalise_key("---"), "");
    }

    #[test]
    fn microseconds_are_the_default_and_round_trip() {
        let unit = TimeUnit::default();
        assert_eq!(unit, TimeUnit::Microseconds);
        assert_eq!(unit.as_str(), "us");
        let seconds = unit.to_seconds(10.0);
        assert!((seconds - 1e-5).abs() < 1e-18);
        assert!((unit.from_seconds(seconds) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn time_unit_tokens_round_trip_through_the_database() {
        for unit in TimeUnit::ALL {
            assert_eq!(unit.as_str().parse::<TimeUnit>().unwrap(), unit);
        }
        assert_eq!("us".parse::<TimeUnit>().unwrap(), TimeUnit::Microseconds);
        // The header may say "µs"; the stored token is always ASCII.
        assert!("µs".parse::<TimeUnit>().is_err());
    }

    #[test]
    fn zone_map_eliminates_groups_that_cannot_match() {
        let field = field_with(100.0, 200.0);
        assert!(field.may_contain(FieldRange::between(150.0, 300.0)));
        assert!(field.may_contain(FieldRange::at_most(100.0)));
        assert!(!field.may_contain(FieldRange::at_least(201.0)));
        assert!(!field.may_contain(FieldRange::at_most(99.0)));
        assert!(field.may_contain(FieldRange::ANY));
    }

    #[test]
    fn an_unsummarised_column_is_scanned_not_skipped() {
        let field = PulseField::new(GroupId::new(1), 0, "power", DType::F32);
        assert!(field.stats.is_none());
        assert!(field.may_contain(FieldRange::between(1.0, 2.0)));
    }

    #[test]
    fn an_all_nan_column_can_never_match() {
        let stats: SignalStats = [f64::NAN].into_iter().collect();
        let field = PulseField::new(GroupId::new(1), 0, "angle", DType::F32).with_stats(stats);
        assert!(!field.may_contain(FieldRange::ANY));
    }

    #[test]
    fn predicates_reject_missing_values() {
        let range = FieldRange::between(0.0, 10.0);
        assert!(range.contains(0.0));
        assert!(range.contains(10.0));
        assert!(!range.contains(10.5));
        assert!(!range.contains(f64::NAN));
        assert!(!FieldRange::ANY.contains(f64::NAN));
    }

    #[test]
    fn pulse_refs_read_as_group_and_index() {
        let pulse = PulseRef::new(GroupId::new(7), 1);
        assert_eq!(pulse.to_string(), "GroupId#7[1]");
    }
}
