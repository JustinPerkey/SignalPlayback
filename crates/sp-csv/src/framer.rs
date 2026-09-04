//! Count-driven block framing (`docs/DESIGN.md` §7.2).
//!
//! A two-state machine — *expect group row* / *consume N pulse rows* — that
//! never looks ahead, so ingest streams at IO speed over a file with millions
//! of rows per group. Peak memory is one group: the framer materialises a
//! group's columns and hands them over, then starts the next group's.

use std::borrow::Cow;
use std::io::BufRead;

use sp_core::{DType, TimeUnit};

use crate::control::{ImportControl, ImportProgress, TICK_ROWS};
use crate::diag::{Diagnostic, Diagnostics};
use crate::error::{CsvError, Result};
use crate::parse::{self, Line, LineReader};
use crate::profile::{ColumnRule, ImportProfile, Layout};

/// One pulse column's values, as the file wrote them.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    /// Numbers, with `NaN` for a missing cell.
    Numbers(Vec<f64>),
    /// Cells kept verbatim, for a column the profile marked as text.
    Text(Vec<String>),
}

impl ColumnData {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Numbers(values) => values.len(),
            Self::Text(values) => values.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The cell at `index` as it will be written back out (§7.5).
    #[must_use]
    pub fn cell(&self, index: usize) -> Option<&str> {
        match self {
            Self::Numbers(_) => None,
            Self::Text(values) => values.get(index).map(String::as_str),
        }
    }
}

/// One framed group: its header row and its pulse rows, already in columns.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupBlock {
    /// Block position in the file, 0-based.
    pub index: u32,
    /// Line the group row was on.
    pub line: u64,
    /// Every group-header value, in header order, as written. Kept whole so
    /// export can reproduce the row even for columns the import dropped.
    pub group_values: Vec<String>,
    /// The count field of the group row.
    pub declared_count: u32,
    /// Times of arrival in the file's own unit, one per row read.
    pub toa: Vec<f64>,
    /// One entry per [`Layout::pulse_stored`] column, in that order.
    pub columns: Vec<ColumnData>,
}

impl GroupBlock {
    /// Pulse rows actually read.
    #[must_use]
    pub fn actual_count(&self) -> u32 {
        self.toa.len() as u32
    }

    /// Whether the block ended where its count said it would.
    #[must_use]
    pub fn count_matches(&self) -> bool {
        self.declared_count == self.actual_count()
    }

    /// A group-header value by position, for the name and for attributes.
    #[must_use]
    pub fn value(&self, index: usize) -> Option<&str> {
        self.group_values.get(index).map(String::as_str)
    }

    /// TOA converted to the library's absolute timeline.
    #[must_use]
    pub fn toa_seconds(&self, unit: TimeUnit) -> Vec<f64> {
        self.toa.iter().map(|t| unit.to_seconds(*t)).collect()
    }
}

/// Reads a file's preamble and headers, then yields one [`GroupBlock`] at a
/// time.
#[derive(Debug)]
pub struct Framer<R> {
    lines: LineReader<R>,
    layout: Layout,
    diagnostics: Diagnostics,
    /// A line read while looking for the end of a group, to be re-examined as
    /// the next group row (tolerant resynchronisation, §7.3).
    pending: Option<Line>,
    next_index: u32,
    pulses: u64,
    finished: bool,
}

impl<R: BufRead> Framer<R> {
    /// Consumes the preamble and both header rows, and resolves `profile`
    /// against them.
    pub fn open(reader: R, profile: &ImportProfile) -> Result<Self> {
        Self::open_inner(reader, profile, false)
    }

    /// Opens for the sniff pass (§7.4): every pulse column is kept, and kept
    /// as text, so the preview shows cells as written and the mapping UI can
    /// decide for itself which columns are numeric.
    pub fn open_probing(reader: R, profile: &ImportProfile) -> Result<Self> {
        Self::open_inner(reader, profile, true)
    }

