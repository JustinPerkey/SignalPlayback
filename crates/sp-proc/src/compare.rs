//! Comparing one run against another (`docs/DESIGN.md` §10.4).
//!
//! Two runs over the same dataset diff group by group, and each group diffs in
//! three registers, because a stage produces three kinds of thing:
//!
//! * **signals** — max absolute error, RMS error and the index of the first
//!   sample that differs, which is the one worth looking at;
//! * **metrics** — the numbers a stage recorded, compared value by value;
//! * **artifacts** — a field-level diff with numeric tolerance, read through a
//!   schema inferred from the payload so a diff works even in a build that no
//!   longer links the stage that wrote it.
//!
//! A **baseline check** is this same diff with a verdict attached: the
//! baseline's tolerances decide whether each difference is drift or a
//! regression, and the run passes only if nothing exceeded them and no group
//! the baseline covers has gone missing. That is what makes
//! `signalplayback run --assert-baseline` a CI gate (G8).
//!
//! The comparison reads the *recorded* rows on both sides. Nothing is
//! recomputed, so a diff says what the runs actually produced rather than what
//! re-running them today would produce.

use std::collections::{BTreeMap, BTreeSet};

use sp_core::artifact::{self, ArtifactData, FieldDiff};
use sp_core::run::RunStatus;
use sp_core::time::SampleRange;
use sp_core::{GroupId, RunId, Tolerances};
use sp_store::runs::{self, RunSignalRow, SOURCE_STAGE};
use sp_store::{Store, StoreError};

use crate::error::{ProcError, Result};

/// Samples read from each side at a time. Two chunks of this size are the
/// comparison's whole memory cost, so a 100 M-sample signal diffs in about a
/// megabyte rather than by loading both columns.
const CHUNK: u64 = 1 << 16;

/// What to compare, and how much difference is allowed.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffOptions {
    pub tolerances: Tolerances,
    /// Compare every stage both runs recorded, rather than only the last one
    /// each group reached. Every stage answers "where did they first
    /// diverge?"; the last one answers "did the answer change?".
    pub every_stage: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            tolerances: Tolerances::EXACT,
            every_stage: true,
        }
    }
}

impl DiffOptions {
    #[must_use]
    pub fn within(tolerances: Tolerances) -> Self {
        Self {
            tolerances,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn final_stage_only(mut self) -> Self {
        self.every_stage = false;
        self
    }
}

/// How two signals of the same name compare at one stage.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalDiff {
    pub stage_ordinal: i32,
    pub name: String,
    /// Sample counts on the baseline and candidate sides.
    pub samples: (u64, u64),
    pub max_abs_error: f64,
    pub rms_error: f64,
    /// The first sample index at which they differ by more than the bound.
    pub first_divergence: Option<u64>,
    /// The largest difference the tolerances allowed for this signal.
    pub bound: f64,
    pub within_tolerance: bool,
    /// Why the two were not compared sample by sample, when they were not.
    pub note: Option<String>,
}

impl SignalDiff {
    /// One line for a report.
    #[must_use]
    pub fn describe(&self) -> String {
        if let Some(note) = &self.note {
            return format!("signal '{}': {note}", self.name);
        }
        if self.samples.0 != self.samples.1 {
            return format!(
                "signal '{}': {} samples, was {}",
                self.name, self.samples.1, self.samples.0
            );
        }
        match self.first_divergence {
            None => format!("signal '{}': identical", self.name),
            Some(index) => format!(
                "signal '{}': max |Δ| {:.6e}, RMS {:.6e}, first differs at sample {index}",
                self.name, self.max_abs_error, self.rms_error
            ),
        }
    }
}

/// How one metric compares at one stage.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricDiff {
    pub stage_ordinal: i32,
    pub name: String,
    /// Baseline and candidate values; `None` where that side did not record
    /// the metric at all.
    pub values: (Option<f64>, Option<f64>),
    pub abs_error: f64,
    pub bound: f64,
    pub within_tolerance: bool,
}

