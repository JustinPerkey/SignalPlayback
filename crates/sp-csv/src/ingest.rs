//! Streaming ingest into the library (`docs/DESIGN.md` §7.4).
//!
//! One file is one **signal train**, and the group blocks in it are that
//! train's segments (§6.6) — they are not independent captures, so every group
//! an import writes hangs off the one train it created.
//!
//! One pass over the file, appending to one column buffer per pulse field and
//! flushing each group's columns to a blob at the group boundary, so peak
//! memory is one group rather than one file. The whole import is one SQLite
//! transaction: a failure — or a cancellation — rolls back the rows *and* the
//! blobs, because both commit together (§14).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use sp_core::group::DatasetId;
use sp_core::props::PropertyValue;
use sp_core::TrainId;
use sp_core::{Attributes, DType, PropScope, PropertyDef, SourceKind};
use sp_store::{
    library, props, trains, Connection, NewDataset, NewPulseField, NewPulseGroup, NewTrain,
};

use crate::control::{ImportControl, ImportProgress};
use crate::diag::{Diagnostic, Diagnostics};
use crate::error::{CsvError, Result};
use crate::framer::{ColumnData, Framer, GroupBlock};
use crate::parse;
use crate::profile::{ImportProfile, Layout, StoredColumn};

/// The attribute prefix a text column's dictionary is stored under, on the
/// group that owns it: `csv_text.<key>` is the array of distinct cells, and
/// the column itself holds indices into it (§7.3).
pub const TEXT_DICTIONARY_PREFIX: &str = "csv_text.";

/// What to import, and how to describe it in the library.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub profile: ImportProfile,
    /// Dataset name. Empty falls back to the file's stem.
    pub name: String,
    /// The file the data came from, recorded on the dataset.
    pub source_uri: Option<String>,
    /// `import_profile.id`, when the import used a saved profile.
    pub profile_id: Option<i64>,
    pub notes: Option<String>,
}

impl ImportRequest {
    #[must_use]
    pub fn new(profile: ImportProfile) -> Self {
        Self {
            profile,
            name: String::new(),
            source_uri: None,
            profile_id: None,
            notes: None,
        }
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    #[must_use]
    pub fn from_file(mut self, path: &Path) -> Self {
        if self.name.is_empty() {
            self.name = path.file_stem().map_or_else(
                || "Imported CSV".to_owned(),
                |stem| stem.to_string_lossy().into_owned(),
            );
        }
        self.source_uri = Some(path.display().to_string());
        self
    }

    #[must_use]
    pub fn with_profile_id(mut self, profile_id: i64) -> Self {
        self.profile_id = Some(profile_id);
        self
    }
}

/// What an import did.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub dataset_id: DatasetId,
    /// The train the file became. One file is one train (§6.6).
    pub train_id: TrainId,
    pub name: String,
    pub groups: u32,
    pub pulses: u64,
    /// The layout the file was read with, as stored on the dataset.
    pub layout: Layout,
    pub diagnostics: Diagnostics,
}

impl ImportReport {
    /// A one-line summary for the status bar.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut summary = format!(
            "{}: {} group{}, {} pulse{}",
            self.name,
            self.groups,
            if self.groups == 1 { "" } else { "s" },
            self.pulses,
            if self.pulses == 1 { "" } else { "s" },
        );
        if !self.diagnostics.is_empty() {
            summary.push_str(&format!(
                ", {} diagnostic{}",
                self.diagnostics.total(),
                if self.diagnostics.total() == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        summary
    }
}

/// Imports a file on disk. The connection is the store's writer connection;
/// the transaction is opened and committed here.
pub fn import_file(
    conn: &mut Connection,
    path: impl AsRef<Path>,
    request: &ImportRequest,
    control: &ImportControl,
) -> Result<ImportReport> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|source| CsvError::open(path, source))?;
    let control = match file.metadata() {
        Ok(metadata) => control.clone().with_total_bytes(metadata.len()),
        Err(_) => control.clone(),
    };
    let request = if request.source_uri.is_none() || request.name.is_empty() {
        request.clone().from_file(path)
    } else {
        request.clone()
    };
    import_reader(conn, BufReader::new(file), &request, &control)
}

