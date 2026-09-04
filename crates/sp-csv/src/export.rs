//! Database → CSV (`docs/DESIGN.md` §7.5).
//!
//! The writer reverses the grammar exactly: the same preamble, the same two
//! header rows, the same column order, the same delimiter, and the time of
//! arrival rescaled to the unit the source file used. That is goal G1, and
//! `tests/roundtrip.rs` is what holds it to it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use sp_core::group::DatasetId;
use sp_core::props::PropertyValue;
use sp_core::{Attributes, GroupId, SampleRange, SignalGroup, TimeUnit};
use sp_store::{library, pulses, Connection};

use crate::error::{CsvError, Result};
use crate::ingest::text_dictionary;
use crate::profile::Layout;
use crate::sniff::format_cell;

/// Pulses read from the store per batch, so exporting a group of ten million
/// costs a few MB rather than a few hundred.
const STRIDE: u64 = 1 << 16;

/// The layout a dataset was imported with, from its attributes.
pub fn layout_of(conn: &Connection, dataset_id: DatasetId) -> Result<Layout> {
    let dataset = library::get_dataset(conn, dataset_id)?;
    let value = dataset
        .attributes
        .get(Layout::ATTRIBUTE)
        .ok_or(CsvError::NotImported)?;
    serde_json::from_value(value.clone()).map_err(CsvError::Json)
}

/// Writes a dataset back out in its source format.
pub fn export_dataset<W: Write>(
    conn: &Connection,
    dataset_id: DatasetId,
    out: &mut W,
) -> Result<()> {
    let layout = layout_of(conn, dataset_id)?;
    // Every group in the dataset, train by train: a file is one train, so an
    // export of a single imported dataset walks one (§6.6).
    let groups = library::list_groups_in_dataset(conn, dataset_id)?;

    for line in &layout.preamble {
        writeln!(out, "{line}").map_err(CsvError::Io)?;
    }
    writeln!(
        out,
        "{}",
        layout.join_row(layout.group_header.iter().map(String::as_str))
    )
    .map_err(CsvError::Io)?;
    writeln!(
        out,
        "{}",
        layout.join_row(layout.pulse_header.iter().map(String::as_str))
    )
    .map_err(CsvError::Io)?;

    for group in &groups {
        write_group(conn, &layout, group, out)?;
    }
    Ok(())
}

/// Writes a dataset to a file, creating or truncating it.
pub fn export_dataset_to_file(
    conn: &Connection,
    dataset_id: DatasetId,
    path: impl AsRef<Path>,
) -> Result<()> {
    let path = path.as_ref();
    let file = File::create(path).map_err(|source| CsvError::open(path, source))?;
    let mut out = BufWriter::new(file);
    export_dataset(conn, dataset_id, &mut out)?;
    out.flush().map_err(CsvError::Io)
}

fn write_group<W: Write>(
    conn: &Connection,
    layout: &Layout,
    group: &SignalGroup,
    out: &mut W,
) -> Result<()> {
    let count = group.actual_count;
    let row: Vec<String> = layout
        .group_header
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index == layout.count_index {
                return count.to_string();
            }
            layout
                .group_stored
                .iter()
                .find(|column| column.index == index)
                .map_or_else(String::new, |column| {
                    cell_of(group.attributes.get(&column.key))
                })
        })
        .collect();
    writeln!(out, "{}", layout.join_row(row.iter().map(String::as_str))).map_err(CsvError::Io)?;

    // The dictionary for every text column, once per group.
    let dictionaries: Vec<Option<Vec<String>>> = layout
        .pulse_stored
        .iter()
        .map(|column| {
            column
                .text
                .then(|| text_dictionary(&group.attributes, &column.key))
                .flatten()
        })
        .collect();

    let mut start = 0u64;
    while start < u64::from(count) {
        let range = SampleRange::new(start, (start + STRIDE).min(u64::from(count)));
        let toa = pulses::read_toa(conn, group.id, range)?;
        let columns = read_columns(conn, layout, group.id, range)?;

        for (i, time) in toa.iter().enumerate() {
            let row: Vec<String> = layout
                .pulse_header
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    if index == layout.time_index {
                        return format_time(*time, layout.time_unit);
                    }
                    let Some(position) = layout
                        .pulse_stored
                        .iter()
                        .position(|column| column.index == index)
                    else {
                        return String::new();
                    };
                    let value = columns[position].get(i).copied().unwrap_or(f64::NAN);
                    match &dictionaries[position] {
                        Some(dictionary) => {
                            dictionary.get(value as usize).cloned().unwrap_or_default()
                        }
                        None => format_cell(value),
                    }
                })
                .collect();
            writeln!(out, "{}", layout.join_row(row.iter().map(String::as_str)))
                .map_err(CsvError::Io)?;
        }
        start = range.end;
    }
    Ok(())
}