impl MetricDiff {
    #[must_use]
    pub fn describe(&self) -> String {
        match self.values {
            (Some(before), Some(after)) => format!(
                "metric '{}': {after} (was {before}, Δ {:.6e})",
                self.name, self.abs_error
            ),
            (Some(before), None) => format!("metric '{}': gone (was {before})", self.name),
            (None, Some(after)) => format!("metric '{}': new, {after}", self.name),
            (None, None) => format!("metric '{}': absent on both sides", self.name),
        }
    }
}

/// How one artifact compares, field by field.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactDiff {
    pub stage_ordinal: i32,
    pub port: String,
    pub fields: Vec<FieldDiff>,
    pub within_tolerance: bool,
    /// Set when one side published on this port and the other did not.
    pub note: Option<String>,
}

impl ArtifactDiff {
    #[must_use]
    pub fn describe(&self) -> String {
        if let Some(note) = &self.note {
            return format!("artifact '{}': {note}", self.port);
        }
        let changed: Vec<&FieldDiff> = self.fields.iter().filter(|f| !f.is_equal()).collect();
        if changed.is_empty() {
            return format!("artifact '{}': identical", self.port);
        }
        let detail = changed
            .iter()
            .map(|field| {
                format!(
                    "{} ({} row(s) differ, max |Δ| {:.6e})",
                    field.field, field.mismatches, field.max_abs_error
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("artifact '{}': {detail}", self.port)
    }
}

/// How one group compares between two runs.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupDiff {
    pub group: GroupId,
    /// Baseline and candidate group status.
    pub status: (RunStatus, RunStatus),
    pub signals: Vec<SignalDiff>,
    pub metrics: Vec<MetricDiff>,
    pub artifacts: Vec<ArtifactDiff>,
}

impl GroupDiff {
    /// Whether everything about this group stayed within tolerance — the
    /// group's status included, since a group that started failing is a
    /// regression however identical its numbers.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.status.0 == self.status.1
            && self.signals.iter().all(|d| d.within_tolerance)
            && self.metrics.iter().all(|d| d.within_tolerance)
            && self.artifacts.iter().all(|d| d.within_tolerance)
    }

    /// Every difference that exceeded tolerance, in words.
    #[must_use]
    pub fn deviations(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.status.0 != self.status.1 {
            lines.push(format!(
                "group is {}, was {}",
                self.status.1.label().to_lowercase(),
                self.status.0.label().to_lowercase()
            ));
        }
        lines.extend(
            self.signals
                .iter()
                .filter(|d| !d.within_tolerance)
                .map(SignalDiff::describe),
        );
        lines.extend(
            self.metrics
                .iter()
                .filter(|d| !d.within_tolerance)
                .map(MetricDiff::describe),
        );
        lines.extend(
            self.artifacts
                .iter()
                .filter(|d| !d.within_tolerance)
                .map(ArtifactDiff::describe),
        );
        lines
    }
}

/// Two runs, compared.
#[derive(Debug, Clone, PartialEq)]
pub struct RunDiff {
    /// The run treated as the reference — the baseline, in a baseline check.
    pub baseline: RunId,
    pub candidate: RunId,
    /// Whether the two ran the same algorithm. A different hash does not fail
    /// a comparison by itself: comparing an old and a new pipeline is exactly
    /// what a deliberate change is reviewed by.
    pub same_pipeline: bool,
    pub groups: Vec<GroupDiff>,
    /// Groups the baseline covers and the candidate does not. Always a
    /// failure: a case stopped being tested.
    pub missing_groups: Vec<GroupId>,
    /// Groups only the candidate covers.
    pub new_groups: Vec<GroupId>,
    tolerances: Tolerances,
}

impl RunDiff {
    /// Whether the candidate is within tolerance of the baseline everywhere.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.missing_groups.is_empty()
            && (self.tolerances.allow_new_groups || self.new_groups.is_empty())
            && self.groups.iter().all(GroupDiff::is_clean)
    }