/// Imports anything readable, in one transaction.
pub fn import_reader<R: BufRead>(
    conn: &mut Connection,
    reader: R,
    request: &ImportRequest,
    control: &ImportControl,
) -> Result<ImportReport> {
    let mut framer = Framer::open(reader, &request.profile)?;
    let layout = framer.layout().clone();

    let name = if request.name.is_empty() {
        "Imported CSV".to_owned()
    } else {
        request.name.clone()
    };

    let tx = conn.transaction().map_err(sp_store::StoreError::from)?;

    // Group-scope definitions turn a mapped column into a typed value (§6.4).
    let defs = props::list_property_defs(&tx, Some(PropScope::Group))?;

    let mut attributes = Attributes::new();
    attributes.insert(Layout::ATTRIBUTE, serde_json::to_value(&layout)?);
    let mut dataset = NewDataset::new(&name, SourceKind::CsvImport);
    dataset.attributes = attributes;
    dataset.notes = request.notes.clone();
    if let Some(uri) = &request.source_uri {
        dataset.source_uri = Some(uri.clone());
    }
    let dataset_id = library::insert_dataset(&tx, &dataset)?;
    if let Some(profile_id) = request.profile_id {
        sp_store::profiles::link_dataset(&tx, dataset_id, profile_id)?;
    }

    // The file is the train; its blocks are segments of it.
    let train_id = trains::insert_train(
        &tx,
        &NewTrain::new(dataset_id, 0)
            .named(&name)
            .with_toa_unit(layout.time_unit),
    )?;

    let mut groups = 0u32;
    let mut pulses = 0u64;
    while let Some(block) = framer.next_group(control)? {
        let group = build_group(train_id, &layout, &block, &defs, framer.diagnostics_mut());
        sp_store::pulses::insert_pulse_group(&tx, &group)?;
        groups += 1;
        pulses += u64::from(block.actual_count());
        control.report(ImportProgress {
            bytes_read: framer.bytes_read(),
            total_bytes: None,
            groups,
            pulses,
            diagnostics: framer.diagnostics().total(),
        });
    }

    control.check()?;
    tx.commit().map_err(sp_store::StoreError::from)?;

    let report = ImportReport {
        dataset_id,
        train_id,
        name,
        groups,
        pulses,
        layout,
        diagnostics: framer.into_diagnostics(),
    };
    tracing::info!(
        dataset = dataset_id.get(),
        groups = report.groups,
        pulses = report.pulses,
        diagnostics = report.diagnostics.total(),
        "imported {}",
        report.name,
    );
    Ok(report)
}

/// Turns one framed block into the group the store writes.
fn build_group(
    train_id: TrainId,
    layout: &Layout,
    block: &GroupBlock,
    defs: &[PropertyDef],
    diagnostics: &mut Diagnostics,
) -> NewPulseGroup {
    let mut attributes = Attributes::new();
    for column in &layout.group_stored {
        let Some(raw) = block.value(column.index) else {
            continue;
        };
        attributes.insert(
            column.key.clone(),
            typed_value(raw, &column.key, defs, block, diagnostics),
        );
    }

    let name = layout
        .name_index
        .and_then(|index| block.value(index))
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let mut group = NewPulseGroup::new(train_id, block.index, block.toa_seconds(layout.time_unit));
    group.name = name;
    group.declared_count = block.declared_count;
    group.toa_unit = layout.time_unit;

    for (data, column) in block.columns.iter().zip(&layout.pulse_stored) {
        let field = match data {
            ColumnData::Numbers(values) => field_of(column, values.clone()),
            ColumnData::Text(values) => {
                let (codes, dictionary) = dictionary_encode(values);
                attributes.insert(
                    format!("{TEXT_DICTIONARY_PREFIX}{}", column.key),
                    PropertyValue::Array(
                        dictionary.into_iter().map(PropertyValue::String).collect(),
                    ),
                );
                // Codes index the dictionary, so the zone map on this column
                // orders by first appearance rather than by value.
                let mut field = field_of(column, codes);
                field.dtype = DType::I32;
                field
            }
        };
        group.fields.push(field);
    }

    group.attributes = attributes;
    group
}

fn field_of(column: &StoredColumn, values: Vec<f64>) -> NewPulseField {
    let mut field = NewPulseField::new(column.label.clone(), values).with_dtype(column.dtype);
    field.unit = column.unit.clone();
    field
}

