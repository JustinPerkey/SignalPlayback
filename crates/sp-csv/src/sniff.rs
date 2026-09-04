//! The sniff pass (`docs/DESIGN.md` §7.4).
//!
//! Reads only the preamble, the two headers and the first group, so a
//! multi-gigabyte file previews instantly. Everything the mapping UI needs to
//! propose a column plan comes from here.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use sp_core::pulse::normalise_key;
use sp_core::DType;

use crate::control::ImportControl;
use crate::diag::Diagnostics;
use crate::error::{CsvError, Result};
use crate::framer::{ColumnData, Framer, GroupBlock};
use crate::parse;
use crate::profile::{ImportProfile, Layout};

/// Pulse rows the preview table shows.
pub const PREVIEW_ROWS: usize = 20;

/// What the sniff pass worked out about one column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnHint {
    /// Position in its header row.
    pub index: usize,
    /// Header text as written.
    pub label: String,
    /// The key it would be stored under with no rule of its own.
    pub key: String,
    /// Every sampled cell read as a number.
    pub numeric: bool,
    /// The narrowest storage the first group's values would fit — `i32` for a
    /// whole-number column — or `None` for a text column. It is shown in the
    /// mapping UI as an offer and never applied on its own: the sniff pass
    /// sees one group, and a later group may need the wider type (G1).
    pub narrowable_dtype: Option<DType>,
    /// The first few cells, as written.
    pub sample: Vec<String>,
}

impl ColumnHint {
    /// The first sampled cell, for the group-header preview.
    #[must_use]
    pub fn first(&self) -> &str {
        self.sample.first().map_or("", String::as_str)
    }
}

/// The preview a file's first group affords.
#[derive(Debug, Clone)]
pub struct Preview {
    /// The layout the current profile resolves to. This is what an import
    /// would use.
    pub layout: Layout,
    /// Every group-header column, including the count column.
    pub group_hints: Vec<ColumnHint>,
    /// Every pulse-header column, including the time column.
    pub pulse_hints: Vec<ColumnHint>,
    /// The first group's row values, as written: TOA first, then every pulse
    /// column in header order.
    pub rows: Vec<Vec<String>>,
    /// The first group's declared count, before any rows were read.
    pub declared_count: u32,
    /// Groups are not counted: the sniff pass stops after the first.
    pub first_group_rows: u32,
    pub diagnostics: Diagnostics,
}

impl Preview {
    /// The preview table's header, matching the order of [`Preview::rows`].
    #[must_use]
    pub fn row_header(&self) -> Vec<String> {
        self.pulse_hints
            .iter()
            .map(|hint| hint.label.clone())
            .collect()
    }

    /// A profile with the sniff pass's structural proposals applied: a column
    /// whose cells do not parse as numbers is marked as text.
    ///
    /// Storage width is deliberately left alone — see
    /// [`ColumnHint::narrowable_dtype`].
    #[must_use]
    pub fn proposed(&self, profile: &ImportProfile) -> ImportProfile {
        let mut proposed = profile.clone();
        for hint in &self.pulse_hints {
            if hint.index == self.layout.time_index {
                continue;
            }
            if hint.sample.is_empty() {
                // Nothing was sampled — the first group declared no rows — so
                // there is nothing to conclude. Leave the column as it is
                // rather than calling an unseen column text.
                continue;
            }
            let mut rule = profile
                .pulse_rule(&hint.label)
                .cloned()
                .unwrap_or_else(|| crate::profile::ColumnRule::new(hint.label.clone()));
            if hint.numeric {
                rule.text = None;
            } else {
                rule.text = Some(true);
                rule.dtype = None;
            }
            proposed.set_pulse_rule(rule);
        }
        proposed
    }
}

/// Previews a file on disk.
pub fn sniff_file(path: impl AsRef<Path>, profile: &ImportProfile) -> Result<Preview> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|source| CsvError::open(path, source))?;
    sniff(BufReader::new(file), profile)
}

/// Previews anything readable.
pub fn sniff<R: BufRead>(reader: R, profile: &ImportProfile) -> Result<Preview> {
    let mut framer = Framer::open_probing(reader, profile)?;
    let probe = framer.layout().clone();
    let control = ImportControl::new();
    let block = framer.next_group(&control)?;
    let diagnostics = framer.into_diagnostics();

    // Re-resolve against the caller's own profile: the probe forced every
    // column to text so the preview could show cells verbatim.
    let layout = profile.resolve(
        probe.delimiter,
        probe.preamble.clone(),
        probe.group_header.clone(),
        probe.pulse_header.clone(),
    )?;

    let group_hints = group_hints(&probe, block.as_ref());
    let pulse_hints = pulse_hints(&probe, block.as_ref(), profile.default_dtype);
    let rows = preview_rows(&probe, block.as_ref());

    Ok(Preview {
        layout,
        group_hints,
        pulse_hints,
        rows,
        declared_count: block.as_ref().map_or(0, |b| b.declared_count),
        first_group_rows: block.as_ref().map_or(0, GroupBlock::actual_count),
        diagnostics,
    })
}