    /// The groups that deviated, with their reasons — what a failing CI run
    /// prints.
    #[must_use]
    pub fn deviations(&self) -> Vec<(GroupId, Vec<String>)> {
        let mut out = Vec::new();
        for group in &self.missing_groups {
            out.push((*group, vec!["the run did not cover this group".to_owned()]));
        }
        if !self.tolerances.allow_new_groups {
            for group in &self.new_groups {
                out.push((
                    *group,
                    vec!["the baseline does not cover this group".to_owned()],
                ));
            }
        }
        for group in &self.groups {
            let lines = group.deviations();
            if !lines.is_empty() {
                out.push((group.group, lines));
            }
        }
        out
    }

    /// One line summarising the comparison.
    #[must_use]
    pub fn describe(&self) -> String {
        let deviating = self.deviations().len();
        if deviating == 0 {
            return format!(
                "run {} matches run {} over {} group(s)",
                self.candidate.get(),
                self.baseline.get(),
                self.groups.len()
            );
        }
        format!(
            "run {} deviates from run {} in {deviating} of {} group(s)",
            self.candidate.get(),
            self.baseline.get(),
            self.groups.len() + self.missing_groups.len()
        )
    }
}

/// Diffs `candidate` against `baseline`, group by group (§10.4).
///
/// Groups are matched by id: two runs over the same dataset compare
/// case-for-case, and a group only one side covers is reported rather than
/// silently dropped.
pub fn diff_runs(
    store: &Store,
    baseline: RunId,
    candidate: RunId,
    options: &DiffOptions,
) -> Result<RunDiff> {
    let (base_run, cand_run) = store.read(|conn| {
        Ok((
            runs::get_run(conn, baseline)?,
            runs::get_run(conn, candidate)?,
        ))
    })?;

    let (base_groups, cand_groups) = store.read(|conn| {
        Ok((
            runs::run_groups(conn, baseline)?,
            runs::run_groups(conn, candidate)?,
        ))
    })?;

    let base_status: BTreeMap<GroupId, RunStatus> = base_groups
        .iter()
        .map(|row| (row.group_id, row.status))
        .collect();
    let cand_status: BTreeMap<GroupId, RunStatus> = cand_groups
        .iter()
        .map(|row| (row.group_id, row.status))
        .collect();

    let mut diff = RunDiff {
        baseline,
        candidate,
        same_pipeline: base_run.pipeline_hash == cand_run.pipeline_hash,
        groups: Vec::new(),
        missing_groups: base_status
            .keys()
            .filter(|group| !cand_status.contains_key(group))
            .copied()
            .collect(),
        new_groups: cand_status
            .keys()
            .filter(|group| !base_status.contains_key(group))
            .copied()
            .collect(),
        tolerances: options.tolerances,
    };

    for (group, status) in &base_status {
        let Some(candidate_status) = cand_status.get(group) else {
            continue;
        };
        diff.groups.push(diff_group(
            store,
            baseline,
            candidate,
            *group,
            (*status, *candidate_status),
            options,
        )?);
    }
    Ok(diff)
}

