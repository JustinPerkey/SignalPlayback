//! Reading a stored artifact payload back against its schema
//! (`docs/DESIGN.md` §10.1, §10.3).
//!
//! A viewer is handed JSON out of `artifact.payload_json` and an
//! [`ArtifactSchema`]; it never knows the Rust type that wrote it, because the
//! writing crate may not even be linked in. [`ArtifactData`] is the middle
//! term: a column per declared field, decoded once, with the row addressing
//! the table, the series chart and the overlay all need.
//!
//! The payload convention is **parallel arrays** — one entry per row — which
//! is what every built-in artifact writes. A field that carries a single value
//! (the `Scalars` view) decodes as a one-row column, so one code path serves
//! both.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{ArtifactSchema, FieldKind, FieldSpec};
use crate::time::TimeRange;

/// A payload that does not match the schema it was declared under.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    #[error("the payload is not valid JSON: {0}")]
    Malformed(String),

    #[error("the payload is a {found}, not an object of fields")]
    NotAnObject { found: &'static str },

    #[error("field '{field}' holds a {found} where the schema declares {expected}")]
    WrongType {
        field: String,
        expected: &'static str,
        found: &'static str,
    },
}

/// One decoded field of an artifact payload.
///
/// Numeric columns keep `None` for a null or non-finite entry rather than
/// substituting a zero: a missing measurement and a measured zero are not the
/// same thing, and a table that conflates them lies.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Float(Vec<Option<f64>>),
    Int(Vec<Option<i64>>),
    Bool(Vec<Option<bool>>),
    Text(Vec<String>),
    /// Instants on the absolute timeline, in seconds.
    Time(Vec<f64>),
    /// Spans on the absolute timeline.
    Span(Vec<TimeRange>),
    /// A row per outer entry, e.g. a spectrogram's rows of magnitudes.
    Matrix(Vec<Vec<f64>>),
}

impl Column {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Float(v) => v.len(),
            Self::Int(v) => v.len(),
            Self::Bool(v) => v.len(),
            Self::Text(v) => v.len(),
            Self::Time(v) => v.len(),
            Self::Span(v) => v.len(),
            Self::Matrix(v) => v.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The numeric value of one row, where the column has one. Text and
    /// matrix columns do not, which is what keeps them out of a numeric diff.
    #[must_use]
    pub fn number_at(&self, row: usize) -> Option<f64> {
        match self {
            Self::Float(v) => v.get(row).copied().flatten(),
            Self::Int(v) => v.get(row).copied().flatten().map(|i| i as f64),
            Self::Bool(v) => v
                .get(row)
                .copied()
                .flatten()
                .map(|b| if b { 1.0 } else { 0.0 }),
            Self::Time(v) => v.get(row).copied(),
            Self::Span(v) => v.get(row).map(|span| span.start_s),
            Self::Text(_) | Self::Matrix(_) => None,
        }
    }

    /// One row rendered for a table cell, formatted to `precision` decimals
    /// where the column is numeric.
    #[must_use]
    pub fn display_at(&self, row: usize, precision: Option<u8>) -> String {
        let float = |value: Option<f64>| match (value, precision) {
            (None, _) => "—".to_owned(),
            (Some(value), Some(places)) => format!("{value:.*}", places as usize),
            (Some(value), None) => format!("{value}"),
        };
        match self {
            Self::Float(v) => float(v.get(row).copied().flatten()),
            Self::Int(v) => v
                .get(row)
                .copied()
                .flatten()
                .map_or_else(|| "—".to_owned(), |i| i.to_string()),
            Self::Bool(v) => v.get(row).copied().flatten().map_or_else(
                || "—".to_owned(),
                |b| if b { "yes" } else { "no" }.to_owned(),
            ),
            Self::Text(v) => v.get(row).cloned().unwrap_or_default(),
            Self::Time(v) => float(v.get(row).copied()),
            Self::Span(v) => v.get(row).map_or_else(
                || "—".to_owned(),
                |span| {
                    let places = precision.unwrap_or(6) as usize;
                    format!("{:.*} – {:.*}", places, span.start_s, places, span.end_s)
                },
            ),
            Self::Matrix(v) => v
                .get(row)
                .map_or_else(|| "—".to_owned(), |row| format!("[{} values]", row.len())),
        }
    }

