//! Import profiles and the layout they resolve to (`docs/DESIGN.md` §7.3).
//!
//! Every framing constant is a profile setting rather than a literal, so a
//! file shape that differs from `sample/sample.csv` is a saved profile rather
//! than a code change (§18). A profile is serialised to JSON and stored in
//! `import_profile.rules_json`.

use std::fmt;

use serde::{Deserialize, Serialize};
use sp_core::pulse::normalise_key;
use sp_core::{DType, TimeUnit};

use crate::error::{CsvError, Result};
use crate::parse::{self, delimiter_label};

/// Header labels that name the count column, matched as normalised keys.
pub const COUNT_CANDIDATES: [&str; 5] = ["count", "pulse_count", "num_pulses", "n", "nrec"];

/// Header labels that name the time-of-arrival column.
pub const TIME_CANDIDATES: [&str; 4] = ["time", "toa", "time_of_arrival", "arrival_time"];

/// Header labels that make a good group name, in preference order.
pub const NAME_CANDIDATES: [&str; 5] = ["groupid", "group_id", "group", "id", "name"];

/// What to do when a group's declared count does not match what was read
/// (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountMode {
    /// Abort the import and roll it back.
    Strict,
    /// Record `actual_count` beside `declared_count`, resynchronise on the
    /// next row that parses as a group row, and warn.
    #[default]
    Tolerant,
}

impl CountMode {
    pub const ALL: [Self; 2] = [Self::Tolerant, Self::Strict];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Tolerant => "Tolerant",
        }
    }
}

impl fmt::Display for CountMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What the user decided about one column of one header row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnRule {
    /// The header label as written, e.g. `pulse width`.
    pub column: String,
    /// `false` drops the column: it is parsed and discarded.
    #[serde(default = "yes")]
    pub include: bool,
    /// The property key this column binds to. `None` derives it from the
    /// label, which is also how it matches a `PropertyDef` by convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Storage type; pulse columns only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtype: Option<DType>,
    /// Store this pulse column as text rather than numbers. The sniff pass
    /// proposes it for a column whose first-group cells do not parse (§7.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<bool>,
}

const fn yes() -> bool {
    true
}

impl ColumnRule {
    #[must_use]
    pub fn new(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            include: true,
            key: None,
            unit: None,
            dtype: None,
            text: None,
        }
    }

    #[must_use]
    pub fn skipped(column: impl Into<String>) -> Self {
        Self {
            include: false,
            ..Self::new(column)
        }
    }

    /// Binds the column to a property definition by key (§6.4).
    #[must_use]
    pub fn bound_to(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    #[must_use]
    pub fn with_dtype(mut self, dtype: DType) -> Self {
        self.dtype = Some(dtype);
        self
    }

    /// Keeps the column's cells as text (§7.3).
    #[must_use]
    pub fn as_text(mut self) -> Self {
        self.text = Some(true);
        self
    }

    /// Whether this column is stored as text.
    #[must_use]
    pub fn is_text(&self) -> bool {
        self.text.unwrap_or(false)
    }

    /// The key this column stores under.
    #[must_use]
    pub fn resolved_key(&self) -> String {
        self.key
            .clone()
            .unwrap_or_else(|| normalise_key(&self.column))
    }
}

/// Everything the parser needs that is not in the file itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportProfile {
    /// Shown in the profile picker; unique in `import_profile`.
    pub name: String,
    /// Lines skipped before the two header rows (§7.2).
    pub preamble_lines: usize,
    /// `None` auto-detects from the group header.
    pub delimiter: Option<char>,
    /// Lines starting with this are skipped everywhere outside a quoted field.
    pub comment_prefix: Option<char>,
    /// Overrides the count-column search.
    pub count_column: Option<String>,
    /// Overrides the time-column search.
    pub time_column: Option<String>,
    /// The unit the file's time column is written in.
    pub time_unit: TimeUnit,
    /// Group-header column used as the group's name.
    pub group_name_column: Option<String>,
    pub mode: CountMode,
    /// Storage type for pulse columns with no rule of their own.
    ///
    /// `f64` rather than `f32`, because G1 asks for a lossless round trip and
    /// a decimal that survives `f32` is the exception. Narrowing a column to
    /// `f32` halves its storage and is a per-column choice in the mapping UI.
    pub default_dtype: DType,
    pub group_columns: Vec<ColumnRule>,
    pub pulse_columns: Vec<ColumnRule>,
}

impl Default for ImportProfile {
    fn default() -> Self {
        Self {
            name: "Default".to_owned(),
            preamble_lines: 1,
            delimiter: None,
            comment_prefix: Some('#'),
            count_column: None,
            time_column: None,
            time_unit: TimeUnit::default(),
            group_name_column: None,
            mode: CountMode::default(),
            default_dtype: DType::F64,
            group_columns: Vec::new(),
            pulse_columns: Vec::new(),
        }
    }
}

