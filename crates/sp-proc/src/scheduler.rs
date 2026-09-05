//! Running a pipeline over a dataset (`docs/DESIGN.md` §9.1, §9.5).
//!
//! ```text
//! for each group  (parallel, bounded by in_flight_cap):
//!     frame ← load(group)
//!     for stage in pipeline:                # strictly ordered
//!         key ← blake3(kind, version, params, input_hash)
//!         output ← cache.get(key)  or  stage.process(ctx, frame)
//!         record(run, group, stage, output)
//!         frame ← frame.apply(output)
//! ```
//!
//! Groups are independent, which is what makes them both the unit of
//! parallelism and the unit of retry: one bad group fails on its own without
//! sinking the run.
//!
//! Every stage instance serves exactly one group, because `process` takes
//! `&mut self` and groups run concurrently. `begin_run` therefore runs once
//! per group, and `end_run` runs after the last group on one fresh instance of
//! each stage, whose artifacts are recorded run-level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use sp_core::run::{Disposition, RunStatus, StageStatus};
use sp_core::{DatasetId, GroupId, PipelineId, RunId};
use sp_store::runs::{
    self, CacheHit, NewArtifact, NewRun, NewRunSignal, RunGroupRow, RunStageRow, SOURCE_STAGE,
};
use sp_store::{library, Store};

use crate::cache;
use crate::error::{ProcError, Result, StageError};
use crate::frame::{GroupFrame, SignalRef};
use crate::param::ParamSet;
use crate::pipeline::{resolve, Pipeline, PipelineStage};
use crate::registry::StageRegistry;
use crate::stage::{RunCtx, Stage, StageCtx, StageOutput};

/// How far a run has got. Reported to the UI so the progress bar and the group
/// list move while the run is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProgress {
    pub run: RunId,
    pub groups_done: usize,
    pub groups_total: usize,
    /// The group this update is about.
    pub group: GroupId,
    /// The stage that just finished, or [`SOURCE_STAGE`] for a group that was
    /// only just loaded.
    pub stage_ordinal: i32,
    pub status: StageStatus,
}

impl RunProgress {
    /// Progress as a fraction, for a determinate bar.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.groups_total == 0 {
            return 1.0;
        }
        self.groups_done as f32 / self.groups_total as f32
    }
}

/// Cancellation and progress reporting for one run (§4.2).
#[derive(Clone, Default)]
pub struct RunControl {
    cancel: Option<Arc<AtomicBool>>,
    progress: Option<Arc<dyn Fn(RunProgress) + Send + Sync>>,
}

impl std::fmt::Debug for RunControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunControl")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl RunControl {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn Fn(RunProgress) + Send + Sync>) -> Self {
        self.progress = Some(progress);
        self
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
    }

    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(ProcError::Cancelled);
        }
        Ok(())
    }

    fn report(&self, progress: RunProgress) {
        if let Some(sink) = &self.progress {
            sink(progress);
        }
    }
}

/// Knobs for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// Groups in flight at once. Bounded so memory stays predictable (§4.2).
    pub in_flight_cap: usize,
    /// Whether a stage whose cache key is already recorded may reuse it.
    pub use_cache: bool,
    /// Recorded on the run, for the dataset the groups came from.
    pub dataset_id: Option<DatasetId>,
    pub notes: Option<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            in_flight_cap: std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(2)
                .clamp(1, 8),
            use_cache: true,
            dataset_id: None,
            notes: None,
        }
    }
}

impl RunOptions {
    #[must_use]
    pub fn over_dataset(mut self, dataset_id: DatasetId) -> Self {
        self.dataset_id = Some(dataset_id);
        self
    }

    /// Runs every stage, ignoring recorded output. What "re-run from scratch"
    /// does.
    #[must_use]
    pub fn without_cache(mut self) -> Self {
        self.use_cache = false;
        self
    }

    #[must_use]
    pub fn in_flight(mut self, cap: usize) -> Self {
        self.in_flight_cap = cap.max(1);
        self
    }
}

/// How a finished run turned out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub run: RunId,
    pub status: RunStatus,
    pub groups_ok: usize,
    pub groups_failed: usize,
    /// Stages that actually ran, across every group.
    pub stages_run: usize,
    /// Stages whose output was reused (§9.5).
    pub stages_cached: usize,
    pub wall_ms: u64,
}