    /// Where this row sits on the timeline, for a temporal column.
    #[must_use]
    pub fn span_at(&self, row: usize) -> Option<TimeRange> {
        match self {
            Self::Time(v) => v.get(row).map(|t| TimeRange::new(*t, *t)),
            Self::Span(v) => v.get(row).copied(),
            _ => None,
        }
    }
}

/// A decoded artifact payload: the schema it was read against plus a column
/// per field that was present.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactData {
    schema: ArtifactSchema,
    columns: BTreeMap<String, Column>,
    rows: usize,
}

impl ArtifactData {
    /// Decodes `payload` — the JSON out of the artifact row — against
    /// `schema`.
    ///
    /// A field the schema declares but the payload omits is simply absent: a
    /// stage that emitted an older version of an artifact still displays,
    /// minus the fields it did not have. A field whose JSON contradicts its
    /// declared kind is an error, because drawing it would be a guess.
    pub fn decode(schema: ArtifactSchema, payload: &str) -> Result<Self, DataError> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| DataError::Malformed(error.to_string()))?;
        Self::from_value(schema, &value)
    }

    pub fn from_value(schema: ArtifactSchema, value: &Value) -> Result<Self, DataError> {
        let Some(object) = value.as_object() else {
            return Err(DataError::NotAnObject {
                found: type_name(value),
            });
        };

        let mut columns = BTreeMap::new();
        let mut rows = 0usize;
        for field in &schema.fields {
            let Some(raw) = object.get(&field.name) else {
                continue;
            };
            if raw.is_null() {
                continue;
            }
            let column = decode_field(field, raw)?;
            rows = rows.max(column.len());
            columns.insert(field.name.clone(), column);
        }

        Ok(Self {
            schema,
            columns,
            rows,
        })
    }

    #[must_use]
    pub fn schema(&self) -> &ArtifactSchema {
        &self.schema
    }

    /// The longest column, which is how many rows the table draws.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    #[must_use]
    pub fn column(&self, field: &str) -> Option<&Column> {
        self.columns.get(field)
    }

    /// The decoded columns in schema order, so a viewer lists fields the way
    /// the artifact declared them rather than alphabetically.
    pub fn fields(&self) -> impl Iterator<Item = (&FieldSpec, &Column)> {
        self.schema
            .fields
            .iter()
            .filter_map(|spec| Some((spec, self.columns.get(&spec.name)?)))
    }

    /// The first temporal column, which is what ties rows to the global
    /// playhead (§10.3).
    #[must_use]
    pub fn time_field(&self) -> Option<(&FieldSpec, &Column)> {
        self.fields().find(|(spec, _)| spec.kind.is_temporal())
    }

    /// The row the playhead is inside, or the last one that started before
    /// it. `None` when the artifact carries no time.
    #[must_use]
    pub fn row_at(&self, t_s: f64) -> Option<usize> {
        let (_, column) = self.time_field()?;
        let mut best: Option<(usize, f64)> = None;
        for row in 0..column.len() {
            let Some(span) = column.span_at(row) else {
                continue;
            };
            if span.start_s <= t_s && t_s <= span.end_s.max(span.start_s) {
                return Some(row);
            }
            if span.start_s <= t_s {
                let distance = t_s - span.start_s;
                if best.is_none_or(|(_, previous)| distance < previous) {
                    best = Some((row, distance));
                }
            }
        }
        best.map(|(row, _)| row)
    }

    /// Every row's timeline position, for the scope's overlay layer. Rows
    /// without one are skipped rather than placed at zero.
    #[must_use]
    pub fn spans(&self) -> Vec<(usize, TimeRange)> {
        let Some((_, column)) = self.time_field() else {
            return Vec::new();
        };
        (0..column.len())
            .filter_map(|row| Some((row, column.span_at(row)?)))
            .collect()
    }

    /// A numeric column to give an overlay its height — the first float field
    /// that is not the timeline itself, which is a score for detections and an
    /// amplitude for symbols.
    #[must_use]
    pub fn magnitude_field(&self) -> Option<&Column> {
        self.fields()
            .find(|(spec, _)| matches!(spec.kind, FieldKind::Float | FieldKind::Int))
            .map(|(_, column)| column)
    }

    /// A one-line label for a row, taken from the first text column, falling
    /// back to the row number.
    #[must_use]
    pub fn label_at(&self, row: usize) -> String {
        self.fields()
            .find(|(spec, _)| spec.kind == FieldKind::Text)
            .map_or_else(|| format!("#{row}"), |(_, c)| c.display_at(row, None))
    }
}