/// One group of two runs, compared stage by stage.
fn diff_group(
    store: &Store,
    baseline: RunId,
    candidate: RunId,
    group: GroupId,
    status: (RunStatus, RunStatus),
    options: &DiffOptions,
) -> Result<GroupDiff> {
    let (base_stages, cand_stages) = store.read(|conn| {
        Ok((
            runs::group_stages(conn, baseline, group)?,
            runs::group_stages(conn, candidate, group)?,
        ))
    })?;

    let mut ordinals: BTreeSet<i32> = BTreeSet::new();
    ordinals.insert(SOURCE_STAGE);
    ordinals.extend(base_stages.iter().map(|row| row.stage_ordinal));
    ordinals.extend(cand_stages.iter().map(|row| row.stage_ordinal));
    if !options.every_stage {
        let last = ordinals.iter().copied().next_back().unwrap_or(SOURCE_STAGE);
        ordinals.retain(|ordinal| *ordinal == last);
    }

    let mut diff = GroupDiff {
        group,
        status,
        signals: Vec::new(),
        metrics: Vec::new(),
        artifacts: Vec::new(),
    };

    for ordinal in ordinals {
        diff.signals.extend(diff_signals(
            store, baseline, candidate, group, ordinal, options,
        )?);
    }

    for (name, values) in metric_pairs(&base_stages, &cand_stages) {
        let ((stage, name), (before, after)) = (name, values);
        let bound = options.tolerances.metric_bound(before.unwrap_or(0.0));
        let abs_error = match (before, after) {
            (Some(a), Some(b)) => (a - b).abs(),
            _ => f64::INFINITY,
        };
        diff.metrics.push(MetricDiff {
            stage_ordinal: stage,
            name,
            values: (before, after),
            abs_error,
            bound,
            within_tolerance: abs_error <= bound,
        });
    }

    diff.artifacts = diff_artifacts(store, baseline, candidate, group, options)?;
    Ok(diff)
}

/// The signals of one stage, matched by name. A name only one side has is
/// reported as an uncompared difference rather than skipped.
fn diff_signals(
    store: &Store,
    baseline: RunId,
    candidate: RunId,
    group: GroupId,
    ordinal: i32,
    options: &DiffOptions,
) -> Result<Vec<SignalDiff>> {
    let (base, cand) = store.read(|conn| {
        Ok((
            runs::stage_signals(conn, baseline, group, ordinal)?,
            runs::stage_signals(conn, candidate, group, ordinal)?,
        ))
    })?;
    if base.is_empty() && cand.is_empty() {
        return Ok(Vec::new());
    }

    let mut names: Vec<String> = base.iter().map(|row| row.name.clone()).collect();
    for row in &cand {
        if !names.contains(&row.name) {
            names.push(row.name.clone());
        }
    }

    let mut out = Vec::new();
    for name in names {
        let left = base.iter().find(|row| row.name == name);
        let right = cand.iter().find(|row| row.name == name);
        out.push(match (left, right) {
            (Some(left), Some(right)) => compare_signal(store, ordinal, left, right, options)?,
            (Some(left), None) => absent(
                ordinal,
                &name,
                (left.sample_count, 0),
                "the run does not have this signal",
            ),
            (None, Some(right)) => absent(
                ordinal,
                &name,
                (0, right.sample_count),
                "the baseline does not have this signal",
            ),
            (None, None) => unreachable!("the name came from one of the two sides"),
        });
    }
    Ok(out)
}

fn absent(stage_ordinal: i32, name: &str, samples: (u64, u64), note: &str) -> SignalDiff {
    SignalDiff {
        stage_ordinal,
        name: name.to_owned(),
        samples,
        max_abs_error: f64::INFINITY,
        rms_error: f64::INFINITY,
        first_divergence: Some(0),
        bound: 0.0,
        within_tolerance: false,
        note: Some(note.to_owned()),
    }
}

