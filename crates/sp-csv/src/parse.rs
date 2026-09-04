//! Line and field decoding with source positions (`docs/DESIGN.md` §7.3).
//!
//! Nothing here knows about groups: it turns bytes into trimmed fields and
//! numbers, and reports where anything it could not read was found.

use std::io::BufRead;

use crate::diag::Diagnostic;
use crate::error::{CsvError, Result};

/// The delimiters auto-detection considers, in preference order.
pub const DELIMITERS: [char; 4] = [',', ';', '\t', '|'];

/// Cells that mean "no value". Matched case-insensitively, after trimming.
const MISSING: [&str; 5] = ["", "nan", "na", "n/a", "null"];

/// One line of the source file, with the position needed to point at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// 1-based.
    pub number: u64,
    /// Byte offset of the first character of the line.
    pub byte_offset: u64,
}

/// Splits a reader into lines, coping with the three line endings and with a
/// byte-order mark, and reporting invalid UTF-8 by offset rather than
/// replacing it (§7.3).
///
/// A lone `CR` is accepted as a line ending and noted once: a file written by
/// a classic-Mac-era tool is readable, but the user is told it is unusual.
#[derive(Debug)]
pub struct LineReader<R> {
    reader: R,
    /// Bytes consumed from the reader so far.
    offset: u64,
    number: u64,
    /// Lines split out of one `read_until` chunk by a lone CR, in reverse.
    pending: Vec<Line>,
    started: bool,
    lone_cr_reported: bool,
}

impl<R: BufRead> LineReader<R> {
    #[must_use]
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            offset: 0,
            number: 0,
            pending: Vec::new(),
            started: false,
            lone_cr_reported: false,
        }
    }

    /// Bytes consumed so far, for progress reporting.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.offset
    }

    /// The next line, or `None` at end of file. `diagnostics` collects the
    /// one-off notes the reader itself raises.
    pub fn next_line(&mut self, diagnostics: &mut Vec<Diagnostic>) -> Result<Option<Line>> {
        if let Some(line) = self.pending.pop() {
            return Ok(Some(line));
        }

        let mut buffer = Vec::new();
        let start = self.offset;
        let read = self
            .reader
            .read_until(b'\n', &mut buffer)
            .map_err(CsvError::Io)?;
        if read == 0 {
            return Ok(None);
        }
        self.offset += read as u64;

        let mut bytes = buffer.as_slice();
        let mut leading = 0usize;
        if !self.started {
            self.started = true;
            let stripped = strip_bom(bytes)?;
            leading = bytes.len() - stripped.len();
            bytes = stripped;
        }
        // Trim the terminator; a chunk that hit EOF has none.
        if bytes.last() == Some(&b'\n') {
            bytes = &bytes[..bytes.len() - 1];
        }
        if bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }

        let text = decode(bytes, start + leading as u64)?;

        // A lone CR inside the chunk is a classic-Mac line ending.
        if text.contains('\r') {
            if !self.lone_cr_reported {
                self.lone_cr_reported = true;
                diagnostics.push(Diagnostic::warning(
                    self.number + 1,
                    start,
                    "a lone carriage return was treated as a line ending",
                ));
            }
            let mut offset = start + leading as u64;
            let mut split = Vec::new();
            for piece in text.split('\r') {
                self.number += 1;
                split.push(Line {
                    text: piece.to_owned(),
                    number: self.number,
                    byte_offset: offset,
                });
                offset += piece.len() as u64 + 1;
            }
            split.reverse();
            self.pending = split;
            return Ok(self.pending.pop());
        }

        self.number += 1;
        Ok(Some(Line {
            text,
            number: self.number,
            byte_offset: start + leading as u64,
        }))
    }
}

fn strip_bom(bytes: &[u8]) -> Result<&[u8]> {
    const UTF8: [u8; 3] = [0xEF, 0xBB, 0xBF];
    if bytes.starts_with(&UTF8) {
        return Ok(&bytes[3..]);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(CsvError::NotUtf8 {
            byte_offset: 0,
            detail: "the file starts with a UTF-16 byte-order mark; only UTF-8 is read".to_owned(),
        });
    }
    Ok(bytes)
}

