//! Vocabulary shared by everything that takes part in a pipeline run
//! (`docs/DESIGN.md` §9.5, §9.6).
//!
//! The orchestrator in `sp-proc` produces these values and the store writes
//! them into the run tables, so they live here rather than in either: the
//! tokens are exactly what the schema's check constraints accept, which is
//! what keeps a status from being spelled two ways.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::signal::ParseTokenError;
use crate::time::TimeRange;

crate::id_newtype! {
    /// Identifies a saved pipeline.
    PipelineId
}

crate::id_newtype! {
    /// Identifies one recorded artifact row.
    ArtifactId
}

/// How a run ended, or that it has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub const ALL: [Self; 4] = [Self::Running, Self::Ok, Self::Failed, Self::Cancelled];

    /// The token stored in `run.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Ok => "Succeeded",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }

    /// Whether the run is over, however it ended.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// What became of one stage on one group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// Ran and produced output.
    Ok,
    /// Raised an error; the rest of the group's stages did not run.
    Failed,
    /// Disabled in the pipeline, or skipped because an earlier stage failed.
    Skipped,
    /// Its cache key was already recorded, so its output was reused (§9.5).
    Cached,
}

impl StageStatus {
    pub const ALL: [Self; 4] = [Self::Ok, Self::Failed, Self::Skipped, Self::Cached];

    /// The token stored in `run_stage.status`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cached => "cached",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "Ran",
            Self::Failed => "Failed",
            Self::Skipped => "Skipped",
            Self::Cached => "Cached",
        }
    }

    /// Whether output was recorded for this stage, which is true of a cache
    /// hit as much as of a stage that actually ran.
    #[must_use]
    pub const fn produced_output(self) -> bool {
        matches!(self, Self::Ok | Self::Cached)
    }
}

/// What a stage did to one of its input signals (§9.4).
///
/// Every input signal gets one of these, so "what did this stage do to signal
/// 3?" is answered by a recorded row rather than by inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Replaced,
    Added,
    Passthrough,
    Dropped,
}

impl Disposition {
    pub const ALL: [Self; 4] = [
        Self::Replaced,
        Self::Added,
        Self::Passthrough,
        Self::Dropped,
    ];

    /// The token stored in `run_signal.disposition`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::Added => "added",
            Self::Passthrough => "passthrough",
            Self::Dropped => "dropped",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Replaced => "Replaced",
            Self::Added => "Added",
            Self::Passthrough => "Untouched",
            Self::Dropped => "Dropped",
        }
    }

    /// Whether the signal is still visible to the next stage. A dropped
    /// signal is recorded at this stage but does not flow onward.
    #[must_use]
    pub const fn flows_onward(self) -> bool {
        !matches!(self, Self::Dropped)
    }
}

/// How long a stage's recorded output is kept (§9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    #[default]
    Always,
    OnFailure,
    Never,
}

impl Retention {
    pub const ALL: [Self; 3] = [Self::Always, Self::OnFailure, Self::Never];

    /// The token stored in `pipeline_stage.retention`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::OnFailure => "on_failure",
            Self::Never => "never",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Always => "Always keep",
            Self::OnFailure => "Keep on failure",
            Self::Never => "Never keep",
        }
    }

    /// Whether samples produced under this policy are written, given how the
    /// group turned out. Metrics and diagnostics are always recorded — they
    /// are small, and they are what a failed run is read for.
    #[must_use]
    pub const fn keeps_samples(self, group_failed: bool) -> bool {
        match self {
            Self::Always => true,
            Self::OnFailure => group_failed,
            Self::Never => false,
        }
    }
}

/// The seriousness of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub const ALL: [Self; 3] = [Self::Info, Self::Warn, Self::Error];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Something a stage wants to say about a group, optionally pinned to a span
/// of the timeline so the scope can point at it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    /// Where on the absolute timeline this applies, when it is local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<TimeRange>,
    /// Which of the group's signals this is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_ordinal: Option<u32>,
}