    fn open_inner(reader: R, profile: &ImportProfile, probe: bool) -> Result<Self> {
        let mut lines = LineReader::new(reader);
        let mut diagnostics = Diagnostics::new();
        let mut raised = Vec::new();

        let mut preamble = Vec::with_capacity(profile.preamble_lines);
        while preamble.len() < profile.preamble_lines {
            match lines.next_line(&mut raised)? {
                Some(line) => preamble.push(line.text),
                None => {
                    return Err(CsvError::Truncated {
                        expected: "the preamble and two header rows",
                        found: preamble.len(),
                    })
                }
            }
        }

        // Comments are skipped everywhere, the header rows included.
        let comment_prefix = profile.comment_prefix;
        let is_comment =
            |text: &str| comment_prefix.is_some_and(|prefix| text.trim_start().starts_with(prefix));

        let mut header_lines = Vec::with_capacity(2);
        while header_lines.len() < 2 {
            match lines.next_line(&mut raised)? {
                Some(line) if is_comment(&line.text) || line.text.trim().is_empty() => continue,
                Some(line) => header_lines.push(line),
                None => {
                    return Err(CsvError::Truncated {
                        expected: "two header rows",
                        found: header_lines.len(),
                    })
                }
            }
        }

        let delimiter = profile
            .delimiter
            .unwrap_or_else(|| parse::detect_delimiter(&header_lines[0].text));
        let group_header = labels(&header_lines[0].text, delimiter);
        let pulse_header = labels(&header_lines[1].text, delimiter);

        let profile = if probe {
            let mut probing = profile.clone();
            for label in group_header.iter().chain(&pulse_header) {
                probing.set_group_rule(ColumnRule::new(label.clone()));
                probing.set_pulse_rule(ColumnRule::new(label.clone()).as_text());
            }
            Cow::Owned(probing)
        } else {
            Cow::Borrowed(profile)
        };
        let layout = profile.resolve(delimiter, preamble, group_header, pulse_header)?;

        for diagnostic in raised {
            diagnostics.push(diagnostic);
        }

        Ok(Self {
            lines,
            layout,
            diagnostics,
            pending: None,
            next_index: 0,
            pulses: 0,
            finished: false,
        })
    }

    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// The diagnostic list, so a caller building on the framer's output adds
    /// to the same collection the parser used.
    pub fn diagnostics_mut(&mut self) -> &mut Diagnostics {
        &mut self.diagnostics
    }

