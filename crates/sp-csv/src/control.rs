//! Progress reporting and cancellation for a long import (`docs/DESIGN.md`
//! §4.2, goal G5).
//!
//! The parser owns no threads and no channels: it is handed a control and
//! calls into it at row-batch boundaries, so cancel latency stays under the
//! ~50 ms budget without the UI reaching into the parser's state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{CsvError, Result};

/// Rows read between cancellation checks and progress reports (§4.2).
pub const TICK_ROWS: u64 = 1_000;

/// What an import has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportProgress {
    pub bytes_read: u64,
    /// `None` when the source has no known length (a pipe, a test string).
    pub total_bytes: Option<u64>,
    pub groups: u32,
    pub pulses: u64,
    pub diagnostics: usize,
}

impl ImportProgress {
    /// Completion in `0.0..=1.0`, when the total is known.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total_bytes?;
        if total == 0 {
            return Some(1.0);
        }
        Some((self.bytes_read as f32 / total as f32).clamp(0.0, 1.0))
    }
}

/// The cancel flag and progress sink handed to an import.
///
/// Cloning shares both, so the UI keeps one and the worker keeps another.
#[derive(Clone, Default)]
pub struct ImportControl {
    cancel: Option<Arc<AtomicBool>>,
    progress: Option<Arc<dyn Fn(ImportProgress) + Send + Sync>>,
    total_bytes: Option<u64>,
}

impl std::fmt::Debug for ImportControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportControl")
            .field("cancelled", &self.is_cancelled())
            .field("total_bytes", &self.total_bytes)
            .field("reports_progress", &self.progress.is_some())
            .finish()
    }
}

impl ImportControl {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Shares a flag the caller sets to stop the import.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Installs the sink progress is reported to. It is called from the worker
    /// thread, roughly every [`TICK_ROWS`] rows and at every group boundary.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn Fn(ImportProgress) + Send + Sync>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// The file's length, so progress can be a fraction rather than a count.
    #[must_use]
    pub fn with_total_bytes(mut self, total_bytes: u64) -> Self {
        self.total_bytes = Some(total_bytes);
        self
    }

    #[must_use]
    pub fn total_bytes(&self) -> Option<u64> {
        self.total_bytes
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// `Err(Cancelled)` once the flag is set, so a `?` unwinds the import to
    /// its transaction boundary and rolls it back.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(CsvError::Cancelled);
        }
        Ok(())
    }

    pub fn report(&self, mut progress: ImportProgress) {
        if let Some(sink) = &self.progress {
            progress.total_bytes = self.total_bytes;
            sink(progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_control_with_nothing_set_never_cancels() {
        let control = ImportControl::new();
        assert!(!control.is_cancelled());
        assert!(control.check().is_ok());
        control.report(ImportProgress::default());
    }

    #[test]
    fn setting_the_flag_cancels_at_the_next_check() {
        let flag = Arc::new(AtomicBool::new(false));
        let control = ImportControl::new().with_cancel(flag.clone());
        assert!(control.check().is_ok());
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(control.check(), Err(CsvError::Cancelled)));
    }

    #[test]
    fn progress_reports_carry_the_total_the_control_knows() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let control = ImportControl::new()
            .with_total_bytes(400)
            .with_progress(Arc::new(move |progress| {
                sink.lock().unwrap().push(progress);
            }));

        control.report(ImportProgress {
            bytes_read: 100,
            groups: 1,
            ..ImportProgress::default()
        });

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].total_bytes, Some(400));
        assert_eq!(seen[0].fraction(), Some(0.25));
    }

    #[test]
    fn a_fraction_needs_a_total_and_is_clamped() {
        assert_eq!(ImportProgress::default().fraction(), None);
        let progress = ImportProgress {
            bytes_read: 900,
            total_bytes: Some(400),
            ..ImportProgress::default()
        };
        assert_eq!(progress.fraction(), Some(1.0));
    }
}