/// How one field of two artifacts of the same kind compare (§10.4).
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDiff {
    pub field: String,
    /// Rows in A and in B; they differ when a stage produced a different
    /// number of detections.
    pub rows: (usize, usize),
    /// Rows that differ by more than the tolerance, or that only one side has.
    pub mismatches: usize,
    /// The largest absolute difference over the rows both sides have.
    pub max_abs_error: f64,
    /// The first row that differs, which is the one worth looking at.
    pub first_divergence: Option<usize>,
}

impl FieldDiff {
    #[must_use]
    pub fn is_equal(&self) -> bool {
        self.mismatches == 0 && self.rows.0 == self.rows.1
    }
}

/// Compares two decoded artifacts field by field, within `tolerance`
/// (§10.4). Fields only one side declares are reported as wholly mismatched,
/// which is what a schema change looks like.
#[must_use]
pub fn diff(a: &ArtifactData, b: &ArtifactData, tolerance: f64) -> Vec<FieldDiff> {
    let mut names: Vec<&str> = a.schema.fields.iter().map(|f| f.name.as_str()).collect();
    for field in &b.schema.fields {
        if !names.contains(&field.name.as_str()) {
            names.push(field.name.as_str());
        }
    }

    names
        .into_iter()
        .filter_map(|name| {
            let left = a.column(name);
            let right = b.column(name);
            if left.is_none() && right.is_none() {
                return None;
            }
            let rows = (left.map_or(0, Column::len), right.map_or(0, Column::len));
            let mut diff = FieldDiff {
                field: name.to_owned(),
                rows,
                mismatches: rows.0.abs_diff(rows.1),
                max_abs_error: 0.0,
                first_divergence: (rows.0 != rows.1).then(|| rows.0.min(rows.1)),
            };
            if let (Some(left), Some(right)) = (left, right) {
                for row in 0..rows.0.min(rows.1) {
                    let differs = match (left.number_at(row), right.number_at(row)) {
                        (Some(x), Some(y)) => {
                            let error = (x - y).abs();
                            diff.max_abs_error = diff.max_abs_error.max(error);
                            error > tolerance
                        }
                        (None, None) => left.display_at(row, None) != right.display_at(row, None),
                        _ => true,
                    };
                    if differs {
                        diff.mismatches += 1;
                        diff.first_divergence =
                            Some(diff.first_divergence.map_or(row, |r| r.min(row)));
                    }
                }
            }
            Some(diff)
        })
        .collect()
}