impl Diagnostic {
    #[must_use]
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(Severity::Info, message)
    }

    #[must_use]
    pub fn warn(message: impl Into<String>) -> Self {
        Self::new(Severity::Warn, message)
    }

    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Severity::Error, message)
    }

    #[must_use]
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
            span: None,
            signal_ordinal: None,
        }
    }

    #[must_use]
    pub fn at(mut self, span: TimeRange) -> Self {
        self.span = Some(span);
        self
    }

    #[must_use]
    pub fn about_signal(mut self, ordinal: u32) -> Self {
        self.signal_ordinal = Some(ordinal);
        self
    }
}

/// Generates the string plumbing every one of these tokens needs: they are
/// written to a check-constrained column and read back out of it.
macro_rules! token_conversions {
    ($name:ident, $what:literal) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseTokenError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .into_iter()
                    .find(|value| value.as_str() == s)
                    .ok_or_else(|| ParseTokenError {
                        kind: $what,
                        token: s.to_owned(),
                    })
            }
        }
    };
}

token_conversions!(RunStatus, "run status");
token_conversions!(StageStatus, "stage status");
token_conversions!(Disposition, "disposition");
token_conversions!(Retention, "retention policy");
token_conversions!(Severity, "severity");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_round_trips_through_its_column() {
        for status in RunStatus::ALL {
            assert_eq!(status.as_str().parse::<RunStatus>().unwrap(), status);
        }
        for status in StageStatus::ALL {
            assert_eq!(status.as_str().parse::<StageStatus>().unwrap(), status);
        }
        for disposition in Disposition::ALL {
            assert_eq!(
                disposition.as_str().parse::<Disposition>().unwrap(),
                disposition
            );
        }
        for retention in Retention::ALL {
            assert_eq!(retention.as_str().parse::<Retention>().unwrap(), retention);
        }
        for severity in Severity::ALL {
            assert_eq!(severity.as_str().parse::<Severity>().unwrap(), severity);
        }
    }

    #[test]
    fn tokens_match_the_schema_check_constraints() {
        let run: Vec<_> = RunStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(run, ["running", "ok", "failed", "cancelled"]);
        let stage: Vec<_> = StageStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(stage, ["ok", "failed", "skipped", "cached"]);
        let disposition: Vec<_> = Disposition::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(disposition, ["replaced", "added", "passthrough", "dropped"]);
        let retention: Vec<_> = Retention::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(retention, ["always", "on_failure", "never"]);
    }

    #[test]
    fn an_unknown_token_names_what_it_was_read_as() {
        let err = "sideways".parse::<RunStatus>().unwrap_err();
        assert_eq!(err.to_string(), "unknown run status 'sideways'");
    }

    #[test]
    fn a_cache_hit_counts_as_having_produced_output() {
        // The results screen shows a cached stage exactly like one that ran;
        // only the timing differs.
        assert!(StageStatus::Cached.produced_output());
        assert!(StageStatus::Ok.produced_output());
        assert!(!StageStatus::Skipped.produced_output());
        assert!(!StageStatus::Failed.produced_output());
    }

    #[test]
    fn a_dropped_signal_is_recorded_but_does_not_flow_on() {
        assert!(!Disposition::Dropped.flows_onward());
        for kept in [
            Disposition::Replaced,
            Disposition::Added,
            Disposition::Passthrough,
        ] {
            assert!(kept.flows_onward());
        }
    }

    #[test]
    fn on_failure_retention_keeps_samples_only_for_a_failed_group() {
        assert!(!Retention::OnFailure.keeps_samples(false));
        assert!(Retention::OnFailure.keeps_samples(true));
        assert!(Retention::Always.keeps_samples(false));
        assert!(!Retention::Never.keeps_samples(true));
    }

    #[test]
    fn a_diagnostic_can_point_at_a_span_of_one_signal() {
        let diagnostic = Diagnostic::warn("clipped")
            .at(TimeRange::new(1.0, 2.0))
            .about_signal(3);
        assert_eq!(diagnostic.severity, Severity::Warn);
        assert_eq!(diagnostic.span, Some(TimeRange::new(1.0, 2.0)));
        assert_eq!(diagnostic.signal_ordinal, Some(3));

        // A plain diagnostic serialises without the optional halves, which is
        // what keeps `diagnostics_json` readable.
        let json = serde_json::to_string(&Diagnostic::info("ok")).unwrap();
        assert_eq!(json, r#"{"severity":"info","message":"ok"}"#);
    }
}