/// Walks two recorded columns in step, chunk by chunk.
fn compare_signal(
    store: &Store,
    stage_ordinal: i32,
    baseline: &RunSignalRow,
    candidate: &RunSignalRow,
    options: &DiffOptions,
) -> Result<SignalDiff> {
    let bound = options
        .tolerances
        .sample_bound(baseline.stats.rms().unwrap_or(0.0));
    let mut diff = SignalDiff {
        stage_ordinal,
        name: baseline.name.clone(),
        samples: (baseline.sample_count, candidate.sample_count),
        max_abs_error: 0.0,
        rms_error: 0.0,
        first_divergence: None,
        bound,
        within_tolerance: true,
        note: None,
    };

    // A stage whose retention dropped the samples still records its summary,
    // so the diff falls back to comparing that rather than reporting nothing.
    if baseline.blob_id.is_none() || candidate.blob_id.is_none() {
        let stats_error = summary_error(baseline, candidate);
        diff.max_abs_error = stats_error;
        diff.rms_error = stats_error;
        diff.within_tolerance =
            stats_error <= bound && baseline.sample_count == candidate.sample_count;
        diff.first_divergence = (!diff.within_tolerance).then_some(0);
        diff.note = Some(if diff.within_tolerance {
            "samples were not kept; the recorded summaries match".to_owned()
        } else {
            "samples were not kept; the recorded summaries differ".to_owned()
        });
        return Ok(diff);
    }

    if baseline.sample_count != candidate.sample_count {
        diff.within_tolerance = false;
        diff.first_divergence = Some(baseline.sample_count.min(candidate.sample_count));
    }

    let shared = baseline.sample_count.min(candidate.sample_count);
    let mut sum_sq = 0.0f64;
    let mut compared = 0u64;
    let mut start = 0u64;
    while start < shared {
        let range = SampleRange::new(start, (start + CHUNK).min(shared));
        let (left, right) = store.read(|conn| {
            Ok((
                runs::read_signal_samples(conn, baseline, range)?,
                runs::read_signal_samples(conn, candidate, range)?,
            ))
        })?;

        for offset in 0..range.len() {
            let index = offset as usize;
            let (Some(a), Some(b)) = (left.value(index as u64), right.value(index as u64)) else {
                continue;
            };
            let error = (a - b).abs();
            // A pair of NaNs is not a difference: an all-NaN signal that
            // re-ran identically must not read as a regression.
            let error = if a.is_nan() && b.is_nan() { 0.0 } else { error };
            sum_sq += error * error;
            compared += 1;
            if error > diff.max_abs_error {
                diff.max_abs_error = error;
            }
            if error > bound
                && diff
                    .first_divergence
                    .is_none_or(|first| start + offset < first)
            {
                diff.first_divergence = Some(start + offset);
                diff.within_tolerance = false;
            }
        }
        start = range.end;
    }

    if compared > 0 {
        diff.rms_error = (sum_sq / compared as f64).sqrt();
    }
    if diff.max_abs_error > bound {
        diff.within_tolerance = false;
    }
    Ok(diff)
}

/// How far apart two signals' recorded summaries are, for the case where the
/// samples themselves were not kept.
fn summary_error(baseline: &RunSignalRow, candidate: &RunSignalRow) -> f64 {
    let pairs = [
        (baseline.stats.min(), candidate.stats.min()),
        (baseline.stats.max(), candidate.stats.max()),
        (baseline.stats.mean(), candidate.stats.mean()),
        (baseline.stats.rms(), candidate.stats.rms()),
    ];
    pairs
        .into_iter()
        .map(|pair| match pair {
            (Some(a), Some(b)) => (a - b).abs(),
            (None, None) => 0.0,
            _ => f64::INFINITY,
        })
        .fold(0.0, f64::max)
}

/// One metric of one stage, as both runs recorded it: `None` on a side means
/// that run did not record it at all.
type MetricPair = ((i32, String), (Option<f64>, Option<f64>));

/// Every `(stage, metric)` either side recorded, paired.
fn metric_pairs(
    baseline: &[sp_store::RunStageRow],
    candidate: &[sp_store::RunStageRow],
) -> Vec<MetricPair> {
    let collect = |rows: &[sp_store::RunStageRow]| -> BTreeMap<(i32, String), f64> {
        rows.iter()
            .flat_map(|row| {
                row.metrics
                    .iter()
                    .map(move |(name, value)| ((row.stage_ordinal, name.clone()), *value))
            })
            .collect()
    };
    let left = collect(baseline);
    let right = collect(candidate);

    let mut keys: BTreeSet<(i32, String)> = left.keys().cloned().collect();
    keys.extend(right.keys().cloned());
    keys.into_iter()
        .map(|key| {
            let values = (left.get(&key).copied(), right.get(&key).copied());
            (key, values)
        })
        .collect()
}

