//! Import diagnostics (`docs/DESIGN.md` §7.3).
//!
//! Every diagnostic carries enough position to jump to the offending line in
//! an editor: the group it was found in, the 1-based line number, the byte
//! offset of the line in the file, and the column index when the problem is a
//! single field.

use std::fmt;

/// How much a diagnostic matters. Only [`Severity::Error`] aborts an import,
/// and then only in strict mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Warning,
    Error,
}

impl Severity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One thing the parser found wrong, positioned in the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Block index in the file, when the problem is inside a group.
    pub group_index: Option<u32>,
    /// 1-based line number.
    pub line: u64,
    /// Byte offset of the start of the line.
    pub byte_offset: u64,
    /// 0-based field index, when the problem is one field.
    pub column_index: Option<usize>,
    pub message: String,
}

impl Diagnostic {
    #[must_use]
    pub fn new(
        severity: Severity,
        line: u64,
        byte_offset: u64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            group_index: None,
            line,
            byte_offset,
            column_index: None,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn warning(line: u64, byte_offset: u64, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, line, byte_offset, message)
    }

    #[must_use]
    pub fn error(line: u64, byte_offset: u64, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, line, byte_offset, message)
    }

    #[must_use]
    pub fn in_group(mut self, group_index: u32) -> Self {
        self.group_index = Some(group_index);
        self
    }

    #[must_use]
    pub fn at_column(mut self, column_index: usize) -> Self {
        self.column_index = Some(column_index);
        self
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}", self.line)?;
        if let Some(column) = self.column_index {
            write!(f, ", column {}", column + 1)?;
        }
        if let Some(group) = self.group_index {
            write!(f, " (group {group})")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// A bounded diagnostic list. A file that is wrong on every one of ten million
/// lines must not fill memory with the news, so collection stops at
/// [`Diagnostics::CAP`] and the total keeps counting.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
    total: usize,
    errors: usize,
}

impl Diagnostics {
    /// Diagnostics retained; the rest are counted only.
    pub const CAP: usize = 1_000;

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.total += 1;
        if diagnostic.is_error() {
            self.errors += 1;
        }
        if self.items.len() < Self::CAP {
            self.items.push(diagnostic);
        }
    }

    #[must_use]
    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    /// Every diagnostic raised, including those past the cap.
    #[must_use]
    pub fn total(&self) -> usize {
        self.total
    }

    #[must_use]
    pub fn error_count(&self) -> usize {
        self.errors
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Whether diagnostics were dropped to stay within the cap.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total > self.items.len()
    }

    #[must_use]
    pub fn into_items(self) -> Vec<Diagnostic> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_reads_as_a_jumpable_position() {
        let diagnostic = Diagnostic::error(42, 900, "count mismatch")
            .in_group(3)
            .at_column(2);
        assert_eq!(
            diagnostic.to_string(),
            "line 42, column 3 (group 3): count mismatch"
        );
    }

    #[test]
    fn collection_stops_at_the_cap_but_counting_does_not() {
        let mut diagnostics = Diagnostics::new();
        for line in 0..(Diagnostics::CAP as u64 + 10) {
            diagnostics.push(Diagnostic::warning(line, line * 8, "odd"));
        }
        assert_eq!(diagnostics.items().len(), Diagnostics::CAP);
        assert_eq!(diagnostics.total(), Diagnostics::CAP + 10);
        assert!(diagnostics.truncated());
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn errors_are_counted_apart_from_warnings() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.push(Diagnostic::warning(1, 0, "a"));
        diagnostics.push(Diagnostic::error(2, 10, "b"));
        assert_eq!(diagnostics.total(), 2);
        assert_eq!(diagnostics.error_count(), 1);
        assert!(diagnostics.has_errors());
    }
}