fn decode(bytes: &[u8], byte_offset: u64) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| {
        let position = byte_offset + error.utf8_error().valid_up_to() as u64;
        CsvError::NotUtf8 {
            byte_offset: position,
            detail: format!("invalid UTF-8 at byte {position}"),
        }
    })
}

/// One decoded field: the text with surrounding whitespace removed, and where
/// it started within its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    /// Byte offset within the line.
    pub offset: usize,
}

/// Splits one line into fields, honouring `"` quoting with `""` as an escaped
/// quote, and trimming whitespace around every field — the format writes `, `
/// as its delimiter run (§7.3).
#[must_use]
pub fn split_fields(line: &str, delimiter: char) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    let mut quoted = false;
    let mut chars = line.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek().map(|(_, c)| *c) == Some('"') {
                    chars.next();
                    current.push('"');
                } else {
                    quoted = false;
                }
            } else {
                current.push(ch);
            }
        } else if ch == '"' && current.trim().is_empty() {
            // An opening quote: anything before it in the field was whitespace.
            current.clear();
            quoted = true;
        } else if ch == delimiter {
            fields.push(Field {
                text: current.trim().to_owned(),
                offset: start,
            });
            current.clear();
            start = index + ch.len_utf8();
        } else {
            current.push(ch);
        }
    }
    fields.push(Field {
        text: current.trim().to_owned(),
        offset: start,
    });
    fields
}

/// Whether a cell means "no value" (§7.3).
#[must_use]
pub fn is_missing(text: &str) -> bool {
    let text = text.trim();
    MISSING
        .iter()
        .any(|candidate| text.eq_ignore_ascii_case(candidate))
}

/// Reads a numeric cell. A missing cell is `NaN`; anything else unreadable is
/// `None` so the caller can raise a diagnostic naming it.
///
/// Locale-independent: `.` is the only decimal point, and a thousands
/// separator is rejected rather than silently dropped.
#[must_use]
pub fn parse_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if is_missing(text) {
        return Some(f64::NAN);
    }
    if text.contains(',') || text.contains('_') || text.contains(' ') {
        return None;
    }
    text.parse::<f64>().ok()
}

/// Picks the delimiter that splits `line` into the most fields, preferring the
/// order in [`DELIMITERS`] on a tie.
#[must_use]
pub fn detect_delimiter(line: &str) -> char {
    let mut best = DELIMITERS[0];
    let mut best_count = 0;
    for delimiter in DELIMITERS {
        let count = split_fields(line, delimiter).len();
        if count > best_count {
            best_count = count;
            best = delimiter;
        }
    }
    best
}

/// How a delimiter is named in the mapping UI.
#[must_use]
pub fn delimiter_label(delimiter: char) -> &'static str {
    match delimiter {
        ',' => "comma",
        ';' => "semicolon",
        '\t' => "tab",
        '|' => "pipe",
        _ => "custom",
    }
}