/// Reads a group-header cell as the property it is bound to, falling back to
/// the plain reading — a number if it parses as one, text otherwise.
fn typed_value(
    raw: &str,
    key: &str,
    defs: &[PropertyDef],
    block: &GroupBlock,
    diagnostics: &mut Diagnostics,
) -> PropertyValue {
    if let Some(def) = defs.iter().find(|def| def.key == key) {
        match def.parse(raw) {
            Ok(value) => return value,
            Err(error) => diagnostics.push(
                Diagnostic::warning(
                    block.line,
                    0,
                    format!("property '{key}': {error}; stored as written"),
                )
                .in_group(block.index),
            ),
        }
    }
    if parse::is_missing(raw) {
        return PropertyValue::Null;
    }
    match parse::parse_number(raw) {
        Some(value) if value.is_finite() => PropertyValue::from(value),
        _ => PropertyValue::String(raw.to_owned()),
    }
}

/// Replaces a text column with indices into a dictionary of its distinct
/// cells, in first-appearance order.
fn dictionary_encode(values: &[String]) -> (Vec<f64>, Vec<String>) {
    let mut dictionary: Vec<String> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut codes = Vec::with_capacity(values.len());
    for value in values {
        let code = match seen.get(value.as_str()) {
            Some(code) => *code,
            None => {
                let code = dictionary.len();
                dictionary.push(value.clone());
                seen.insert(value.as_str(), code);
                code
            }
        };
        codes.push(code as f64);
    }
    (codes, dictionary)
}

/// The dictionary stored for a text column, if it has one.
#[must_use]
pub fn text_dictionary(attributes: &Attributes, key: &str) -> Option<Vec<String>> {
    let value = attributes.get(&format!("{TEXT_DICTIONARY_PREFIX}{key}"))?;
    let entries = value.as_array()?;
    Some(
        entries
            .iter()
            .map(|entry| entry.as_str().unwrap_or_default().to_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_column_becomes_codes_plus_a_dictionary() {
        let values = ["a", "b", "a", "c", "b"].map(str::to_owned).to_vec();
        let (codes, dictionary) = dictionary_encode(&values);
        assert_eq!(codes, [0.0, 1.0, 0.0, 2.0, 1.0]);
        assert_eq!(dictionary, ["a", "b", "c"]);
    }

    #[test]
    fn the_dictionary_round_trips_through_group_attributes() {
        let mut attributes = Attributes::new();
        attributes.insert(
            format!("{TEXT_DICTIONARY_PREFIX}label"),
            PropertyValue::Array(vec![
                PropertyValue::String("a".into()),
                PropertyValue::String("b".into()),
            ]),
        );
        assert_eq!(
            text_dictionary(&attributes, "label"),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(text_dictionary(&attributes, "missing"), None);
    }

    #[test]
    fn a_group_cell_reads_as_a_number_or_as_text() {
        let block = GroupBlock {
            index: 0,
            line: 1,
            group_values: Vec::new(),
            declared_count: 0,
            toa: Vec::new(),
            columns: Vec::new(),
        };
        let mut diagnostics = Diagnostics::new();
        assert_eq!(
            typed_value("1000", "total_time", &[], &block, &mut diagnostics),
            PropertyValue::from(1000.0)
        );
        assert_eq!(
            typed_value("info", "info", &[], &block, &mut diagnostics),
            PropertyValue::String("info".into())
        );
        assert_eq!(
            typed_value("", "info", &[], &block, &mut diagnostics),
            PropertyValue::Null
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn a_bound_column_is_read_as_its_property_kind() {
        use sp_core::PropKind;

        let defs = vec![PropertyDef::new(
            "channel",
            PropScope::Group,
            PropKind::Int {
                min: None,
                max: None,
            },
        )];
        let block = GroupBlock {
            index: 2,
            line: 9,
            group_values: Vec::new(),
            declared_count: 0,
            toa: Vec::new(),
            columns: Vec::new(),
        };
        let mut diagnostics = Diagnostics::new();
        assert_eq!(
            typed_value("7", "channel", &defs, &block, &mut diagnostics),
            PropertyValue::from(7)
        );
        assert!(diagnostics.is_empty());

        // A cell the definition refuses is kept, with the reason recorded.
        assert_eq!(
            typed_value("7.5", "channel", &defs, &block, &mut diagnostics),
            PropertyValue::from(7.5)
        );
        assert_eq!(diagnostics.total(), 1);
        assert_eq!(diagnostics.items()[0].group_index, Some(2));
    }
}