fn read_columns(
    conn: &Connection,
    layout: &Layout,
    group_id: GroupId,
    range: SampleRange,
) -> Result<Vec<Vec<f64>>> {
    layout
        .pulse_stored
        .iter()
        .enumerate()
        .map(|(ordinal, _)| {
            pulses::read_field(conn, group_id, ordinal as u32, range)
                .map(|buffer| buffer.to_f64())
                .map_err(CsvError::from)
        })
        .collect()
}

/// An attribute as the cell it came from.
fn cell_of(value: Option<&PropertyValue>) -> String {
    match value {
        None | Some(PropertyValue::Null) => String::new(),
        Some(PropertyValue::String(text)) => text.clone(),
        Some(PropertyValue::Bool(flag)) => flag.to_string(),
        Some(PropertyValue::Number(number)) => number
            .as_f64()
            .map_or_else(|| number.to_string(), format_cell),
        Some(other) => other.to_string(),
    }
}

/// The stored time of arrival, back in the file's own unit.
///
/// Scaling to seconds and back is not exactly reversible in binary floating
/// point, so the shortest decimal that *does* scale back to the stored value
/// is chosen: re-importing an exported file reproduces the same bits.
#[must_use]
pub fn format_time(seconds: f64, unit: TimeUnit) -> String {
    if seconds.is_nan() {
        return String::new();
    }
    let approx = unit.from_seconds(seconds);
    for decimals in 0..=17usize {
        let text = format!("{approx:.decimals$}");
        if text
            .parse::<f64>()
            .is_ok_and(|value| unit.to_seconds(value).to_bits() == seconds.to_bits())
        {
            return trim_zeros(text);
        }
    }
    format_cell(approx)
}

fn trim_zeros(text: String) -> String {
    if !text.contains('.') {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The attributes an exported group row is built from, for the inspector.
#[must_use]
pub fn group_row(layout: &Layout, attributes: &Attributes, count: u32) -> Vec<String> {
    layout
        .group_header
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index == layout.count_index {
                return count.to_string();
            }
            layout
                .group_stored
                .iter()
                .find(|column| column.index == index)
                .map_or_else(String::new, |column| cell_of(attributes.get(&column.key)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_of_arrival_survives_the_trip_to_seconds_and_back() {
        for unit in TimeUnit::ALL {
            for raw in [0.0, 10.0, 20.0, 12.5, 1234.5678, 1e9, -3.25] {
                let seconds = unit.to_seconds(raw);
                let text = format_time(seconds, unit);
                let parsed: f64 = text.parse().unwrap();
                assert_eq!(
                    unit.to_seconds(parsed).to_bits(),
                    seconds.to_bits(),
                    "{raw} {unit}: wrote '{text}'"
                );
            }
        }
    }

    #[test]
    fn the_shortest_form_is_chosen_not_the_naive_division() {
        // 10 us stored as seconds is not exactly 1e-5 in binary.
        let seconds = TimeUnit::Microseconds.to_seconds(10.0);
        assert_eq!(format_time(seconds, TimeUnit::Microseconds), "10");
        assert_eq!(
            format_time(
                TimeUnit::Microseconds.to_seconds(12.5),
                TimeUnit::Microseconds
            ),
            "12.5"
        );
    }

    #[test]
    fn a_missing_time_is_written_as_an_empty_cell() {
        assert_eq!(format_time(f64::NAN, TimeUnit::Microseconds), "");
    }

    #[test]
    fn attributes_are_written_back_as_the_cells_they_came_from() {
        assert_eq!(cell_of(None), "");
        assert_eq!(cell_of(Some(&PropertyValue::Null)), "");
        assert_eq!(cell_of(Some(&PropertyValue::String("info".into()))), "info");
        assert_eq!(cell_of(Some(&PropertyValue::from(1000.0))), "1000");
        assert_eq!(cell_of(Some(&PropertyValue::from(2.5))), "2.5");
        assert_eq!(cell_of(Some(&PropertyValue::Bool(true))), "true");
    }
}
