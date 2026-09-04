//! Progress reporting and cancellation for a long generation
//! (`docs/DESIGN.md` §4.2, goal G5).
//!
//! The same shape as `sp_csv::ImportControl`, and for the same reason: the
//! renderer owns no threads and no channels, it just calls into a control at
//! chunk boundaries, so the UI never reaches into the renderer's state.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{GenError, Result};

/// Samples rendered between cancellation checks. Large enough that the check
/// is free, small enough to stay inside the ~50 ms cancel budget (§4.2).
pub const CHUNK_SAMPLES: u64 = 65_536;

/// What a generation has done so far.
///
/// The two shapes of output are counted through one pair of names: a waveform
/// generation writes signals of samples (§8.1), a pulse-train generation
/// writes groups of pulses (§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GenProgress {
    /// Signals written in waveform mode — a sweep writes one per rung (§8.3) —
    /// or groups written in pulse-train mode.
    pub items_done: u32,
    pub items_total: u32,
    /// Samples written in waveform mode, pulses in pulse-train mode.
    pub values_done: u64,
    pub values_total: u64,
}

impl GenProgress {
    /// Completion in `0.0..=1.0`.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        if self.values_total == 0 {
            return None;
        }
        Some((self.values_done as f32 / self.values_total as f32).clamp(0.0, 1.0))
    }
}

/// The cancel flag and progress sink handed to a generation.
///
/// Cloning shares both, so the UI keeps one and the worker keeps another.
#[derive(Clone, Default)]
pub struct GenControl {
    cancel: Option<Arc<AtomicBool>>,
    progress: Option<Arc<dyn Fn(GenProgress) + Send + Sync>>,
}

impl fmt::Debug for GenControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenControl")
            .field("cancelled", &self.is_cancelled())
            .field("reports_progress", &self.progress.is_some())
            .finish()
    }
}

impl GenControl {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Shares a flag the caller sets to stop the generation.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Installs the sink progress is reported to. It is called from the worker
    /// thread, at chunk and signal boundaries.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn Fn(GenProgress) + Send + Sync>) -> Self {
        self.progress = Some(progress);
        self
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// `Err(Cancelled)` once the flag is set, so a `?` unwinds to the
    /// transaction boundary and rolls the whole batch back.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(GenError::Cancelled);
        }
        Ok(())
    }

    /// A control that still cancels but reports nothing, for a step whose
    /// caller reports progress at a coarser grain.
    #[must_use]
    pub fn cancel_only(&self) -> Self {
        Self {
            cancel: self.cancel.clone(),
            progress: None,
        }
    }

    pub fn report(&self, progress: GenProgress) {
        if let Some(sink) = &self.progress {
            sink(progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn a_control_with_nothing_set_never_cancels() {
        let control = GenControl::new();
        assert!(!control.is_cancelled());
        assert!(control.check().is_ok());
        control.report(GenProgress::default());
    }

    #[test]
    fn setting_the_flag_cancels_at_the_next_check() {
        let flag = Arc::new(AtomicBool::new(false));
        let control = GenControl::new().with_cancel(flag.clone());
        assert!(control.check().is_ok());
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(control.check(), Err(GenError::Cancelled)));
    }

    #[test]
    fn progress_reaches_the_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let control = GenControl::new().with_progress(Arc::new(move |progress| {
            sink.lock().unwrap().push(progress);
        }));
        control.report(GenProgress {
            values_done: 50,
            values_total: 200,
            ..GenProgress::default()
        });
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].fraction(), Some(0.25));
    }

    #[test]
    fn a_fraction_needs_a_total() {
        assert_eq!(GenProgress::default().fraction(), None);
    }
}