fn decode_field(field: &FieldSpec, raw: &Value) -> Result<Column, DataError> {
    // A single value is a one-row column: that is what a `Scalars` artifact
    // writes, and it saves every viewer a special case.
    let entries: Vec<&Value> = match raw.as_array() {
        Some(array) => array.iter().collect(),
        None => vec![raw],
    };

    let wrong = |found: &'static str, expected: &'static str| DataError::WrongType {
        field: field.name.clone(),
        expected,
        found,
    };

    match field.kind {
        FieldKind::Float => Ok(Column::Float(
            entries.iter().map(|value| finite(value.as_f64())).collect(),
        )),
        FieldKind::Int => Ok(Column::Int(
            entries.iter().map(|value| value.as_i64()).collect(),
        )),
        FieldKind::Bool => Ok(Column::Bool(
            entries.iter().map(|value| value.as_bool()).collect(),
        )),
        FieldKind::Text => Ok(Column::Text(
            entries
                .iter()
                .map(|value| match value.as_str() {
                    Some(text) => text.to_owned(),
                    None => value.to_string(),
                })
                .collect(),
        )),
        FieldKind::TimeS => {
            let mut times = Vec::with_capacity(entries.len());
            for value in entries {
                times.push(
                    finite(value.as_f64())
                        .ok_or_else(|| wrong(type_name(value), "a time in seconds"))?,
                );
            }
            Ok(Column::Time(times))
        }
        FieldKind::SpanS => {
            let mut spans = Vec::with_capacity(entries.len());
            for value in entries {
                let pair = value
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .ok_or_else(|| wrong(type_name(value), "a [start, end] pair"))?;
                let (Some(start), Some(end)) = (finite(pair[0].as_f64()), finite(pair[1].as_f64()))
                else {
                    return Err(wrong(type_name(value), "a [start, end] pair"));
                };
                spans.push(TimeRange::new(start, end));
            }
            Ok(Column::Span(spans))
        }
        FieldKind::FloatArray => {
            // Either a flat run of values — a spectrum — or rows of them,
            // which is how a heatmap arrives.
            if entries.iter().any(|value| value.is_array()) {
                let mut matrix = Vec::with_capacity(entries.len());
                for value in entries {
                    let row = value
                        .as_array()
                        .ok_or_else(|| wrong(type_name(value), "rows of values"))?;
                    matrix.push(
                        row.iter()
                            .map(|cell| cell.as_f64().unwrap_or(f64::NAN))
                            .collect(),
                    );
                }
                Ok(Column::Matrix(matrix))
            } else {
                Ok(Column::Float(
                    entries.iter().map(|value| finite(value.as_f64())).collect(),
                ))
            }
        }
    }
}