impl RunSummary {
    /// One line for the status bar.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{}: {} group(s) ok, {} failed, {} stage(s) run, {} cached, {} ms",
            self.status.label(),
            self.groups_ok,
            self.groups_failed,
            self.stages_run,
            self.stages_cached,
            self.wall_ms
        )
    }
}

/// Runs `pipeline` over `groups`, recording everything (§9.5).
///
/// The pipeline is validated first, so a run either starts with every stage
/// constructible and configured or does not start at all. After that the only
/// errors that end the run early are cancellation and the store itself
/// failing: a stage error fails its group and the run carries on.
pub fn run_pipeline(
    store: &Store,
    registry: &StageRegistry,
    pipeline_id: PipelineId,
    pipeline: &Pipeline,
    groups: &[GroupId],
    options: &RunOptions,
    control: &RunControl,
) -> Result<RunSummary> {
    pipeline.validate(registry)?;
    if groups.is_empty() {
        return Err(ProcError::NoGroups);
    }
    let started = Instant::now();
    let pipeline_hash = pipeline.hash(registry)?;

    let mut new_run = NewRun::new(pipeline_id, pipeline_hash.clone());
    new_run.dataset_id = options.dataset_id;
    new_run.notes.clone_from(&options.notes);
    let run = store.write(move |conn| runs::begin_run(conn, &new_run))?;
    tracing::info!(
        run = run.get(),
        groups = groups.len(),
        "pipeline run started"
    );

    let ctx = RunEnv {
        store: store.clone(),
        registry,
        pipeline,
        pipeline_hash,
        run,
        options: options.clone(),
        control: control.clone(),
        groups_total: groups.len(),
    };

    let outcomes = run_groups(&ctx, groups);

    let mut summary = RunSummary {
        run,
        status: RunStatus::Ok,
        groups_ok: 0,
        groups_failed: 0,
        stages_run: 0,
        stages_cached: 0,
        wall_ms: 0,
    };
    let mut store_error = None;
    for outcome in outcomes {
        match outcome {
            Ok(group) => {
                summary.stages_run += group.stages_run;
                summary.stages_cached += group.stages_cached;
                if group.status == RunStatus::Ok {
                    summary.groups_ok += 1;
                } else {
                    summary.groups_failed += 1;
                }
            }
            Err(error) => {
                summary.groups_failed += 1;
                tracing::error!(%error, "a group could not be recorded");
                store_error.get_or_insert(error);
            }
        }
    }

    if summary.groups_failed == 0 && !control.is_cancelled() {
        record_run_artifacts(&ctx)?;
    }

    summary.status = if control.is_cancelled() {
        RunStatus::Cancelled
    } else if summary.groups_failed > 0 {
        RunStatus::Failed
    } else {
        RunStatus::Ok
    };
    let status = summary.status;
    store.write(move |conn| runs::finish_run(conn, run, status))?;
    summary.wall_ms = started.elapsed().as_millis() as u64;

    if let Some(error) = store_error {
        // Every group's outcome is on record; the run is closed. The caller
        // still needs to know the library, not the algorithm, is what broke.
        if matches!(error, ProcError::Cancelled) {
            return Ok(summary);
        }
        return Err(error);
    }
    tracing::info!(
        run = run.get(),
        summary = summary.describe(),
        "pipeline run finished"
    );
    Ok(summary)
}

/// Everything one group's worker needs.
struct RunEnv<'a> {
    store: Store,
    registry: &'a StageRegistry,
    pipeline: &'a Pipeline,
    pipeline_hash: String,
    run: RunId,
    options: RunOptions,
    control: RunControl,
    groups_total: usize,
}

#[derive(Debug)]
struct GroupOutcome {
    status: RunStatus,
    stages_run: usize,
    stages_cached: usize,
}

/// Fans out over groups, bounded by `in_flight_cap`. Stage order within a
/// group stays strict; only whole groups overlap.
fn run_groups(ctx: &RunEnv<'_>, groups: &[GroupId]) -> Vec<Result<GroupOutcome>> {
    use rayon::prelude::*;

    let done = std::sync::atomic::AtomicUsize::new(0);
    let work = |&group: &GroupId| -> Result<GroupOutcome> {
        let outcome = run_group(ctx, group);
        let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
        ctx.control.report(RunProgress {
            run: ctx.run,
            groups_done: finished,
            groups_total: ctx.groups_total,
            group,
            stage_ordinal: last_ordinal(ctx.pipeline),
            status: match &outcome {
                Ok(o) if o.status == RunStatus::Ok => StageStatus::Ok,
                Ok(_) => StageStatus::Failed,
                Err(_) => StageStatus::Failed,
            },
        });
        outcome
    };

    match rayon::ThreadPoolBuilder::new()
        .num_threads(ctx.options.in_flight_cap)
        .build()
    {
        Ok(pool) => pool.install(|| groups.par_iter().map(work).collect()),
        // A pool is an optimisation, not a requirement: without one the run is
        // sequential rather than failed.
        Err(error) => {
            tracing::warn!(%error, "running groups sequentially");
            groups.iter().map(work).collect()
        }
    }
}