    #[must_use]
    pub fn into_diagnostics(self) -> Diagnostics {
        self.diagnostics
    }

    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.lines.bytes_read()
    }

    /// Progress as of this moment.
    #[must_use]
    pub fn progress(&self) -> ImportProgress {
        ImportProgress {
            bytes_read: self.bytes_read(),
            total_bytes: None,
            groups: self.next_index,
            pulses: self.pulses,
            diagnostics: self.diagnostics.total(),
        }
    }

    /// The next group, or `None` at end of file.
    pub fn next_group(&mut self, control: &ImportControl) -> Result<Option<GroupBlock>> {
        if self.finished {
            return Ok(None);
        }
        control.check()?;

        let Some(group_line) = self.next_group_row()? else {
            self.finished = true;
            return Ok(None);
        };

        let index = self.next_index;
        self.next_index += 1;
        let fields = parse::split_fields(&group_line.text, self.layout.delimiter);
        let arity = self.layout.group_header.len();
        if fields.len() != arity {
            self.raise(
                Diagnostic::warning(
                    group_line.number,
                    group_line.byte_offset,
                    format!(
                        "the group row has {} fields but the group header has {arity}",
                        fields.len()
                    ),
                )
                .in_group(index),
            )?;
        }
        let mut group_values: Vec<String> = fields.iter().map(|f| f.text.clone()).collect();
        group_values.resize(arity, String::new());

        let declared_count = match parse::parse_number(&group_values[self.layout.count_index]) {
            Some(value) if value >= 0.0 && value.is_finite() => value as u32,
            _ => {
                self.raise(
                    Diagnostic::warning(
                        group_line.number,
                        group_line.byte_offset,
                        format!(
                            "the count field '{}' is not a row count; reading to the next group",
                            group_values[self.layout.count_index]
                        ),
                    )
                    .in_group(index)
                    .at_column(self.layout.count_index),
                )?;
                u32::MAX
            }
        };

        let mut block = GroupBlock {
            index,
            line: group_line.number,
            group_values,
            declared_count,
            toa: Vec::new(),
            columns: self
                .layout
                .pulse_stored
                .iter()
                .map(|column| {
                    if column.text {
                        ColumnData::Text(Vec::new())
                    } else {
                        ColumnData::Numbers(Vec::new())
                    }
                })
                .collect(),
        };

        self.read_rows(&mut block, control)?;
        self.pulses += u64::from(block.actual_count());

        if !block.count_matches() && block.declared_count != u32::MAX {
            self.raise(
                Diagnostic::warning(
                    group_line.number,
                    group_line.byte_offset,
                    format!(
                        "group declared {} pulse rows but {} were read",
                        block.declared_count,
                        block.actual_count()
                    ),
                )
                .in_group(index),
            )?;
        }
        if block.declared_count == u32::MAX {
            block.declared_count = block.actual_count();
        }

        control.report(self.progress());
        Ok(Some(block))
    }

    /// Consumes this block's pulse rows, stopping at the declared count or at
    /// the first row that cannot be one.
    fn read_rows(&mut self, block: &mut GroupBlock, control: &ImportControl) -> Result<()> {
        let arity = self.layout.pulse_header.len();
        while block.actual_count() < block.declared_count {
            if u64::from(block.actual_count()) % TICK_ROWS == 0 {
                control.check()?;
                control.report(ImportProgress {
                    bytes_read: self.lines.bytes_read(),
                    total_bytes: None,
                    groups: self.next_index.saturating_sub(1),
                    pulses: self.pulses + u64::from(block.actual_count()),
                    diagnostics: self.diagnostics.total(),
                });
            }

            let Some(line) = self.next_data_line()? else {
                if block.declared_count != u32::MAX {
                    self.raise(
                        Diagnostic::warning(
                            block.line,
                            0,
                            format!(
                                "the file ended after {} of the {} rows this group declared",
                                block.actual_count(),
                                block.declared_count
                            ),
                        )
                        .in_group(block.index),
                    )?;
                }
                return Ok(());
            };

            let fields = parse::split_fields(&line.text, self.layout.delimiter);
            if fields.len() != arity {
                // The block ended early: this row is not one of its pulses.
                // Hand it back so it is examined as the next group row.
                if block.declared_count != u32::MAX {
                    self.raise(
                        Diagnostic::warning(
                            line.number,
                            line.byte_offset,
                            format!(
                                "expected a pulse row of {arity} fields but found {}; \
                                 resynchronising on the next group row",
                                fields.len()
                            ),
                        )
                        .in_group(block.index),
                    )?;
                }
                self.pending = Some(line);
                return Ok(());
            }

            let toa_text = &fields[self.layout.time_index].text;
            let toa = match parse::parse_number(toa_text) {
                Some(value) => value,
                None => {
                    self.raise(
                        Diagnostic::warning(
                            line.number,
                            line.byte_offset + fields[self.layout.time_index].offset as u64,
                            format!("'{toa_text}' is not a time of arrival"),
                        )
                        .in_group(block.index)
                        .at_column(self.layout.time_index),
                    )?;
                    f64::NAN
                }
            };
            block.toa.push(toa);

            for (column, stored) in block.columns.iter_mut().zip(&self.layout.pulse_stored) {
                let field = &fields[stored.index];
                match column {
                    ColumnData::Text(values) => values.push(field.text.clone()),
                    ColumnData::Numbers(values) => match parse::parse_number(&field.text) {
                        Some(value) => values.push(value),
                        None => {
                            let diagnostic = Diagnostic::warning(
                                line.number,
                                line.byte_offset + field.offset as u64,
                                format!(
                                    "'{}' in column '{}' is not a number; stored as missing",
                                    field.text, stored.label
                                ),
                            )
                            .in_group(block.index)
                            .at_column(stored.index);
                            self.diagnostics.push(diagnostic);
                            values.push(f64::NAN);
                        }
                    },
                }
            }
        }
        Ok(())
    }

    /// The next line that could be a group row: the one handed back by a
    /// resynchronisation, or the next line of the file.
    fn next_group_row(&mut self) -> Result<Option<Line>> {
        self.next_data_line()
    }

    fn next_data_line(&mut self) -> Result<Option<Line>> {
        if let Some(line) = self.pending.take() {
            return Ok(Some(line));
        }
        let mut raised = Vec::new();
        let result = loop {
            match self.lines.next_line(&mut raised)? {
                Some(line) if line.text.trim().is_empty() => continue,
                Some(line) if self.layout.is_comment(&line.text) => continue,
                other => break other,
            }
        };
        for diagnostic in raised {
            self.diagnostics.push(diagnostic);
        }
        Ok(result)
    }

    /// Records a diagnostic, promoting it to a hard error in strict mode.
    fn raise(&mut self, diagnostic: Diagnostic) -> Result<()> {
        if self.layout.mode == crate::profile::CountMode::Strict {
            return Err(CsvError::Strict(diagnostic.to_string()));
        }
        self.diagnostics.push(diagnostic);
        Ok(())
    }
}

fn labels(line: &str, delimiter: char) -> Vec<String> {
    parse::split_fields(line, delimiter)
        .into_iter()
        .map(|field| field.text)
        .collect()
}