impl ImportProfile {
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// The rule for a column, if the user set one.
    #[must_use]
    pub fn group_rule(&self, column: &str) -> Option<&ColumnRule> {
        find_rule(&self.group_columns, column)
    }

    #[must_use]
    pub fn pulse_rule(&self, column: &str) -> Option<&ColumnRule> {
        find_rule(&self.pulse_columns, column)
    }

    /// Replaces the rule for one group column, adding it if it is new.
    pub fn set_group_rule(&mut self, rule: ColumnRule) {
        set_rule(&mut self.group_columns, rule);
    }

    pub fn set_pulse_rule(&mut self, rule: ColumnRule) {
        set_rule(&mut self.pulse_columns, rule);
    }

    /// Reads a profile back out of `import_profile.rules_json`.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(CsvError::Json)
    }

    /// The JSON stored in `import_profile.rules_json`.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(CsvError::Json)
    }

    /// Resolves the profile against the two header rows read from a file.
    ///
    /// This is where "which column is the count" stops being a search and
    /// becomes an index; everything downstream works from the [`Layout`].
    pub fn resolve(
        &self,
        delimiter: char,
        preamble: Vec<String>,
        group_header: Vec<String>,
        pulse_header: Vec<String>,
    ) -> Result<Layout> {
        let count_index = match &self.count_column {
            Some(column) => {
                index_of(&group_header, column).ok_or_else(|| CsvError::UnknownColumn {
                    column: column.clone(),
                    which: "group",
                })?
            }
            None => find_candidate(&group_header, &COUNT_CANDIDATES).ok_or_else(|| {
                CsvError::NoCountColumn {
                    looked_for: COUNT_CANDIDATES.join(", "),
                }
            })?,
        };

        let time_index = match &self.time_column {
            Some(column) => {
                index_of(&pulse_header, column).ok_or_else(|| CsvError::UnknownColumn {
                    column: column.clone(),
                    which: "pulse",
                })?
            }
            None => find_candidate(&pulse_header, &TIME_CANDIDATES).ok_or_else(|| {
                CsvError::NoTimeColumn {
                    looked_for: TIME_CANDIDATES.join(", "),
                }
            })?,
        };

        let name_index = match &self.group_name_column {
            Some(column) => {
                Some(
                    index_of(&group_header, column).ok_or_else(|| CsvError::UnknownColumn {
                        column: column.clone(),
                        which: "group",
                    })?,
                )
            }
            None => find_candidate(&group_header, &NAME_CANDIDATES).filter(|i| *i != count_index),
        };

        let mut group_stored = Vec::new();
        for (index, label) in group_header.iter().enumerate() {
            if index == count_index {
                continue;
            }
            let rule = self.group_rule(label);
            if rule.is_some_and(|rule| !rule.include) {
                continue;
            }
            group_stored.push(StoredColumn {
                index,
                label: label.clone(),
                key: rule.map_or_else(|| normalise_key(label), ColumnRule::resolved_key),
                unit: rule.and_then(|rule| rule.unit.clone()),
                dtype: self.default_dtype,
                text: false,
            });
        }

        let mut pulse_stored = Vec::new();
        for (index, label) in pulse_header.iter().enumerate() {
            if index == time_index {
                continue;
            }
            let rule = self.pulse_rule(label);
            if rule.is_some_and(|rule| !rule.include) {
                continue;
            }
            pulse_stored.push(StoredColumn {
                index,
                label: label.clone(),
                key: rule.map_or_else(|| normalise_key(label), ColumnRule::resolved_key),
                unit: rule.and_then(|rule| rule.unit.clone()),
                dtype: rule
                    .and_then(|rule| rule.dtype)
                    .unwrap_or(self.default_dtype),
                text: rule.is_some_and(ColumnRule::is_text),
            });
        }

        if group_stored.iter().any(|c| c.key.is_empty())
            || pulse_stored.iter().any(|c| c.key.is_empty())
        {
            return Err(CsvError::profile(
                "a column has no usable key: give it a name in the mapping panel",
            ));
        }

        Ok(Layout {
            delimiter,
            comment_prefix: self.comment_prefix,
            preamble,
            group_header,
            pulse_header,
            count_index,
            time_index,
            name_index,
            time_unit: self.time_unit,
            mode: self.mode,
            group_stored,
            pulse_stored,
        })
    }
}

fn find_rule<'a>(rules: &'a [ColumnRule], column: &str) -> Option<&'a ColumnRule> {
    rules.iter().find(|rule| rule.column == column)
}