fn last_ordinal(pipeline: &Pipeline) -> i32 {
    pipeline
        .enabled()
        .last()
        .map_or(SOURCE_STAGE, |(ordinal, _)| ordinal as i32)
}

/// One group through every enabled stage, in order.
fn run_group(ctx: &RunEnv<'_>, group_id: GroupId) -> Result<GroupOutcome> {
    ctx.control.check()?;
    let started = Instant::now();
    let store = &ctx.store;

    let mut frame = load_frame(ctx, group_id)?;
    record_source(ctx, &frame)?;

    // The group is on record as running before its first stage, so a run that
    // is killed outright still says which groups it had reached.
    let run = ctx.run;
    store.write(move |conn| {
        runs::record_group(
            conn,
            run,
            &RunGroupRow {
                group_id,
                status: RunStatus::Running,
                wall_ms: None,
                message: None,
            },
        )
    })?;

    let mut stages = build_stages(ctx)?;
    let mut outcome = GroupOutcome {
        status: RunStatus::Ok,
        stages_run: 0,
        stages_cached: 0,
    };

    for (position, (ordinal, stage)) in ctx.pipeline.enabled().enumerate() {
        ctx.control.check()?;
        let ordinal = ordinal as i32;
        let (instance, params) = &mut stages[position];
        let descriptor = instance.descriptor();

        let input_hash = cache::input_hash(&frame);
        let key = cache::stage_key(
            descriptor.kind,
            descriptor.version,
            &params.to_json(),
            &input_hash,
        );

        let cached = if ctx.options.use_cache && descriptor.pure {
            reuse_cached(ctx, group_id, ordinal, &key, &mut frame)?
        } else {
            false
        };
        if cached {
            outcome.stages_cached += 1;
            report_stage(ctx, group_id, ordinal, StageStatus::Cached);
            continue;
        }

        let stage_ctx = StageCtx::new(
            RunCtx::new(ctx.run, ctx.pipeline_hash.clone(), ordinal)
                .with_cancel(ctx.control.cancel_flag()),
        )
        .labelled(stage.label.clone());

        let began = Instant::now();
        match instance.process(&stage_ctx, &frame) {
            Ok(output) => {
                let wall_ms = began.elapsed().as_millis() as u64;
                match frame.apply(&output, ordinal) {
                    Ok(next) => {
                        frame = record_stage_output(
                            ctx, group_id, ordinal, stage, &frame, &output, next, &key, wall_ms,
                        )?;
                        outcome.stages_run += 1;
                        report_stage(ctx, group_id, ordinal, StageStatus::Ok);
                    }
                    Err(error) => {
                        fail_stage(ctx, group_id, ordinal, &key, &error)?;
                        outcome.status = RunStatus::Failed;
                        break;
                    }
                }
            }
            Err(error) => {
                let cancelled = matches!(error, StageError::Cancelled);
                fail_stage(ctx, group_id, ordinal, &key, &error)?;
                outcome.status = if cancelled {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                };
                break;
            }
        }
    }

    let wall_ms = started.elapsed().as_millis() as u64;
    let status = outcome.status;
    ctx.store.write(move |conn| {
        runs::record_group(
            conn,
            run,
            &RunGroupRow {
                group_id,
                status,
                wall_ms: Some(wall_ms),
                message: None,
            },
        )
    })?;
    Ok(outcome)
}

/// Loads a group's metadata and lazy signal handles.
fn load_frame(ctx: &RunEnv<'_>, group_id: GroupId) -> Result<GroupFrame> {
    let store = &ctx.store;
    let (meta, signals) = store.read(|conn| {
        let group = library::get_group(conn, group_id)?;
        let signals = library::list_signals(conn, group_id)?;
        let mut with_blobs = Vec::with_capacity(signals.len());
        for signal in signals {
            let blob = library::signal_blob(conn, signal.id)?;
            with_blobs.push((signal, blob));
        }
        Ok((group.meta(), with_blobs))
    })?;

    let mut refs = Vec::with_capacity(signals.len());
    for (signal, blob) in &signals {
        refs.push(
            SignalRef::from_library(store, signal, *blob)
                .map_err(|error| ProcError::Store(store_error(error)))?,
        );
    }
    Ok(GroupFrame::new(meta, refs, ctx.run))
}