/// The narrowest storage a column of these values would fit, for the mapping
/// UI to offer.
#[must_use]
pub fn narrowable_dtype(values: &[f64], default: DType) -> DType {
    if values
        .iter()
        .all(|v| !v.is_finite() || (v.fract() == 0.0 && v.abs() <= f64::from(i32::MAX)))
    {
        DType::I32
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{ColumnRule, CountMode};

    const SAMPLE: &str = "\
Skip Row\r
groupID,total time, count, info\r
time, pulse width, power, angle\r
1, 1000, 2, info\r
10, 100, 100, 100\r
20, 100, 100, 100\r
2, 1000, 2, info\r
30, 100, 100, 100\r
40, 100, 100, 100
";

    fn frame(
        text: &str,
        profile: &ImportProfile,
    ) -> Result<(Vec<GroupBlock>, Diagnostics, Layout)> {
        let mut framer = Framer::open(text.as_bytes(), profile)?;
        let control = ImportControl::new();
        let mut blocks = Vec::new();
        while let Some(block) = framer.next_group(&control)? {
            blocks.push(block);
        }
        let layout = framer.layout().clone();
        Ok((blocks, framer.into_diagnostics(), layout))
    }

    fn numbers(block: &GroupBlock, column: usize) -> Vec<f64> {
        match &block.columns[column] {
            ColumnData::Numbers(values) => values.clone(),
            ColumnData::Text(_) => panic!("column {column} is text"),
        }
    }

    #[test]
    fn the_sample_file_frames_into_two_groups_of_two_pulses() {
        let (blocks, diagnostics, layout) = frame(SAMPLE, &ImportProfile::default()).unwrap();
        assert!(diagnostics.is_empty(), "{:?}", diagnostics.items());
        assert_eq!(layout.preamble, ["Skip Row"]);
        assert_eq!(blocks.len(), 2);

        assert_eq!(blocks[0].index, 0);
        assert_eq!(blocks[0].line, 4);
        assert_eq!(blocks[0].declared_count, 2);
        assert_eq!(blocks[0].actual_count(), 2);
        assert!(blocks[0].count_matches());
        assert_eq!(blocks[0].group_values, ["1", "1000", "2", "info"]);
        assert_eq!(blocks[0].toa, [10.0, 20.0]);
        assert_eq!(numbers(&blocks[0], 0), [100.0, 100.0]);
        assert_eq!(blocks[1].toa, [30.0, 40.0]);
    }

    #[test]
    fn there_is_no_separator_the_count_alone_ends_a_block() {
        // Both headers have four columns, so only the count can say where a
        // block stops.
        let (blocks, _, _) = frame(SAMPLE, &ImportProfile::default()).unwrap();
        assert_eq!(blocks[0].toa.len(), 2);
        assert_eq!(blocks[1].group_values[0], "2");
    }

    #[test]
    fn toa_is_scaled_to_seconds_on_the_way_in() {
        let (blocks, _, layout) = frame(SAMPLE, &ImportProfile::default()).unwrap();
        let seconds = blocks[0].toa_seconds(layout.time_unit);
        assert!((seconds[0] - 10e-6).abs() < 1e-18);
        assert!((seconds[1] - 20e-6).abs() < 1e-18);
    }

    #[test]
    fn a_row_matching_the_group_header_mid_file_is_data_not_a_header() {
        let text = "\
Skip Row
groupID,total time, count, info
time, pulse width, power, angle
1, 1000, 3, info
groupID,total time, count, info
10, 100, 100, 100
20, 100, 100, 100
";
        let (blocks, diagnostics, _) = frame(text, &ImportProfile::default()).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].actual_count(), 3);
        // The repeated header line was read as a pulse row, and its unreadable
        // cells were reported rather than skipped.
        assert!(blocks[0].toa[0].is_nan());
        assert!(diagnostics.total() >= 2);
    }

    #[test]
    fn a_short_block_warns_and_resynchronises_on_the_next_group() {
        let text = "\
Skip Row
groupID,total time, count, info
time, pulse width, power, angle
1, 1000, 5, info
10, 100, 100, 100
2, 1000, 1, info
30, 100, 100, 100
";
        // Every row here has four fields, so the framer only learns the block
        // was short when the file runs out.
        let (blocks, diagnostics, _) = frame(text, &ImportProfile::default()).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].declared_count, 5);
        assert_eq!(blocks[0].actual_count(), 3);
        assert!(!blocks[0].count_matches());
        assert!(diagnostics
            .items()
            .iter()
            .any(|d| d.message.contains("declared 5")));
    }

    #[test]
    fn a_row_of_the_wrong_arity_ends_the_block_and_is_read_as_the_next_group() {
        let text = "\
Skip Row
gid, count
time, power, angle
1, 4
10, 100, 5
2, 1
30, 100, 5
";
        let (blocks, diagnostics, _) = frame(text, &ImportProfile::default()).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].actual_count(), 1);
        assert_eq!(blocks[1].group_values, ["2", "1"]);
        assert_eq!(blocks[1].actual_count(), 1);
        assert!(diagnostics
            .items()
            .iter()
            .any(|d| d.message.contains("resynchronising") || d.message.contains("declared 4")));
    }

    #[test]
    fn strict_mode_refuses_what_tolerant_mode_warns_about() {
        let text = "\
Skip Row
groupID,total time, count, info
time, pulse width, power, angle
1, 1000, 5, info
10, 100, 100, 100
";
        let profile = ImportProfile {
            mode: CountMode::Strict,
            ..ImportProfile::default()
        };
        assert!(matches!(frame(text, &profile), Err(CsvError::Strict(_))));
        // The same file imports in tolerant mode.
        assert!(frame(text, &ImportProfile::default()).is_ok());
    }

    #[test]
    fn comments_and_blank_lines_are_skipped_everywhere() {
        let text = "\
Skip Row
# a note
groupID, count
time, power

# another
1, 2
10, 100

20, 200
";
        let (blocks, _, _) = frame(text, &ImportProfile::default()).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].toa, [10.0, 20.0]);
        assert_eq!(numbers(&blocks[0], 0), [100.0, 200.0]);
    }

    #[test]
    fn a_missing_cell_is_stored_as_a_gap() {
        let text = "\
Skip Row
gid, count
time, power
1, 2
10,
20, NA
";
        let (blocks, diagnostics, _) = frame(text, &ImportProfile::default()).unwrap();
        assert!(diagnostics.is_empty(), "{:?}", diagnostics.items());
        assert!(numbers(&blocks[0], 0).iter().all(|v| v.is_nan()));
    }

    #[test]
    fn an_unreadable_cell_is_a_diagnostic_not_an_abort() {
        let text = "\
Skip Row
gid, count
time, power
1, 2
10, 100
20, oops
";
        let (blocks, diagnostics, _) = frame(text, &ImportProfile::default()).unwrap();
        assert_eq!(blocks[0].actual_count(), 2);
        assert!(numbers(&blocks[0], 0)[1].is_nan());
        let diagnostic = &diagnostics.items()[0];
        assert_eq!(diagnostic.line, 6);
        assert_eq!(diagnostic.column_index, Some(1));
        assert_eq!(diagnostic.group_index, Some(0));
        assert!(diagnostic.message.contains("oops"));
    }

    #[test]
    fn a_column_marked_as_text_keeps_its_cells_verbatim() {
        let text = "\
Skip Row
gid, count
time, label
1, 2
10, alpha
20, beta
";
        let mut profile = ImportProfile::default();
        profile.set_pulse_rule(ColumnRule::new("label").as_text());
        let (blocks, diagnostics, _) = frame(text, &profile).unwrap();
        assert!(diagnostics.is_empty(), "{:?}", diagnostics.items());
        assert_eq!(
            blocks[0].columns[0],
            ColumnData::Text(vec!["alpha".to_owned(), "beta".to_owned()])
        );
        assert_eq!(blocks[0].columns[0].cell(1), Some("beta"));
    }

    #[test]
    fn a_file_too_short_for_its_headers_is_refused() {
        assert!(matches!(
            frame("Skip Row\nonly, one, header\n", &ImportProfile::default()),
            Err(CsvError::Truncated { .. })
        ));
        assert!(matches!(
            frame("", &ImportProfile::default()),
            Err(CsvError::Truncated { .. })
        ));
    }

    #[test]
    fn cancellation_stops_the_framer_between_groups() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let flag = Arc::new(AtomicBool::new(false));
        let control = ImportControl::new().with_cancel(flag.clone());
        let mut framer = Framer::open(SAMPLE.as_bytes(), &ImportProfile::default()).unwrap();
        assert!(framer.next_group(&control).unwrap().is_some());
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(
            framer.next_group(&control),
            Err(CsvError::Cancelled)
        ));
    }

    #[test]
    fn an_integer_column_is_offered_narrow_storage() {
        assert_eq!(narrowable_dtype(&[1.0, 2.0, 3.0], DType::F64), DType::I32);
        assert_eq!(narrowable_dtype(&[1.5, 2.0], DType::F64), DType::F64);
        assert_eq!(narrowable_dtype(&[1e12], DType::F64), DType::F64);
    }
}