fn set_rule(rules: &mut Vec<ColumnRule>, rule: ColumnRule) {
    match rules.iter_mut().find(|r| r.column == rule.column) {
        Some(existing) => *existing = rule,
        None => rules.push(rule),
    }
}

fn index_of(header: &[String], column: &str) -> Option<usize> {
    header.iter().position(|label| label == column).or_else(|| {
        let wanted = normalise_key(column);
        header
            .iter()
            .position(|label| normalise_key(label) == wanted)
    })
}

fn find_candidate(header: &[String], candidates: &[&str]) -> Option<usize> {
    let keys: Vec<String> = header.iter().map(|label| normalise_key(label)).collect();
    candidates
        .iter()
        .find_map(|candidate| keys.iter().position(|key| key == candidate))
}

/// One column the import keeps, resolved to a position and a storage plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredColumn {
    /// Position in its header row.
    pub index: usize,
    /// Header text as written.
    pub label: String,
    /// Property key it is stored under.
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub dtype: DType,
    /// Cells are text: stored dictionary-encoded, with the dictionary on the
    /// group so export restores the original spelling (§7.3).
    #[serde(default)]
    pub text: bool,
}

/// A profile resolved against a specific file's headers.
///
/// This is also what export needs to reverse the grammar exactly (§7.5), so
/// it is stored on the dataset as JSON when an import commits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub delimiter: char,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_prefix: Option<char>,
    /// The skipped lines, verbatim, so export reproduces them (G1).
    pub preamble: Vec<String>,
    pub group_header: Vec<String>,
    pub pulse_header: Vec<String>,
    pub count_index: usize,
    pub time_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_index: Option<usize>,
    pub time_unit: TimeUnit,
    #[serde(default)]
    pub mode: CountMode,
    pub group_stored: Vec<StoredColumn>,
    pub pulse_stored: Vec<StoredColumn>,
}

impl Layout {
    /// The attribute key the layout is stored under on the dataset.
    pub const ATTRIBUTE: &'static str = "csv_layout";

    #[must_use]
    pub fn count_label(&self) -> &str {
        &self.group_header[self.count_index]
    }

    #[must_use]
    pub fn time_label(&self) -> &str {
        &self.pulse_header[self.time_index]
    }

    /// The group column stored under `key`, if any.
    #[must_use]
    pub fn group_column(&self, key: &str) -> Option<&StoredColumn> {
        self.group_stored.iter().find(|column| column.key == key)
    }