/// A stage error that came out of the store on the way in, unwrapped so the
/// run reports the library problem rather than a stage problem.
fn store_error(error: StageError) -> sp_store::StoreError {
    match error {
        StageError::Store(inner) => inner,
        other => sp_store::StoreError::Corrupt(other.to_string()),
    }
}

/// Records the signals as they entered, under [`SOURCE_STAGE`], sharing the
/// library's blobs.
fn record_source(ctx: &RunEnv<'_>, frame: &GroupFrame) -> Result<()> {
    let run = ctx.run;
    let group_id = frame.group.id;
    let rows: Vec<NewRunSignal> = frame
        .signals
        .iter()
        .enumerate()
        .map(|(ordinal, signal)| {
            let mut row = NewRunSignal::new(
                group_id,
                SOURCE_STAGE,
                ordinal as u32,
                signal.name(),
                signal.timebase(),
            )
            .with_domain(signal.domain())
            .with_disposition(Disposition::Passthrough)
            .with_attributes(signal.attributes().clone());
            if let Some(blob) = signal.blob_id() {
                row = row.sharing_blob(blob, signal.sample_count(), *signal.stats());
            }
            row
        })
        .collect();

    ctx.store.write(move |conn| {
        for row in &rows {
            runs::record_signal(conn, run, row)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Builds and configures one instance of every enabled stage, for one group.
fn build_stages(ctx: &RunEnv<'_>) -> Result<Vec<(Box<dyn Stage>, ParamSet)>> {
    let mut stages = Vec::new();
    for (ordinal, stage) in ctx.pipeline.enabled() {
        let descriptor = ctx
            .registry
            .descriptor(&stage.kind)
            .ok_or_else(|| ProcError::UnknownStage(stage.kind.clone()))?;
        let params = resolve(stage, descriptor, ordinal)?;
        let mut instance = ctx
            .registry
            .create(&stage.kind)
            .ok_or_else(|| ProcError::UnknownStage(stage.kind.clone()))?;
        instance
            .configure(&params)
            .map_err(|source| invalid(stage, ordinal, source))?;
        let run_ctx = RunCtx::new(ctx.run, ctx.pipeline_hash.clone(), ordinal as i32)
            .with_cancel(ctx.control.cancel_flag());
        if let Err(error) = instance.begin_run(&run_ctx) {
            return Err(ProcError::Store(store_error(error)));
        }
        stages.push((instance, params));
    }
    Ok(stages)
}

fn invalid(stage: &PipelineStage, ordinal: usize, source: crate::error::ConfigError) -> ProcError {
    ProcError::Invalid(vec![crate::pipeline::PipelineIssue::BadParams {
        ordinal,
        label: stage.label.clone().unwrap_or_else(|| stage.kind.clone()),
        source,
    }])
}

/// Copies a cache hit's recorded output into this run and rebuilds the frame
/// from it. `false` when the key is not published, or its rows are gone.
fn reuse_cached(
    ctx: &RunEnv<'_>,
    group_id: GroupId,
    ordinal: i32,
    key: &str,
    frame: &mut GroupFrame,
) -> Result<bool> {
    let run = ctx.run;
    let key_owned = key.to_owned();
    let hit: Option<CacheHit> = ctx
        .store
        .read(|conn| runs::cache_lookup(conn, &key_owned))?;
    let Some(hit) = hit else {
        return Ok(false);
    };

    // The metrics and diagnostics come with it: a cached stage has to look
    // like one that ran everywhere except its status (§9.5).
    let recorded = ctx
        .store
        .read(|conn| runs::get_stage(conn, hit.run_id, hit.group_id, hit.stage_ordinal))?;

    let key_owned = key.to_owned();
    let copied = ctx.store.write(move |conn| {
        let tx = conn.unchecked_transaction()?;
        let copied = runs::copy_cached_output(&tx, hit, run, group_id, ordinal)?;
        if copied > 0 {
            let mut row = RunStageRow::new(group_id, ordinal, StageStatus::Cached);
            row.cache_key = key_owned;
            row.wall_ms = Some(0);
            if let Some(source) = recorded {
                row.metrics = source.metrics;
                row.diagnostics = source.diagnostics;
            }
            runs::record_stage(&tx, run, &row)?;
        }
        tx.commit()?;
        Ok(copied)
    })?;
    if copied == 0 {
        return Ok(false);
    }

    *frame = frame_from_records(ctx, group_id, ordinal, frame)?;
    Ok(true)
}

/// Rebuilds a frame from what is recorded at (group, stage): the signals that
/// flow onward, plus the artifacts this stage published.
fn frame_from_records(
    ctx: &RunEnv<'_>,
    group_id: GroupId,
    ordinal: i32,
    previous: &GroupFrame,
) -> Result<GroupFrame> {
    let run = ctx.run;
    let (rows, artifacts) = ctx.store.read(|conn| {
        Ok((
            runs::stage_signals(conn, run, group_id, ordinal)?,
            runs::stage_artifacts(conn, run, Some(group_id), ordinal)?,
        ))
    })?;

    let mut signals = Vec::new();
    for row in rows.iter().filter(|r| r.disposition.flows_onward()) {
        signals.push(
            SignalRef::from_recorded(&ctx.store, row)
                .map_err(|error| ProcError::Store(store_error(error)))?,
        );
    }

    let mut inbound = previous.inbound.clone();
    for artifact in &artifacts {
        let payload = ctx
            .store
            .read(|conn| runs::artifact_payload(conn, artifact))?;
        inbound.publish(crate::frame::PortValue {
            port: artifact.port.clone(),
            kind: artifact.kind.clone(),
            kind_version: artifact.kind_version,
            payload_json: payload,
            summary: artifact.summary.clone(),
            stage_ordinal: ordinal,
        });
    }

    Ok(GroupFrame {
        group: previous.group.clone(),
        signals,
        inbound,
        run,
    })
}

/// Writes one stage's output and returns the frame the next stage sees.
///
/// When the samples were kept, the returned frame reads them back out of the
/// store rather than holding them: a long pipeline then costs one group's
/// worth of memory rather than one per stage.
#[allow(clippy::too_many_arguments)]
fn record_stage_output(
    ctx: &RunEnv<'_>,
    group_id: GroupId,
    ordinal: i32,
    stage: &PipelineStage,
    before: &GroupFrame,
    output: &StageOutput,
    after: GroupFrame,
    key: &str,
    wall_ms: u64,
) -> Result<GroupFrame> {
    let run = ctx.run;
    let keep_samples = stage.retention.keeps_samples(false);
    let dispositions = GroupFrame::dispositions(output);

    let mut rows: Vec<NewRunSignal> = Vec::with_capacity(after.signals.len());
    for (ordinal_out, (signal, disposition)) in after.signals.iter().zip(dispositions).enumerate() {
        rows.push(signal_row(
            group_id,
            ordinal,
            ordinal_out as u32,
            signal,
            disposition,
            keep_samples,
        ));
    }
    // A dropped signal has no position in the next frame, so it is recorded
    // after the survivors — still inspectable at this stage, gone downstream.
    let mut extra = after.signals.len();
    for out in &output.signals {
        if let crate::stage::SignalOut::Drop { ordinal: input } = out {
            let signal = before.signals.get(*input).ok_or_else(|| {
                ProcError::Store(sp_store::StoreError::Corrupt(format!(
                    "stage dropped signal {input}, which the group does not have"
                )))
            })?;
            rows.push(signal_row(
                group_id,
                ordinal,
                extra as u32,
                signal,
                Disposition::Dropped,
                keep_samples,
            ));
            extra += 1;
        }
    }

    let artifacts: Vec<NewArtifact> = output
        .artifacts
        .iter()
        .map(|artifact| {
            let mut new = NewArtifact::new(
                ordinal,
                artifact.port.clone(),
                artifact.kind.clone(),
                artifact.kind_version,
                artifact.payload_json.clone(),
            )
            .for_group(group_id);
            new.summary.clone_from(&artifact.summary);
            new
        })
        .collect();

    let mut stage_row = RunStageRow::new(group_id, ordinal, StageStatus::Ok);
    stage_row.cache_key = key.to_owned();
    stage_row.wall_ms = Some(wall_ms);
    stage_row.metrics.clone_from(&output.metrics);
    stage_row.diagnostics.clone_from(&output.diagnostics);

    let publish_cache =
        keep_samples && ctx.registry.descriptor(&stage.kind).is_some_and(|d| d.pure);
    let key_owned = key.to_owned();

    ctx.store.write(move |conn| {
        let tx = conn.unchecked_transaction()?;
        for row in &rows {
            runs::record_signal(&tx, run, row)?;
        }
        for artifact in &artifacts {
            runs::insert_artifact(&tx, run, artifact)?;
        }
        runs::record_stage(&tx, run, &stage_row)?;
        if publish_cache {
            runs::cache_put(
                &tx,
                &key_owned,
                CacheHit {
                    run_id: run,
                    group_id,
                    stage_ordinal: ordinal,
                },
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;

    if keep_samples {
        frame_from_records(ctx, group_id, ordinal, &after)
    } else {
        Ok(after)
    }
}

fn signal_row(
    group_id: GroupId,
    stage_ordinal: i32,
    signal_ordinal: u32,
    signal: &SignalRef,
    disposition: Disposition,
    keep_samples: bool,
) -> NewRunSignal {
    let mut row = NewRunSignal::new(
        group_id,
        stage_ordinal,
        signal_ordinal,
        signal.name(),
        signal.timebase(),
    )
    .with_domain(signal.domain())
    .with_disposition(disposition)
    .with_attributes(signal.attributes().clone());
    row.sample_count = signal.sample_count();
    row.stats = Some(*signal.stats());

    if keep_samples {
        match (signal.buffer(), signal.blob_id()) {
            (Some(buffer), _) => row = row.with_samples(buffer.clone()),
            (None, Some(blob)) => {
                row = row.sharing_blob(blob, signal.sample_count(), *signal.stats());
            }
            (None, None) => {}
        }
    }
    row
}

/// Records a stage's failure and leaves the rest of the run alone (§9.1).
fn fail_stage(
    ctx: &RunEnv<'_>,
    group_id: GroupId,
    ordinal: i32,
    key: &str,
    error: &StageError,
) -> Result<()> {
    tracing::warn!(group = group_id.get(), stage = ordinal, %error, "stage failed");
    let run = ctx.run;
    let mut row = RunStageRow::new(group_id, ordinal, StageStatus::Failed);
    row.cache_key = key.to_owned();
    row.message = Some(error.to_string());
    ctx.store
        .write(move |conn| runs::record_stage(conn, run, &row))?;
    report_stage(ctx, group_id, ordinal, StageStatus::Failed);
    Ok(())
}

fn report_stage(ctx: &RunEnv<'_>, group: GroupId, stage_ordinal: i32, status: StageStatus) {
    ctx.control.report(RunProgress {
        run: ctx.run,
        groups_done: 0,
        groups_total: ctx.groups_total,
        group,
        stage_ordinal,
        status,
    });
}

/// Gives every stage a chance to emit run-level output after the last group
/// (§9.2). The instance is fresh: the ones that processed groups belong to
/// those groups.
fn record_run_artifacts(ctx: &RunEnv<'_>) -> Result<()> {
    for (ordinal, stage) in ctx.pipeline.enabled() {
        let descriptor = ctx
            .registry
            .descriptor(&stage.kind)
            .ok_or_else(|| ProcError::UnknownStage(stage.kind.clone()))?;
        let params = resolve(stage, descriptor, ordinal)?;
        let Some(mut instance) = ctx.registry.create(&stage.kind) else {
            continue;
        };
        if instance.configure(&params).is_err() {
            continue;
        }
        let run_ctx = RunCtx::new(ctx.run, ctx.pipeline_hash.clone(), ordinal as i32)
            .with_cancel(ctx.control.cancel_flag());
        let output = match instance.end_run(&run_ctx) {
            Ok(output) => output,
            Err(error) => {
                tracing::warn!(stage = ordinal, %error, "end_run failed");
                continue;
            }
        };
        if output.artifacts.is_empty() {
            continue;
        }
        let run = ctx.run;
        let ordinal = ordinal as i32;
        let artifacts: Vec<NewArtifact> = output
            .artifacts
            .iter()
            .map(|artifact| {
                let mut new = NewArtifact::new(
                    ordinal,
                    artifact.port.clone(),
                    artifact.kind.clone(),
                    artifact.kind_version,
                    artifact.payload_json.clone(),
                );
                new.summary.clone_from(&artifact.summary);
                new
            })
            .collect();
        ctx.store.write(move |conn| {
            for artifact in &artifacts {
                runs::insert_artifact(conn, run, artifact)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