fn group_hints(probe: &Layout, block: Option<&GroupBlock>) -> Vec<ColumnHint> {
    probe
        .group_header
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let sample: Vec<String> = block
                .and_then(|block| block.value(index))
                .map(|value| vec![value.to_owned()])
                .unwrap_or_default();
            let numeric = sample
                .iter()
                .all(|cell| parse::parse_number(cell).is_some());
            ColumnHint {
                index,
                label: label.clone(),
                key: normalise_key(label),
                numeric,
                narrowable_dtype: None,
                sample,
            }
        })
        .collect()
}

fn pulse_hints(probe: &Layout, block: Option<&GroupBlock>, default: DType) -> Vec<ColumnHint> {
    probe
        .pulse_header
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let sample = sampled_cells(probe, block, index);
            let numbers: Vec<Option<f64>> = sample
                .iter()
                .map(|cell| parse::parse_number(cell))
                .collect();
            let numeric = !sample.is_empty() && numbers.iter().all(Option::is_some);
            let values: Vec<f64> = numbers.into_iter().flatten().collect();
            ColumnHint {
                index,
                label: label.clone(),
                key: normalise_key(label),
                numeric,
                narrowable_dtype: numeric.then(|| {
                    if index == probe.time_index {
                        // The timebase is always stored at full precision.
                        DType::F64
                    } else {
                        crate::framer::narrowable_dtype(&values, default)
                    }
                }),
                sample,
            }
        })
        .collect()
}

/// The first [`PREVIEW_ROWS`] cells of one pulse column, as written. The probe
/// keeps the TOA column as numbers, so it is formatted back from those.
fn sampled_cells(probe: &Layout, block: Option<&GroupBlock>, index: usize) -> Vec<String> {
    let Some(block) = block else {
        return Vec::new();
    };
    let take = block.toa.len().min(PREVIEW_ROWS);
    if index == probe.time_index {
        return block.toa[..take].iter().map(|t| format_cell(*t)).collect();
    }
    let Some(position) = probe
        .pulse_stored
        .iter()
        .position(|column| column.index == index)
    else {
        return Vec::new();
    };
    match &block.columns[position] {
        ColumnData::Text(values) => values[..take.min(values.len())].to_vec(),
        ColumnData::Numbers(values) => values[..take.min(values.len())]
            .iter()
            .map(|v| format_cell(*v))
            .collect(),
    }
}

fn preview_rows(probe: &Layout, block: Option<&GroupBlock>) -> Vec<Vec<String>> {
    let Some(block) = block else {
        return Vec::new();
    };
    let columns: Vec<Vec<String>> = (0..probe.pulse_header.len())
        .map(|index| sampled_cells(probe, Some(block), index))
        .collect();
    let rows = columns.iter().map(Vec::len).min().unwrap_or(0);
    (0..rows)
        .map(|row| columns.iter().map(|cells| cells[row].clone()).collect())
        .collect()
}