/// Artifacts matched by `(stage, port)` and diffed field by field.
fn diff_artifacts(
    store: &Store,
    baseline: RunId,
    candidate: RunId,
    group: GroupId,
    options: &DiffOptions,
) -> Result<Vec<ArtifactDiff>> {
    let (base, cand) = store.read(|conn| {
        let read = |run: RunId| -> sp_store::Result<BTreeMap<(i32, String), String>> {
            let mut out = BTreeMap::new();
            for row in runs::run_artifacts(conn, run, Some(group))? {
                let payload = runs::artifact_payload(conn, &row)?;
                out.insert((row.stage_ordinal, row.port.clone()), payload);
            }
            Ok(out)
        };
        Ok((read(baseline)?, read(candidate)?))
    })?;

    let mut keys: BTreeSet<(i32, String)> = base.keys().cloned().collect();
    keys.extend(cand.keys().cloned());

    let mut out = Vec::new();
    for (stage_ordinal, port) in keys {
        let key = (stage_ordinal, port.clone());
        let left = base.get(&key);
        let right = cand.get(&key);
        let mut diff = ArtifactDiff {
            stage_ordinal,
            port,
            fields: Vec::new(),
            within_tolerance: true,
            note: None,
        };
        match (left, right) {
            (Some(left), Some(right)) => match (decode(left), decode(right)) {
                (Ok(left), Ok(right)) => {
                    diff.fields = artifact::diff(&left, &right, options.tolerances.artifact_abs);
                    diff.within_tolerance = diff.fields.iter().all(FieldDiff::is_equal);
                }
                _ => {
                    diff.within_tolerance = false;
                    diff.note = Some("the payload would not decode".to_owned());
                }
            },
            (Some(_), None) => {
                diff.within_tolerance = false;
                diff.note = Some("the run did not publish this artifact".to_owned());
            }
            (None, Some(_)) => {
                diff.within_tolerance = false;
                diff.note = Some("the baseline did not publish this artifact".to_owned());
            }
            (None, None) => continue,
        }
        out.push(diff);
    }
    Ok(out)
}

/// Reads a payload back through a schema inferred from itself, so a diff does
/// not depend on the writing crate being linked in (§10.4).
fn decode(payload: &str) -> std::result::Result<ArtifactData, ()> {
    let value: serde_json::Value = serde_json::from_str(payload).map_err(|_| ())?;
    ArtifactData::from_value(artifact::infer_schema(&value), &value).map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Baseline checks
// ---------------------------------------------------------------------------

/// A run measured against a named baseline: the diff, plus the verdict its
/// tolerances imply.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineReport {
    pub baseline: String,
    pub diff: RunDiff,
}

impl BaselineReport {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.diff.is_clean()
    }

    /// The report as lines, which is what the CLI prints and the results
    /// screen lists.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![if self.passed() {
            format!(
                "baseline '{}': matched over {} group(s)",
                self.baseline,
                self.diff.groups.len()
            )
        } else {
            format!("baseline '{}': {}", self.baseline, self.diff.describe())
        }];
        if !self.diff.same_pipeline {
            lines.push("  note: the pipeline is not the one the baseline ran".to_owned());
        }
        for (group, reasons) in self.diff.deviations() {
            lines.push(format!("  group {}:", group.get()));
            lines.extend(reasons.into_iter().map(|reason| format!("    {reason}")));
        }
        lines
    }
}

/// Compares `run` against the baseline called `name`, under that baseline's
/// own tolerances (§10.4).
pub fn check_baseline(store: &Store, run: RunId, name: &str) -> Result<BaselineReport> {
    let owned = name.to_owned();
    let baseline = store
        .read(move |conn| sp_store::regress::get_baseline_by_name(conn, &owned))
        .map_err(ProcError::Store)?;
    if baseline.run_id == run {
        return Err(ProcError::Store(StoreError::Invalid(format!(
            "run {} is the baseline '{}' itself",
            run.get(),
            baseline.name
        ))));
    }
    let options = DiffOptions::within(baseline.tolerances);
    let diff = diff_runs(store, baseline.run_id, run, &options)?;
    Ok(BaselineReport {
        baseline: baseline.name,
        diff,
    })
}