/// Quotes a field for output when it contains the delimiter, a quote, or
/// leading or trailing whitespace that would otherwise be lost.
#[must_use]
pub fn quote_field(text: &str, delimiter: char) -> String {
    let needs_quoting = text.contains(delimiter)
        || text.contains('"')
        || text.contains('\n')
        || text.contains('\r')
        || text.trim() != text;
    if !needs_quoting {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &str) -> (Vec<Line>, Vec<Diagnostic>) {
        let mut reader = LineReader::new(input.as_bytes());
        let mut diagnostics = Vec::new();
        let mut out = Vec::new();
        while let Some(line) = reader.next_line(&mut diagnostics).unwrap() {
            out.push(line);
        }
        (out, diagnostics)
    }

    #[test]
    fn crlf_and_lf_both_read_as_line_endings() {
        let (lf, _) = lines("a\nb\nc");
        let (crlf, _) = lines("a\r\nb\r\nc");
        assert_eq!(
            lf.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(
            crlf.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(lf[1].number, 2);
        assert_eq!(crlf[1].number, 2);
    }

    #[test]
    fn a_lone_cr_is_a_line_ending_with_one_warning() {
        let (read, diagnostics) = lines("a\rb\rc\n");
        assert_eq!(
            read.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("carriage return"));
    }

    #[test]
    fn line_offsets_point_at_the_start_of_the_line() {
        let (read, _) = lines("ab\ncd\r\nef");
        assert_eq!(read[0].byte_offset, 0);
        assert_eq!(read[1].byte_offset, 3);
        assert_eq!(read[2].byte_offset, 7);
    }

    #[test]
    fn a_utf8_bom_is_stripped_and_utf16_is_refused() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"time,power\n");
        let mut reader = LineReader::new(bytes.as_slice());
        let line = reader.next_line(&mut Vec::new()).unwrap().unwrap();
        assert_eq!(line.text, "time,power");
        assert_eq!(line.byte_offset, 3);

        let utf16: &[u8] = &[0xFF, 0xFE, b't', 0x00];
        let mut reader = LineReader::new(utf16);
        assert!(matches!(
            reader.next_line(&mut Vec::new()),
            Err(CsvError::NotUtf8 { .. })
        ));
    }

    #[test]
    fn invalid_utf8_is_reported_by_offset() {
        let bytes: &[u8] = b"ok\n\xff\xfe bad\n";
        let mut reader = LineReader::new(bytes);
        let mut diagnostics = Vec::new();
        assert!(reader.next_line(&mut diagnostics).unwrap().is_some());
        match reader.next_line(&mut diagnostics) {
            Err(CsvError::NotUtf8 { byte_offset, .. }) => assert_eq!(byte_offset, 3),
            other => panic!("expected a UTF-8 error, got {other:?}"),
        }
    }

    #[test]
    fn fields_are_trimmed_and_quotes_are_honoured() {
        let plain = split_fields("1, 1000, 2, info", ',');
        assert_eq!(
            plain.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            ["1", "1000", "2", "info"]
        );
        let quoted = split_fields("a,\"b,c\",d", ',');
        assert_eq!(
            quoted.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            ["a", "b,c", "d"]
        );
        let escaped = split_fields("a,\"say \"\"hi\"\"\",c", ',');
        assert_eq!(escaped[1].text, "say \"hi\"");
    }

    #[test]
    fn field_offsets_locate_a_bad_cell_within_its_line() {
        let fields = split_fields("10, 100, oops", ',');
        assert_eq!(fields[2].offset, 8);
        assert_eq!(fields[2].text, "oops");
    }

    #[test]
    fn an_empty_line_is_one_empty_field() {
        let fields = split_fields("", ',');
        assert_eq!(fields.len(), 1);
        assert!(fields[0].text.is_empty());
    }

    #[test]
    fn missing_values_read_as_nan_and_junk_reads_as_nothing() {
        for text in ["", " ", "NaN", "nan", "NA", "n/a", "null", "NULL"] {
            assert!(parse_number(text).unwrap().is_nan(), "{text}");
        }
        assert_eq!(parse_number("100"), Some(100.0));
        assert_eq!(parse_number("-1.5e3"), Some(-1500.0));
        assert_eq!(parse_number(" 2.5 "), Some(2.5));
        assert_eq!(parse_number("1,234"), None);
        assert_eq!(parse_number("1_000"), None);
        assert_eq!(parse_number("1 000"), None);
        assert_eq!(parse_number("twelve"), None);
    }

    #[test]
    fn the_delimiter_is_detected_from_a_header_line() {
        assert_eq!(detect_delimiter("groupID,total time, count, info"), ',');
        assert_eq!(detect_delimiter("a;b;c;d"), ';');
        assert_eq!(detect_delimiter("a\tb\tc"), '\t');
        assert_eq!(detect_delimiter("a|b|c"), '|');
        // Nothing to go on: the format's own default.
        assert_eq!(detect_delimiter("single"), ',');
    }

    #[test]
    fn quoting_survives_a_round_trip_through_the_splitter() {
        for text in ["plain", "a,b", "say \"hi\"", "a;b"] {
            let written = quote_field(text, ',');
            let read = split_fields(&written, ',');
            assert_eq!(read.len(), 1, "{text} -> {written}");
            assert_eq!(read[0].text, text.trim(), "{text} -> {written}");
        }
    }
}