/// A number as a CSV cell: no exponent for ordinary magnitudes, no trailing
/// zeros, and an empty cell for a missing value.
#[must_use]
pub fn format_cell(value: f64) -> String {
    if value.is_nan() {
        return String::new();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "inf"
        } else {
            "-inf"
        }
        .to_owned();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    // Rust's shortest round-tripping form, which is what G1 needs.
    format!("{value}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ColumnRule;

    const SAMPLE: &str = "\
Skip Row
groupID,total time, count, info
time, pulse width, power, angle
1, 1000, 2, info
10, 100, 100.5, 100
20, 100, 100.5, 100
2, 1000, 2, info
30, 100, 100.5, 100
40, 100, 100.5, 100
";

    fn preview(text: &str) -> Preview {
        sniff(text.as_bytes(), &ImportProfile::default()).unwrap()
    }

    #[test]
    fn the_sniff_reads_the_headers_and_only_the_first_group() {
        let preview = preview(SAMPLE);
        assert_eq!(preview.layout.preamble, ["Skip Row"]);
        assert_eq!(preview.declared_count, 2);
        assert_eq!(preview.first_group_rows, 2);
        assert_eq!(preview.rows.len(), 2);
        assert_eq!(preview.rows[0], ["10", "100", "100.5", "100"]);
        assert_eq!(
            preview.row_header(),
            ["time", "pulse width", "power", "angle"]
        );
    }

    #[test]
    fn every_column_gets_a_hint_including_the_count_and_time_columns() {
        let preview = preview(SAMPLE);
        assert_eq!(preview.group_hints.len(), 4);
        assert_eq!(preview.group_hints[2].label, "count");
        assert_eq!(preview.group_hints[2].first(), "2");
        assert!(!preview.group_hints[3].numeric);
        assert_eq!(preview.group_hints[3].key, "info");

        assert_eq!(preview.pulse_hints.len(), 4);
        assert!(preview.pulse_hints.iter().all(|hint| hint.numeric));
        assert_eq!(preview.pulse_hints[1].key, "pulse_width");
    }

    #[test]
    fn whole_number_columns_are_offered_narrow_storage_but_not_given_it() {
        let preview = preview(SAMPLE);
        // 'pulse width' and 'angle' are integers; 'power' is not.
        assert_eq!(preview.pulse_hints[1].narrowable_dtype, Some(DType::I32));
        assert_eq!(preview.pulse_hints[2].narrowable_dtype, Some(DType::F64));
        assert_eq!(preview.pulse_hints[3].narrowable_dtype, Some(DType::I32));
        // The timebase is never narrowed.
        assert_eq!(preview.pulse_hints[0].narrowable_dtype, Some(DType::F64));

        // The offer is not taken on the user's behalf: a later group may hold
        // a value that does not fit.
        let proposed = preview.proposed(&ImportProfile::default());
        let layout = proposed
            .resolve(
                ',',
                preview.layout.preamble.clone(),
                preview.layout.group_header.clone(),
                preview.layout.pulse_header.clone(),
            )
            .unwrap();
        assert!(layout.pulse_stored.iter().all(|c| c.dtype == DType::F64));
    }

    #[test]
    fn a_text_column_is_spotted_and_proposed_as_text() {
        let text = "\
Skip Row
gid, count
time, power, label
1, 2
10, 100, alpha
20, 200, beta
";
        let preview = preview(text);
        assert!(!preview.pulse_hints[2].numeric);
        assert_eq!(preview.pulse_hints[2].narrowable_dtype, None);
        assert_eq!(preview.rows[1], ["20", "200", "beta"]);

        let proposed = preview.proposed(&ImportProfile::default());
        assert_eq!(
            proposed.pulse_rule("label").map(ColumnRule::is_text),
            Some(true)
        );
        assert_eq!(
            proposed.pulse_rule("power").map(ColumnRule::is_text),
            Some(false)
        );
        assert_eq!(
            proposed.pulse_rule("power").and_then(|rule| rule.dtype),
            None
        );
    }

    #[test]
    fn the_preview_reflects_the_callers_profile_not_the_probe() {
        let mut profile = ImportProfile::default();
        profile.set_pulse_rule(ColumnRule::skipped("angle"));
        let preview = sniff(SAMPLE.as_bytes(), &profile).unwrap();
        // Dropped from what would be stored...
        assert!(preview
            .layout
            .pulse_stored
            .iter()
            .all(|column| column.label != "angle"));
        // ...but still shown in the preview so it can be put back.
        assert_eq!(preview.pulse_hints.len(), 4);
        assert_eq!(preview.rows[0].len(), 4);
    }

    #[test]
    fn a_file_that_is_not_this_format_fails_the_sniff_rather_than_the_import() {
        let text = "preamble\na,b,c\nd,e,f\n1,2,3\n";
        assert!(matches!(
            sniff(text.as_bytes(), &ImportProfile::default()),
            Err(CsvError::NoCountColumn { .. })
        ));
        // A file with nothing after the preamble cannot even be framed.
        assert!(matches!(
            sniff("just a preamble\n".as_bytes(), &ImportProfile::default()),
            Err(CsvError::Truncated { .. })
        ));
    }

    #[test]
    fn cells_format_without_exponents_or_trailing_zeros() {
        assert_eq!(format_cell(100.0), "100");
        assert_eq!(format_cell(-2.5), "-2.5");
        assert_eq!(format_cell(f64::NAN), "");
        assert_eq!(format_cell(0.0), "0");
        assert_eq!(format_cell(1e-7), "0.0000001");
    }
}