/// Non-finite floats are treated as absent: JSON has no NaN, and a payload
/// that carries one through a string would poison an axis fit.
fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{ColumnSpec, OverlayForm, ViewHint};

    fn detections_schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("spans", FieldKind::SpanS),
                FieldSpec::new("scores", FieldKind::Float),
                FieldSpec::new("labels", FieldKind::Text),
            ],
            ViewHint::Overlay {
                form: OverlayForm::Spans,
            },
        )
    }

    fn detections() -> ArtifactData {
        ArtifactData::decode(
            detections_schema(),
            r#"{"spans":[[0.0,1.0],[2.0,2.5]],"scores":[0.4,0.9],"labels":["first","second"]}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_payload_decodes_into_one_column_per_declared_field() {
        let data = detections();
        assert_eq!(data.rows(), 2);
        assert_eq!(
            data.column("spans"),
            Some(&Column::Span(vec![
                TimeRange::new(0.0, 1.0),
                TimeRange::new(2.0, 2.5)
            ]))
        );
        assert_eq!(data.column("scores").unwrap().number_at(1), Some(0.9));
        assert_eq!(data.column("labels").unwrap().display_at(0, None), "first");
    }

    #[test]
    fn a_field_the_payload_omits_is_absent_rather_than_fatal() {
        // An artifact written before a field was added still displays.
        let data = ArtifactData::decode(detections_schema(), r#"{"spans":[[0.0,1.0]]}"#).unwrap();
        assert_eq!(data.rows(), 1);
        assert!(data.column("scores").is_none());
        assert_eq!(data.fields().count(), 1);
    }

    #[test]
    fn a_field_that_contradicts_its_kind_is_an_error() {
        let error = ArtifactData::decode(detections_schema(), r#"{"spans":[[0.0]]}"#).unwrap_err();
        assert_eq!(
            error.to_string(),
            "field 'spans' holds a array where the schema declares a [start, end] pair"
        );
        assert!(matches!(
            ArtifactData::decode(detections_schema(), "[]").unwrap_err(),
            DataError::NotAnObject { found: "array" }
        ));
        assert!(matches!(
            ArtifactData::decode(detections_schema(), "{oops"),
            Err(DataError::Malformed(_))
        ));
    }

    #[test]
    fn a_scalar_field_is_a_one_row_column() {
        let schema = ArtifactSchema::new(
            vec![
                FieldSpec::new("snr_db", FieldKind::Float),
                FieldSpec::new("passed", FieldKind::Bool),
            ],
            ViewHint::Scalars,
        );
        let data = ArtifactData::decode(schema, r#"{"snr_db":12.5,"passed":true}"#).unwrap();
        assert_eq!(data.rows(), 1);
        assert_eq!(data.column("snr_db").unwrap().number_at(0), Some(12.5));
        assert_eq!(data.column("passed").unwrap().display_at(0, None), "yes");
    }

    #[test]
    fn a_null_or_non_finite_number_reads_as_missing_not_as_zero() {
        let schema = ArtifactSchema::new(
            vec![FieldSpec::new("mean", FieldKind::Float)],
            ViewHint::Scalars,
        );
        let data = ArtifactData::decode(schema, r#"{"mean":[1.0,null,3.0]}"#).unwrap();
        assert_eq!(data.column("mean").unwrap().number_at(1), None);
        assert_eq!(data.column("mean").unwrap().display_at(1, Some(2)), "—");
        assert_eq!(data.column("mean").unwrap().display_at(2, Some(2)), "3.00");
    }

    #[test]
    fn rows_of_values_decode_as_a_matrix_for_a_heatmap() {
        let schema = ArtifactSchema::new(
            vec![FieldSpec::new("power", FieldKind::FloatArray)],
            ViewHint::Heatmap {
                rows: "power".into(),
                cols: "power".into(),
                values: "power".into(),
            },
        );
        let data = ArtifactData::decode(schema, r#"{"power":[[1.0,2.0],[3.0,4.0]]}"#).unwrap();
        assert_eq!(
            data.column("power"),
            Some(&Column::Matrix(vec![vec![1.0, 2.0], vec![3.0, 4.0]]))
        );
    }

    #[test]
    fn the_playhead_selects_the_row_it_is_inside() {
        let data = detections();
        assert_eq!(data.row_at(0.5), Some(0));
        assert_eq!(data.row_at(2.2), Some(1));
        // Between two spans, the last one that started.
        assert_eq!(data.row_at(1.5), Some(0));
        // Before anything happened, no row is current.
        assert_eq!(data.row_at(-1.0), None);
    }

    #[test]
    fn spans_are_what_the_overlay_layer_draws() {
        let data = detections();
        assert_eq!(
            data.spans(),
            vec![(0, TimeRange::new(0.0, 1.0)), (1, TimeRange::new(2.0, 2.5))]
        );
        assert_eq!(data.label_at(1), "second");
        assert_eq!(data.magnitude_field().unwrap().number_at(0), Some(0.4));
    }

    #[test]
    fn an_identical_pair_diffs_clean() {
        let diffs = diff(&detections(), &detections(), 0.0);
        assert_eq!(diffs.len(), 3);
        assert!(diffs.iter().all(FieldDiff::is_equal));
    }

    #[test]
    fn a_diff_reports_the_first_row_that_moved_and_by_how_much() {
        let changed = ArtifactData::decode(
            detections_schema(),
            r#"{"spans":[[0.0,1.0],[2.0,2.5]],"scores":[0.4,0.7],"labels":["first","second"]}"#,
        )
        .unwrap();
        let diffs = diff(&detections(), &changed, 1e-9);
        let scores = diffs.iter().find(|d| d.field == "scores").unwrap();
        assert_eq!(scores.first_divergence, Some(1));
        assert_eq!(scores.mismatches, 1);
        assert!((scores.max_abs_error - 0.2).abs() < 1e-12);
        assert!(!scores.is_equal());
        // Within tolerance the same pair is equal.
        let tolerant = diff(&detections(), &changed, 0.5);
        assert!(tolerant.iter().all(FieldDiff::is_equal));
    }

    #[test]
    fn a_diff_counts_the_rows_only_one_side_has() {
        let shorter = ArtifactData::decode(
            detections_schema(),
            r#"{"spans":[[0.0,1.0]],"scores":[0.4],"labels":["first"]}"#,
        )
        .unwrap();
        let diffs = diff(&detections(), &shorter, 0.0);
        let spans = diffs.iter().find(|d| d.field == "spans").unwrap();
        assert_eq!(spans.rows, (2, 1));
        assert_eq!(spans.mismatches, 1);
        assert_eq!(spans.first_divergence, Some(1));
    }

    #[test]
    fn a_table_cell_honours_the_column_precision() {
        let column = Column::Float(vec![Some(1.23456)]);
        assert_eq!(column.display_at(0, Some(2)), "1.23");
        assert_eq!(column.display_at(0, None), "1.23456");
        let _ = ColumnSpec::new("x", "X").with_precision(2);
    }
}