    /// A one-line description for the preview panel.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} delimiter · count '{}' · time '{}' in {} · {} preamble line{}",
            delimiter_label(self.delimiter),
            self.count_label(),
            self.time_label(),
            self.time_unit,
            self.preamble.len(),
            if self.preamble.len() == 1 { "" } else { "s" },
        )
    }

    /// Whether `line` should be ignored as a comment (§7.3).
    #[must_use]
    pub fn is_comment(&self, line: &str) -> bool {
        self.comment_prefix
            .is_some_and(|prefix| line.trim_start().starts_with(prefix))
    }

    /// Joins one output row, quoting only the fields that need it.
    #[must_use]
    pub fn join_row<'a>(&self, fields: impl IntoIterator<Item = &'a str>) -> String {
        fields
            .into_iter()
            .map(|field| parse::quote_field(field, self.delimiter))
            .collect::<Vec<_>>()
            .join(&self.delimiter.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> (Vec<String>, Vec<String>) {
        (
            ["groupID", "total time", "count", "info"]
                .map(str::to_owned)
                .to_vec(),
            ["time", "pulse width", "power", "angle"]
                .map(str::to_owned)
                .to_vec(),
        )
    }

    fn resolve(profile: &ImportProfile) -> Result<Layout> {
        let (group, pulse) = headers();
        profile.resolve(',', vec!["Skip Row".to_owned()], group, pulse)
    }

    #[test]
    fn the_sample_files_headers_resolve_without_any_settings() {
        let layout = resolve(&ImportProfile::default()).unwrap();
        assert_eq!(layout.count_index, 2);
        assert_eq!(layout.time_index, 0);
        // 'groupID' is the obvious name, and it is not the count column.
        assert_eq!(layout.name_index, Some(0));
        assert_eq!(layout.time_unit, TimeUnit::Microseconds);
        assert_eq!(
            layout
                .pulse_stored
                .iter()
                .map(|c| c.key.as_str())
                .collect::<Vec<_>>(),
            ["pulse_width", "power", "angle"]
        );
        assert_eq!(
            layout
                .group_stored
                .iter()
                .map(|c| c.key.as_str())
                .collect::<Vec<_>>(),
            ["groupid", "total_time", "info"]
        );
    }

    #[test]
    fn pulse_columns_default_to_f64_so_the_round_trip_is_lossless() {
        let layout = resolve(&ImportProfile::default()).unwrap();
        assert!(layout.pulse_stored.iter().all(|c| c.dtype == DType::F64));

        let mut profile = ImportProfile::default();
        profile.set_pulse_rule(ColumnRule::new("power").with_dtype(DType::F32));
        let layout = resolve(&profile).unwrap();
        let power = layout
            .pulse_stored
            .iter()
            .find(|c| c.label == "power")
            .unwrap();
        assert_eq!(power.dtype, DType::F32);
    }

    #[test]
    fn a_column_can_be_bound_to_a_property_definition_or_dropped() {
        let mut profile = ImportProfile::default();
        profile.set_pulse_rule(
            ColumnRule::new("pulse width")
                .bound_to("pw_s")
                .with_unit("s"),
        );
        profile.set_pulse_rule(ColumnRule::skipped("angle"));
        let layout = resolve(&profile).unwrap();

        let keys: Vec<&str> = layout.pulse_stored.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["pw_s", "power"]);
        assert_eq!(layout.pulse_stored[0].unit.as_deref(), Some("s"));
    }

    #[test]
    fn overrides_win_over_the_candidate_search() {
        let profile = ImportProfile {
            count_column: Some("total time".to_owned()),
            time_column: Some("angle".to_owned()),
            group_name_column: Some("info".to_owned()),
            ..ImportProfile::default()
        };
        let layout = resolve(&profile).unwrap();
        assert_eq!(layout.count_index, 1);
        assert_eq!(layout.time_index, 3);
        assert_eq!(layout.name_index, Some(3));
        // The count column is never also stored as a property.
        assert!(layout.group_column("total_time").is_none());
    }

    #[test]
    fn a_header_with_no_count_or_time_column_is_refused() {
        let profile = ImportProfile::default();
        let plain = ["a", "b"].map(str::to_owned).to_vec();
        assert!(matches!(
            profile.resolve(',', Vec::new(), plain.clone(), vec!["time".to_owned()]),
            Err(CsvError::NoCountColumn { .. })
        ));
        assert!(matches!(
            profile.resolve(',', Vec::new(), vec!["count".to_owned()], plain),
            Err(CsvError::NoTimeColumn { .. })
        ));
    }

    #[test]
    fn a_column_named_in_the_profile_but_not_in_the_file_is_refused() {
        let profile = ImportProfile {
            count_column: Some("nope".to_owned()),
            ..ImportProfile::default()
        };
        assert!(matches!(
            resolve(&profile),
            Err(CsvError::UnknownColumn { which: "group", .. })
        ));
    }

    #[test]
    fn profiles_round_trip_through_the_json_column() {
        let mut profile = ImportProfile::named("Radar A");
        profile.preamble_lines = 3;
        profile.delimiter = Some(';');
        profile.time_unit = TimeUnit::Nanoseconds;
        profile.mode = CountMode::Strict;
        profile.set_pulse_rule(
            ColumnRule::new("power")
                .with_dtype(DType::F32)
                .with_unit("W"),
        );

        let json = profile.to_json().unwrap();
        assert_eq!(ImportProfile::from_json(&json).unwrap(), profile);
    }

    #[test]
    fn an_older_profile_missing_newer_fields_still_loads() {
        let profile = ImportProfile::from_json(r#"{"name":"legacy","preamble_lines":2}"#).unwrap();
        assert_eq!(profile.name, "legacy");
        assert_eq!(profile.preamble_lines, 2);
        assert_eq!(profile.mode, CountMode::Tolerant);
        assert_eq!(profile.default_dtype, DType::F64);
    }

    #[test]
    fn the_layout_describes_itself_for_the_preview_panel() {
        let layout = resolve(&ImportProfile::default()).unwrap();
        assert_eq!(
            layout.describe(),
            "comma delimiter · count 'count' · time 'time' in us · 1 preamble line"
        );
    }

    #[test]
    fn output_rows_quote_only_what_needs_it() {
        let layout = resolve(&ImportProfile::default()).unwrap();
        assert_eq!(
            layout.join_row(["1", "1000", "2", "a,b"]),
            "1,1000,2,\"a,b\""
        );
    }

    #[test]
    fn comment_lines_are_recognised_by_the_configured_prefix() {
        let layout = resolve(&ImportProfile::default()).unwrap();
        assert!(layout.is_comment("# note"));
        assert!(layout.is_comment("   # indented"));
        assert!(!layout.is_comment("1, 1000, 2, info"));
    }
}
