//! Pipelines and the runs they produce (`docs/DESIGN.md` §9.6).
//!
//! Everything a run records goes through this module: the group and stage
//! rows, the signals as they existed at the output of each stage, and the
//! artifacts a stage emitted. Nothing here knows what a stage *is* — that is
//! `sp-proc`'s job — so the vocabulary is the plain tokens from
//! [`sp_core::run`].
//!
//! Two things are worth knowing before reading on:
//!
//! - **`stage_ordinal = -1` is the source.** The signals entering the pipeline
//!   are recorded like any other stage's output, so "before" needs no special
//!   case anywhere upstream of the schema.
//! - **Sample blobs are shared, not copied.** A passthrough signal records the
//!   blob its input already used and takes a reference on it, so an unchanged
//!   signal costs one row regardless of how long the pipeline is.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Row};
use sp_core::run::{Disposition, Retention, RunStatus, StageStatus};
use sp_core::time::{now_utc, SampleRange, Timestamp};
use sp_core::{
    ArtifactId, Attributes, DatasetId, Diagnostic, Domain, GroupId, PipelineId, RunId,
    SampleBuffer, SignalStats, Timebase,
};

use crate::blob::{self, BlobId};
use crate::error::{Result, StoreError};
use crate::library::{
    attributes_json, format_timestamp, parse_attributes, parse_timestamp, stats_columns,
    stats_from_columns,
};

/// The `stage_ordinal` under which a group's untouched source signals are
/// recorded.
pub const SOURCE_STAGE: i32 = -1;

// ---------------------------------------------------------------------------
// Pipelines
// ---------------------------------------------------------------------------

/// A pipeline to save: the header row only, stages are set separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPipeline {
    pub name: String,
    pub notes: Option<String>,
}

impl NewPipeline {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            notes: None,
        }
    }

    #[must_use]
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }
}

/// A saved pipeline's header row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineRow {
    pub id: PipelineId,
    pub name: String,
    pub notes: Option<String>,
    pub created_utc: Timestamp,
}

/// One stage of a saved pipeline. `params_json` is opaque here: only the stage
/// implementation knows how to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineStageRow {
    pub ordinal: u32,
    pub stage_kind: String,
    pub label: Option<String>,
    pub params_json: String,
    pub enabled: bool,
    pub retention: Retention,
}

impl PipelineStageRow {
    #[must_use]
    pub fn new(ordinal: u32, stage_kind: impl Into<String>) -> Self {
        Self {
            ordinal,
            stage_kind: stage_kind.into(),
            label: None,
            params_json: "{}".into(),
            enabled: true,
            retention: Retention::Always,
        }
    }

    #[must_use]
    pub fn with_params_json(mut self, params_json: impl Into<String>) -> Self {
        self.params_json = params_json.into();
        self
    }

    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }
}

pub fn insert_pipeline(conn: &Connection, pipeline: &NewPipeline) -> Result<PipelineId> {
    conn.execute(
        "INSERT INTO pipeline (name, notes, created_utc) VALUES (?1, ?2, ?3)",
        params![pipeline.name, pipeline.notes, format_timestamp(now_utc())?],
    )?;
    Ok(PipelineId::new(conn.last_insert_rowid()))
}

const PIPELINE_COLUMNS: &str = "id, name, notes, created_utc";

fn pipeline_from_row(row: &Row<'_>) -> Result<PipelineRow> {
    Ok(PipelineRow {
        id: PipelineId::new(row.get(0)?),
        name: row.get(1)?,
        notes: row.get(2)?,
        created_utc: parse_timestamp(&row.get::<_, String>(3)?)?,
    })
}

pub fn get_pipeline(conn: &Connection, id: PipelineId) -> Result<PipelineRow> {
    conn.query_row_and_then(
        &format!("SELECT {PIPELINE_COLUMNS} FROM pipeline WHERE id = ?1"),
        [id.get()],
        pipeline_from_row,
    )
    .map_err(|e| not_found_as(e, "pipeline", id.get()))
}

pub fn list_pipelines(conn: &Connection) -> Result<Vec<PipelineRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PIPELINE_COLUMNS} FROM pipeline ORDER BY name"
    ))?;
    let rows = stmt.query_and_then([], pipeline_from_row)?;
    rows.collect()
}

/// Replaces a pipeline's stage list wholesale. Editing a pipeline is always
/// "here is the new list": stage ordinals are positions, so patching them
/// individually would need renumbering anyway.
pub fn set_pipeline_stages(
    conn: &Connection,
    id: PipelineId,
    stages: &[PipelineStageRow],
) -> Result<()> {
    conn.execute(
        "DELETE FROM pipeline_stage WHERE pipeline_id = ?1",
        [id.get()],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO pipeline_stage
             (pipeline_id, ordinal, stage_kind, label, params_json, enabled, retention)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for stage in stages {
        stmt.execute(params![
            id.get(),
            stage.ordinal,
            stage.stage_kind,
            stage.label,
            stage.params_json,
            stage.enabled,
            stage.retention.as_str(),
        ])?;
    }
    Ok(())
}

pub fn pipeline_stages(conn: &Connection, id: PipelineId) -> Result<Vec<PipelineStageRow>> {
    let mut stmt = conn.prepare(
        "SELECT ordinal, stage_kind, label, params_json, enabled, retention
         FROM pipeline_stage WHERE pipeline_id = ?1 ORDER BY ordinal",
    )?;
    let rows = stmt.query_and_then([id.get()], |row| {
        Ok(PipelineStageRow {
            ordinal: row.get(0)?,
            stage_kind: row.get(1)?,
            label: row.get(2)?,
            params_json: row.get(3)?,
            enabled: row.get(4)?,
            retention: row.get::<_, String>(5)?.parse()?,
        })
    })?;
    rows.collect()
}

/// Renames a saved pipeline. What a pipeline is called changes no result, so
/// this leaves its runs and their hashes alone.
pub fn rename_pipeline(conn: &Connection, id: PipelineId, name: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE pipeline SET name = ?1 WHERE id = ?2",
        params![name, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("pipeline", id.get()));
    }
    Ok(())
}

/// Deletes a pipeline and every run of it, releasing the runs' blobs.
pub fn delete_pipeline(conn: &Connection, id: PipelineId) -> Result<()> {
    for run in list_runs(conn, Some(id))? {
        delete_run(conn, run.id)?;
    }
    let deleted = conn.execute("DELETE FROM pipeline WHERE id = ?1", [id.get()])?;
    if deleted == 0 {
        return Err(StoreError::not_found("pipeline", id.get()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// What to record when a run starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    pub pipeline_id: PipelineId,
    pub dataset_id: Option<DatasetId>,
    /// Kinds, versions and parameters, canonicalised (§9.2). Two runs sharing
    /// a hash ran the same algorithm.
    pub pipeline_hash: String,
    pub app_version: String,
    pub notes: Option<String>,
}

impl NewRun {
    #[must_use]
    pub fn new(pipeline_id: PipelineId, pipeline_hash: impl Into<String>) -> Self {
        Self {
            pipeline_id,
            dataset_id: None,
            pipeline_hash: pipeline_hash.into(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            notes: None,
        }
    }

    #[must_use]
    pub fn over_dataset(mut self, dataset_id: DatasetId) -> Self {
        self.dataset_id = Some(dataset_id);
        self
    }

    #[must_use]
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }
}

/// A recorded run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: RunId,
    pub pipeline_id: PipelineId,
    pub dataset_id: Option<DatasetId>,
    pub started_utc: Timestamp,
    pub finished_utc: Option<Timestamp>,
    pub status: RunStatus,
    pub pipeline_hash: String,
    pub app_version: String,
    pub notes: Option<String>,
}

/// Opens a run in `running` state. Every later record refers to the returned
/// id, so this is the first thing the scheduler does.
pub fn begin_run(conn: &Connection, run: &NewRun) -> Result<RunId> {
    conn.execute(
        "INSERT INTO run (pipeline_id, dataset_id, started_utc, finished_utc, status,
                          pipeline_hash, app_version, notes)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7)",
        params![
            run.pipeline_id.get(),
            run.dataset_id.map(DatasetId::get),
            format_timestamp(now_utc())?,
            RunStatus::Running.as_str(),
            run.pipeline_hash,
            run.app_version,
            run.notes,
        ],
    )?;
    Ok(RunId::new(conn.last_insert_rowid()))
}

/// Closes a run with its final status and stamps the finish time.
pub fn finish_run(conn: &Connection, id: RunId, status: RunStatus) -> Result<()> {
    let changed = conn.execute(
        "UPDATE run SET status = ?1, finished_utc = ?2 WHERE id = ?3",
        params![status.as_str(), format_timestamp(now_utc())?, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("run", id.get()));
    }
    Ok(())
}

const RUN_COLUMNS: &str = "id, pipeline_id, dataset_id, started_utc, finished_utc, status,
     pipeline_hash, app_version, notes";

fn run_from_row(row: &Row<'_>) -> Result<RunRow> {
    let finished: Option<String> = row.get(4)?;
    Ok(RunRow {
        id: RunId::new(row.get(0)?),
        pipeline_id: PipelineId::new(row.get(1)?),
        dataset_id: row.get::<_, Option<i64>>(2)?.map(DatasetId::new),
        started_utc: parse_timestamp(&row.get::<_, String>(3)?)?,
        finished_utc: finished.as_deref().map(parse_timestamp).transpose()?,
        status: row.get::<_, String>(5)?.parse()?,
        pipeline_hash: row.get(6)?,
        app_version: row.get(7)?,
        notes: row.get(8)?,
    })
}

pub fn get_run(conn: &Connection, id: RunId) -> Result<RunRow> {
    conn.query_row_and_then(
        &format!("SELECT {RUN_COLUMNS} FROM run WHERE id = ?1"),
        [id.get()],
        run_from_row,
    )
    .map_err(|e| not_found_as(e, "run", id.get()))
}

/// Runs newest first, optionally narrowed to one pipeline.
pub fn list_runs(conn: &Connection, pipeline: Option<PipelineId>) -> Result<Vec<RunRow>> {
    match pipeline {
        Some(id) => {
            let mut stmt = conn.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM run WHERE pipeline_id = ?1 ORDER BY id DESC"
            ))?;
            let rows = stmt.query_and_then([id.get()], run_from_row)?;
            rows.collect()
        }
        None => {
            let mut stmt =
                conn.prepare(&format!("SELECT {RUN_COLUMNS} FROM run ORDER BY id DESC"))?;
            let rows = stmt.query_and_then([], run_from_row)?;
            rows.collect()
        }
    }
}

/// Deletes a run and releases every blob it held a reference on. Cascades take
/// the rows; the refcounts have to be walked by hand first, because a blob may
/// be shared with a signal in the library or with another run.
///
/// A run a baseline names is refused: the baseline's golden data *is* the
/// run's rows (§10.4), so deleting it would leave every later comparison with
/// nothing to compare against. Drop the baseline first, or promote another run
/// under the same name.
pub fn delete_run(conn: &Connection, id: RunId) -> Result<()> {
    let named = crate::regress::baselines_of_run(conn, id)?;
    if let Some(baseline) = named.first() {
        return Err(StoreError::invalid(format!(
            "run {} is the baseline '{}'",
            id.get(),
            baseline.name
        )));
    }
    let mut blobs = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT blob_id FROM run_signal WHERE run_id = ?1 AND blob_id IS NOT NULL
             UNION ALL
             SELECT blob_id FROM artifact WHERE run_id = ?1 AND blob_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([id.get()], |row| row.get::<_, i64>(0))?;
        for row in rows {
            blobs.push(BlobId::new(row?));
        }
    }
    let deleted = conn.execute("DELETE FROM run WHERE id = ?1", [id.get()])?;
    if deleted == 0 {
        return Err(StoreError::not_found("run", id.get()));
    }
    for blob_id in blobs {
        blob::release(conn, blob_id)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Group and stage records
// ---------------------------------------------------------------------------

/// How one group of a run turned out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGroupRow {
    pub group_id: GroupId,
    pub status: RunStatus,
    pub wall_ms: Option<u64>,
    pub message: Option<String>,
}

/// Records a group's outcome, overwriting the `running` row it started with.
pub fn record_group(conn: &Connection, run: RunId, group: &RunGroupRow) -> Result<()> {
    conn.execute(
        "INSERT INTO run_group (run_id, group_id, status, wall_ms, message)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(run_id, group_id) DO UPDATE SET
             status = excluded.status,
             wall_ms = excluded.wall_ms,
             message = excluded.message",
        params![
            run.get(),
            group.group_id.get(),
            group.status.as_str(),
            group.wall_ms.map(|ms| ms as i64),
            group.message,
        ],
    )?;
    Ok(())
}

pub fn run_groups(conn: &Connection, run: RunId) -> Result<Vec<RunGroupRow>> {
    let mut stmt = conn.prepare(
        "SELECT group_id, status, wall_ms, message FROM run_group
         WHERE run_id = ?1 ORDER BY group_id",
    )?;
    let rows = stmt.query_and_then([run.get()], |row| {
        Ok(RunGroupRow {
            group_id: GroupId::new(row.get(0)?),
            status: row.get::<_, String>(1)?.parse()?,
            wall_ms: row.get::<_, Option<i64>>(2)?.map(|ms| ms.max(0) as u64),
            message: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// What one stage did to one group.
#[derive(Debug, Clone, PartialEq)]
pub struct RunStageRow {
    pub group_id: GroupId,
    pub stage_ordinal: i32,
    pub status: StageStatus,
    pub wall_ms: Option<u64>,
    pub cache_key: String,
    pub metrics: BTreeMap<String, f64>,
    pub diagnostics: Vec<Diagnostic>,
    pub message: Option<String>,
}

impl RunStageRow {
    #[must_use]
    pub fn new(group_id: GroupId, stage_ordinal: i32, status: StageStatus) -> Self {
        Self {
            group_id,
            stage_ordinal,
            status,
            wall_ms: None,
            cache_key: String::new(),
            metrics: BTreeMap::new(),
            diagnostics: Vec::new(),
            message: None,
        }
    }
}

pub fn record_stage(conn: &Connection, run: RunId, stage: &RunStageRow) -> Result<()> {
    conn.execute(
        "INSERT INTO run_stage (run_id, group_id, stage_ordinal, status, wall_ms, cache_key,
                                metrics_json, diagnostics_json, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(run_id, group_id, stage_ordinal) DO UPDATE SET
             status = excluded.status,
             wall_ms = excluded.wall_ms,
             cache_key = excluded.cache_key,
             metrics_json = excluded.metrics_json,
             diagnostics_json = excluded.diagnostics_json,
             message = excluded.message",
        params![
            run.get(),
            stage.group_id.get(),
            stage.stage_ordinal,
            stage.status.as_str(),
            stage.wall_ms.map(|ms| ms as i64),
            stage.cache_key,
            serde_json::to_string(&stage.metrics)?,
            serde_json::to_string(&stage.diagnostics)?,
            stage.message,
        ],
    )?;
    Ok(())
}

const STAGE_COLUMNS: &str =
    "group_id, stage_ordinal, status, wall_ms, cache_key, metrics_json, diagnostics_json, message";

fn stage_from_row(row: &Row<'_>) -> Result<RunStageRow> {
    Ok(RunStageRow {
        group_id: GroupId::new(row.get(0)?),
        stage_ordinal: row.get(1)?,
        status: row.get::<_, String>(2)?.parse()?,
        wall_ms: row.get::<_, Option<i64>>(3)?.map(|ms| ms.max(0) as u64),
        cache_key: row.get(4)?,
        metrics: serde_json::from_str(&row.get::<_, String>(5)?)?,
        diagnostics: serde_json::from_str(&row.get::<_, String>(6)?)?,
        message: row.get(7)?,
    })
}

/// Every stage record of a run, in group then stage order.
pub fn run_stages(conn: &Connection, run: RunId) -> Result<Vec<RunStageRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {STAGE_COLUMNS} FROM run_stage WHERE run_id = ?1
         ORDER BY group_id, stage_ordinal"
    ))?;
    let rows = stmt.query_and_then([run.get()], stage_from_row)?;
    rows.collect()
}

/// One stage's record, if it has one.
pub fn get_stage(
    conn: &Connection,
    run: RunId,
    group: GroupId,
    stage_ordinal: i32,
) -> Result<Option<RunStageRow>> {
    conn.query_row_and_then(
        &format!(
            "SELECT {STAGE_COLUMNS} FROM run_stage
             WHERE run_id = ?1 AND group_id = ?2 AND stage_ordinal = ?3"
        ),
        params![run.get(), group.get(), stage_ordinal],
        stage_from_row,
    )
    .map(Some)
    .or_else(|error| match error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        other => Err(other),
    })
}

/// The stage records for one group of a run.
pub fn group_stages(conn: &Connection, run: RunId, group: GroupId) -> Result<Vec<RunStageRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {STAGE_COLUMNS} FROM run_stage WHERE run_id = ?1 AND group_id = ?2
         ORDER BY stage_ordinal"
    ))?;
    let rows = stmt.query_and_then(params![run.get(), group.get()], stage_from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Recorded signals
// ---------------------------------------------------------------------------

/// A signal at the output of one stage, ready to be written.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRunSignal {
    pub group_id: GroupId,
    pub stage_ordinal: i32,
    pub signal_ordinal: u32,
    pub name: String,
    pub domain: Domain,
    pub disposition: Disposition,
    pub timebase: Timebase,
    pub sample_count: u64,
    pub attributes: Attributes,
    /// The samples to write, for a signal this stage produced.
    pub samples: Option<SampleBuffer>,
    /// An existing blob to share instead of writing samples — what a
    /// passthrough records.
    pub blob_id: Option<BlobId>,
    /// Pre-computed summary, for a shared blob whose statistics are already
    /// known. Computed from `samples` when absent.
    pub stats: Option<SignalStats>,
}

impl NewRunSignal {
    #[must_use]
    pub fn new(
        group_id: GroupId,
        stage_ordinal: i32,
        signal_ordinal: u32,
        name: impl Into<String>,
        timebase: Timebase,
    ) -> Self {
        Self {
            group_id,
            stage_ordinal,
            signal_ordinal,
            name: name.into(),
            domain: Domain::Analog,
            disposition: Disposition::Added,
            timebase,
            sample_count: 0,
            attributes: Attributes::new(),
            samples: None,
            blob_id: None,
            stats: None,
        }
    }

    #[must_use]
    pub fn with_domain(mut self, domain: Domain) -> Self {
        self.domain = domain;
        self
    }

    #[must_use]
    pub fn with_disposition(mut self, disposition: Disposition) -> Self {
        self.disposition = disposition;
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }

    /// Records freshly produced samples. The blob is written on insert.
    #[must_use]
    pub fn with_samples(mut self, samples: SampleBuffer) -> Self {
        self.sample_count = samples.len() as u64;
        self.samples = Some(samples);
        self
    }

    /// Records a signal by pointing at a blob that already exists.
    #[must_use]
    pub fn sharing_blob(mut self, blob_id: BlobId, sample_count: u64, stats: SignalStats) -> Self {
        self.blob_id = Some(blob_id);
        self.sample_count = sample_count;
        self.stats = Some(stats);
        self
    }
}

/// A recorded signal row, without its samples.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSignalRow {
    pub group_id: GroupId,
    pub stage_ordinal: i32,
    pub signal_ordinal: u32,
    pub name: String,
    pub domain: Domain,
    pub disposition: Disposition,
    pub timebase: Timebase,
    pub sample_count: u64,
    pub blob_id: Option<BlobId>,
    pub stats: SignalStats,
    pub attributes: Attributes,
}

/// Writes one signal of a stage's output, taking a reference on its blob.
///
/// Passing `samples` writes a column blob, which deduplicates by content: a
/// stage that hands back the bytes it was given costs nothing extra.
pub fn record_signal(conn: &Connection, run: RunId, signal: &NewRunSignal) -> Result<()> {
    let (blob_id, stats) = match (&signal.samples, signal.blob_id) {
        (Some(samples), _) => {
            let stats = signal
                .stats
                .unwrap_or_else(|| sp_core::stats::summarise(samples));
            let id = blob::write_column(conn, samples, signal.timebase, blob::DEFAULT_CHUNK_SIZE)?;
            (Some(id), stats)
        }
        (None, Some(id)) => {
            blob::retain(conn, id)?;
            (Some(id), signal.stats.unwrap_or_default())
        }
        // A dropped signal, or one written under a `never` retention policy:
        // the row still records what happened to it.
        (None, None) => (None, signal.stats.unwrap_or_default()),
    };
    let (min, max, mean, rms) = stats_columns(&stats);

    conn.execute(
        "INSERT INTO run_signal (run_id, group_id, stage_ordinal, signal_ordinal, name, domain,
                                 disposition, blob_id, sample_rate_hz, t0_s, sample_count,
                                 min_value, max_value, mean_value, rms_value, attrs_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            run.get(),
            signal.group_id.get(),
            signal.stage_ordinal,
            signal.signal_ordinal,
            signal.name,
            signal.domain.as_str(),
            signal.disposition.as_str(),
            blob_id.map(BlobId::get),
            signal.timebase.sample_rate_hz,
            signal.timebase.t0_s,
            signal.sample_count as i64,
            min,
            max,
            mean,
            rms,
            attributes_json(&signal.attributes)?,
        ],
    )?;
    Ok(())
}

const RUN_SIGNAL_COLUMNS: &str = "group_id, stage_ordinal, signal_ordinal, name, domain,
     disposition, blob_id, sample_rate_hz, t0_s, sample_count,
     min_value, max_value, mean_value, rms_value, attrs_json";

fn run_signal_from_row(row: &Row<'_>) -> Result<RunSignalRow> {
    let sample_rate_hz: Option<f64> = row.get(7)?;
    let t0_s: f64 = row.get::<_, Option<f64>>(8)?.unwrap_or(0.0);
    let timebase = match sample_rate_hz {
        Some(rate) if rate > 0.0 => Timebase::regular(rate, t0_s),
        _ => Timebase::irregular(t0_s),
    };
    let sample_count: i64 = row.get::<_, Option<i64>>(9)?.unwrap_or(0);
    let sample_count =
        u64::try_from(sample_count).map_err(|_| StoreError::corrupt("negative sample_count"))?;
    Ok(RunSignalRow {
        group_id: GroupId::new(row.get(0)?),
        stage_ordinal: row.get(1)?,
        signal_ordinal: row.get(2)?,
        name: row.get(3)?,
        domain: row.get::<_, String>(4)?.parse()?,
        disposition: row.get::<_, String>(5)?.parse()?,
        blob_id: row.get::<_, Option<i64>>(6)?.map(BlobId::new),
        timebase,
        sample_count,
        stats: stats_from_columns(
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
            sample_count,
            0,
        ),
        attributes: parse_attributes(&row.get::<_, String>(14)?)?,
    })
}

/// The signals as they existed at the output of one stage of one group. Pass
/// [`SOURCE_STAGE`] for the signals that entered the pipeline.
pub fn stage_signals(
    conn: &Connection,
    run: RunId,
    group: GroupId,
    stage_ordinal: i32,
) -> Result<Vec<RunSignalRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RUN_SIGNAL_COLUMNS} FROM run_signal
         WHERE run_id = ?1 AND group_id = ?2 AND stage_ordinal = ?3
         ORDER BY signal_ordinal"
    ))?;
    let rows = stmt.query_and_then(
        params![run.get(), group.get(), stage_ordinal],
        run_signal_from_row,
    )?;
    rows.collect()
}

/// Reads back a recorded signal's samples over `range`.
pub fn read_signal_samples(
    conn: &Connection,
    signal: &RunSignalRow,
    range: SampleRange,
) -> Result<SampleBuffer> {
    let blob_id = signal.blob_id.ok_or_else(|| {
        StoreError::invalid(format!("'{}' was recorded without samples", signal.name))
    })?;
    blob::read_column(conn, blob_id, range)
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// A typed stage output that is not a signal (§10.1), ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewArtifact {
    /// `None` for a run-level artifact, which is what `end_run` emits.
    pub group_id: Option<GroupId>,
    pub stage_ordinal: i32,
    pub port: String,
    pub kind: String,
    pub kind_version: u32,
    pub payload_json: String,
    pub summary: Option<String>,
}

impl NewArtifact {
    #[must_use]
    pub fn new(
        stage_ordinal: i32,
        port: impl Into<String>,
        kind: impl Into<String>,
        kind_version: u32,
        payload_json: impl Into<String>,
    ) -> Self {
        Self {
            group_id: None,
            stage_ordinal,
            port: port.into(),
            kind: kind.into(),
            kind_version,
            payload_json: payload_json.into(),
            summary: None,
        }
    }

    #[must_use]
    pub fn for_group(mut self, group_id: GroupId) -> Self {
        self.group_id = Some(group_id);
        self
    }

    #[must_use]
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

/// A recorded artifact row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRow {
    pub id: ArtifactId,
    pub group_id: Option<GroupId>,
    pub stage_ordinal: i32,
    pub port: String,
    pub kind: String,
    pub kind_version: u32,
    pub payload_json: Option<String>,
    pub blob_id: Option<BlobId>,
    pub summary: Option<String>,
}

/// Payloads at or below this many bytes go inline in `payload_json`; larger
/// ones become a blob, so one enormous spectrogram cannot bloat every query
/// that touches the artifact table.
pub const INLINE_PAYLOAD_LIMIT: usize = 64 * 1024;

pub fn insert_artifact(
    conn: &Connection,
    run: RunId,
    artifact: &NewArtifact,
) -> Result<ArtifactId> {
    let inline = artifact.payload_json.len() <= INLINE_PAYLOAD_LIMIT;
    let blob_id = if inline {
        None
    } else {
        let mut writer =
            blob::BlobWriter::begin(conn, blob::BlobKind::Artifact, blob::DEFAULT_CHUNK_SIZE)?;
        writer.write(artifact.payload_json.as_bytes())?;
        Some(writer.finish()?)
    };

    conn.execute(
        "INSERT INTO artifact (run_id, group_id, stage_ordinal, port, kind, kind_version,
                               payload_json, blob_id, summary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            run.get(),
            artifact.group_id.map(GroupId::get),
            artifact.stage_ordinal,
            artifact.port,
            artifact.kind,
            artifact.kind_version,
            inline.then_some(artifact.payload_json.as_str()),
            blob_id.map(BlobId::get),
            artifact.summary,
        ],
    )?;
    Ok(ArtifactId::new(conn.last_insert_rowid()))
}

const ARTIFACT_COLUMNS: &str =
    "id, group_id, stage_ordinal, port, kind, kind_version, payload_json, blob_id, summary";

fn artifact_from_row(row: &Row<'_>) -> Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: ArtifactId::new(row.get(0)?),
        group_id: row.get::<_, Option<i64>>(1)?.map(GroupId::new),
        stage_ordinal: row.get(2)?,
        port: row.get(3)?,
        kind: row.get(4)?,
        kind_version: row.get(5)?,
        payload_json: row.get(6)?,
        blob_id: row.get::<_, Option<i64>>(7)?.map(BlobId::new),
        summary: row.get(8)?,
    })
}

/// The artifacts one stage emitted for one group, or — with `group` as `None`
/// — the run-level ones.
pub fn stage_artifacts(
    conn: &Connection,
    run: RunId,
    group: Option<GroupId>,
    stage_ordinal: i32,
) -> Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM artifact
         WHERE run_id = ?1 AND stage_ordinal = ?2
           AND group_id IS ?3
         ORDER BY id"
    ))?;
    let rows = stmt.query_and_then(
        params![run.get(), stage_ordinal, group.map(GroupId::get)],
        artifact_from_row,
    )?;
    rows.collect()
}

/// Every artifact of one group of a run, in stage then insertion order — or,
/// with `group` as `None`, the run-level ones.
///
/// Assertions and diffs read a whole group's artifacts at once rather than
/// stage by stage: they are addressed by port, not by where they came from.
pub fn run_artifacts(
    conn: &Connection,
    run: RunId,
    group: Option<GroupId>,
) -> Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM artifact
         WHERE run_id = ?1 AND group_id IS ?2
         ORDER BY stage_ordinal, id"
    ))?;
    let rows = stmt.query_and_then(
        params![run.get(), group.map(GroupId::get)],
        artifact_from_row,
    )?;
    rows.collect()
}

/// The payload of an artifact, wherever it was stored.
pub fn artifact_payload(conn: &Connection, artifact: &ArtifactRow) -> Result<String> {
    match (&artifact.payload_json, artifact.blob_id) {
        (Some(json), _) => Ok(json.clone()),
        (None, Some(blob_id)) => {
            let meta = blob::info(conn, blob_id)?;
            let bytes = blob::read_bytes_with(conn, &meta, 0, meta.byte_len as usize)?;
            String::from_utf8(bytes)
                .map_err(|_| StoreError::corrupt("artifact payload is not UTF-8"))
        }
        (None, None) => Err(StoreError::corrupt(format!(
            "artifact {} has no payload",
            artifact.id
        ))),
    }
}

// ---------------------------------------------------------------------------
// Stage cache
// ---------------------------------------------------------------------------

/// Where a cache key's recorded output lives (§9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheHit {
    pub run_id: RunId,
    pub group_id: GroupId,
    pub stage_ordinal: i32,
}

/// Publishes a cache key, pointing it at output already recorded. A repeated
/// key overwrites: the newest recording of identical output is as good as the
/// oldest, and is the one least likely to be deleted first.
pub fn cache_put(conn: &Connection, key: &str, hit: CacheHit) -> Result<()> {
    conn.execute(
        "INSERT INTO stage_cache (cache_key, run_id, group_id, stage_ordinal, created_utc)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(cache_key) DO UPDATE SET
             run_id = excluded.run_id,
             group_id = excluded.group_id,
             stage_ordinal = excluded.stage_ordinal,
             created_utc = excluded.created_utc",
        params![
            key,
            hit.run_id.get(),
            hit.group_id.get(),
            hit.stage_ordinal,
            format_timestamp(now_utc())?,
        ],
    )?;
    Ok(())
}

pub fn cache_lookup(conn: &Connection, key: &str) -> Result<Option<CacheHit>> {
    let hit = conn
        .query_row(
            "SELECT run_id, group_id, stage_ordinal FROM stage_cache WHERE cache_key = ?1",
            [key],
            |row| {
                Ok(CacheHit {
                    run_id: RunId::new(row.get(0)?),
                    group_id: GroupId::new(row.get(1)?),
                    stage_ordinal: row.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(hit)
}

/// Copies a cached stage's recorded signals and artifacts into this run,
/// sharing their blobs rather than recomputing anything.
///
/// The copy is what makes a cache hit indistinguishable from a stage that ran:
/// the results screen reads the same rows either way, and the `cached` status
/// is the only trace.
pub fn copy_cached_output(
    conn: &Connection,
    from: CacheHit,
    into_run: RunId,
    into_group: GroupId,
    into_stage: i32,
) -> Result<usize> {
    let mut copied = 0;

    for signal in stage_signals(conn, from.run_id, from.group_id, from.stage_ordinal)? {
        if let Some(blob_id) = signal.blob_id {
            blob::retain(conn, blob_id)?;
        }
        let (min, max, mean, rms) = stats_columns(&signal.stats);
        conn.execute(
            "INSERT INTO run_signal (run_id, group_id, stage_ordinal, signal_ordinal, name, domain,
                                     disposition, blob_id, sample_rate_hz, t0_s, sample_count,
                                     min_value, max_value, mean_value, rms_value, attrs_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                into_run.get(),
                into_group.get(),
                into_stage,
                signal.signal_ordinal,
                signal.name,
                signal.domain.as_str(),
                signal.disposition.as_str(),
                signal.blob_id.map(BlobId::get),
                signal.timebase.sample_rate_hz,
                signal.timebase.t0_s,
                signal.sample_count as i64,
                min,
                max,
                mean,
                rms,
                attributes_json(&signal.attributes)?,
            ],
        )?;
        copied += 1;
    }

    for artifact in stage_artifacts(conn, from.run_id, Some(from.group_id), from.stage_ordinal)? {
        if let Some(blob_id) = artifact.blob_id {
            blob::retain(conn, blob_id)?;
        }
        conn.execute(
            "INSERT INTO artifact (run_id, group_id, stage_ordinal, port, kind, kind_version,
                                   payload_json, blob_id, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                into_run.get(),
                into_group.get(),
                into_stage,
                artifact.port,
                artifact.kind,
                artifact.kind_version,
                artifact.payload_json,
                artifact.blob_id.map(BlobId::get),
                artifact.summary,
            ],
        )?;
        copied += 1;
    }

    Ok(copied)
}

fn not_found_as(error: StoreError, what: &'static str, id: i64) -> StoreError {
    match error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => StoreError::not_found(what, id),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::run::Severity;
    use sp_core::{DType, SampleBuffer, SourceKind};

    use crate::library::{self, NewDataset, NewGroup};
    use crate::trains::{self, NewTrain};
    use crate::Store;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        groups: Vec<GroupId>,
    }

    fn fixture(group_count: u32) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let groups = store
            .write(move |conn| {
                let dataset = library::insert_dataset(
                    conn,
                    &NewDataset::new("run test", SourceKind::CsvImport),
                )?;
                let train = trains::insert_train(conn, &NewTrain::new(dataset, 0))?;
                let groups = (0..group_count)
                    .map(|ordinal| library::insert_group(conn, &NewGroup::new(train, ordinal, 0)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(groups)
            })
            .unwrap();
        Fixture {
            _dir: dir,
            store,
            groups,
        }
    }

    fn buffer(values: &[f64]) -> SampleBuffer {
        SampleBuffer::from_f64(DType::F64, values)
    }

    fn saved_pipeline(store: &Store) -> PipelineId {
        store
            .write(|conn| {
                let id = insert_pipeline(conn, &NewPipeline::new("smoke"))?;
                set_pipeline_stages(
                    conn,
                    id,
                    &[
                        PipelineStageRow::new(0, "util.passthrough"),
                        PipelineStageRow::new(1, "dsp.gain").with_params_json(r#"{"gain":2.0}"#),
                    ],
                )?;
                Ok(id)
            })
            .unwrap()
    }

    #[test]
    fn a_pipeline_round_trips_with_its_stages() {
        let fx = fixture(1);
        let id = saved_pipeline(&fx.store);
        let (header, stages) = fx
            .store
            .read(|conn| Ok((get_pipeline(conn, id)?, pipeline_stages(conn, id)?)))
            .unwrap();
        assert_eq!(header.name, "smoke");
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[1].stage_kind, "dsp.gain");
        assert_eq!(stages[1].params_json, r#"{"gain":2.0}"#);
        assert_eq!(stages[0].retention, Retention::Always);
        assert!(stages[0].enabled);
    }

    #[test]
    fn setting_stages_replaces_the_previous_list() {
        let fx = fixture(1);
        let id = saved_pipeline(&fx.store);
        fx.store
            .write(move |conn| {
                set_pipeline_stages(conn, id, &[PipelineStageRow::new(0, "dsp.detrend")])
            })
            .unwrap();
        let stages = fx.store.read(|conn| pipeline_stages(conn, id)).unwrap();
        assert_eq!(stages.len(), 1, "the old stage 1 is gone, not renumbered");
        assert_eq!(stages[0].stage_kind, "dsp.detrend");
    }

    #[test]
    fn a_run_records_its_groups_and_stages() {
        let fx = fixture(2);
        let pipeline = saved_pipeline(&fx.store);
        let groups = fx.groups.clone();
        let run = fx
            .store
            .write(move |conn| {
                let run = begin_run(
                    conn,
                    &NewRun::new(pipeline, "hash-1").over_dataset(DatasetId::new(1)),
                )?;
                for &group_id in &groups {
                    record_group(
                        conn,
                        run,
                        &RunGroupRow {
                            group_id,
                            status: RunStatus::Ok,
                            wall_ms: Some(4),
                            message: None,
                        },
                    )?;
                    for ordinal in 0..2 {
                        let mut stage = RunStageRow::new(group_id, ordinal, StageStatus::Ok);
                        stage.cache_key = format!("k{ordinal}");
                        stage.metrics.insert("rms".into(), 0.5);
                        stage
                            .diagnostics
                            .push(Diagnostic::warn("close to full scale"));
                        record_stage(conn, run, &stage)?;
                    }
                }
                finish_run(conn, run, RunStatus::Ok)?;
                Ok(run)
            })
            .unwrap();

        let (header, group_rows, stage_rows) = fx
            .store
            .read(|conn| {
                Ok((
                    get_run(conn, run)?,
                    run_groups(conn, run)?,
                    run_stages(conn, run)?,
                ))
            })
            .unwrap();

        assert_eq!(header.status, RunStatus::Ok);
        assert!(header.finished_utc.is_some());
        assert_eq!(header.pipeline_hash, "hash-1");
        assert_eq!(group_rows.len(), 2);
        assert_eq!(stage_rows.len(), 4, "two stages over two groups");
        assert_eq!(stage_rows[0].metrics.get("rms"), Some(&0.5));
        assert_eq!(stage_rows[0].diagnostics[0].severity, Severity::Warn);
    }

    #[test]
    fn the_source_signals_are_recorded_as_stage_minus_one() {
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        let run = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-2"))?;
                record_signal(
                    conn,
                    run,
                    &NewRunSignal::new(
                        group,
                        SOURCE_STAGE,
                        0,
                        "raw",
                        Timebase::regular(1000.0, 0.0),
                    )
                    .with_disposition(Disposition::Passthrough)
                    .with_samples(buffer(&[1.0, 2.0, 3.0])),
                )?;
                record_signal(
                    conn,
                    run,
                    &NewRunSignal::new(group, 0, 0, "raw", Timebase::regular(1000.0, 0.0))
                        .with_disposition(Disposition::Replaced)
                        .with_samples(buffer(&[2.0, 4.0, 6.0])),
                )?;
                Ok(run)
            })
            .unwrap();

        let (before, after) = fx
            .store
            .read(|conn| {
                Ok((
                    stage_signals(conn, run, group, SOURCE_STAGE)?,
                    stage_signals(conn, run, group, 0)?,
                ))
            })
            .unwrap();

        assert_eq!(before.len(), 1);
        assert_eq!(before[0].disposition, Disposition::Passthrough);
        assert_eq!(after[0].disposition, Disposition::Replaced);
        assert_eq!(after[0].stats.max(), Some(6.0));

        let samples = fx
            .store
            .read(|conn| read_signal_samples(conn, &after[0], SampleRange::first(3)))
            .unwrap();
        assert_eq!(samples.values().collect::<Vec<_>>(), [2.0, 4.0, 6.0]);
    }

    #[test]
    fn an_unchanged_signal_shares_the_blob_it_came_in_on() {
        // Content addressing is what makes `Passthrough` free: recording the
        // same samples twice must not store them twice (§9.4).
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        fx.store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-3"))?;
                for stage in 0..3 {
                    record_signal(
                        conn,
                        run,
                        &NewRunSignal::new(group, stage, 0, "raw", Timebase::regular(1.0, 0.0))
                            .with_disposition(Disposition::Passthrough)
                            .with_samples(buffer(&[1.0, 2.0, 3.0])),
                    )?;
                }
                Ok(())
            })
            .unwrap();

        let blobs = fx.store.read(blob::list).unwrap();
        assert_eq!(blobs.len(), 1, "three rows, one blob");
        assert_eq!(blobs[0].refcount, 3);
    }

    #[test]
    fn deleting_a_run_releases_its_blobs() {
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        let run = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-4"))?;
                record_signal(
                    conn,
                    run,
                    &NewRunSignal::new(group, 0, 0, "out", Timebase::regular(1.0, 0.0))
                        .with_samples(buffer(&[9.0; 8])),
                )?;
                Ok(run)
            })
            .unwrap();
        assert_eq!(fx.store.read(blob::list).unwrap().len(), 1);

        fx.store.write(move |conn| delete_run(conn, run)).unwrap();
        assert!(fx.store.read(blob::list).unwrap().is_empty());
    }

    #[test]
    fn a_large_artifact_payload_moves_out_of_line() {
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        let big = format!("[{}]", vec!["0.125"; 40_000].join(","));
        let small = r#"{"count":4}"#.to_owned();

        let (run, big_copy) = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-5"))?;
                insert_artifact(
                    conn,
                    run,
                    &NewArtifact::new(0, "detections", "detections.v1", 1, small)
                        .for_group(group)
                        .with_summary("4 detections"),
                )?;
                insert_artifact(
                    conn,
                    run,
                    &NewArtifact::new(1, "spectrum", "spectrum.v1", 1, big.clone())
                        .for_group(group),
                )?;
                Ok((run, big))
            })
            .unwrap();

        let (inline, out_of_line) = fx
            .store
            .read(|conn| {
                Ok((
                    stage_artifacts(conn, run, Some(group), 0)?,
                    stage_artifacts(conn, run, Some(group), 1)?,
                ))
            })
            .unwrap();

        assert!(inline[0].payload_json.is_some());
        assert_eq!(inline[0].summary.as_deref(), Some("4 detections"));
        assert!(out_of_line[0].payload_json.is_none());
        assert!(out_of_line[0].blob_id.is_some());

        let payload = fx
            .store
            .read(|conn| artifact_payload(conn, &out_of_line[0]))
            .unwrap();
        assert_eq!(payload, big_copy, "a blob payload reads back verbatim");
    }

    #[test]
    fn a_run_level_artifact_is_the_one_with_no_group() {
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        let run = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-6"))?;
                insert_artifact(conn, run, &NewArtifact::new(0, "roc", "roc.v1", 1, "{}"))?;
                insert_artifact(
                    conn,
                    run,
                    &NewArtifact::new(0, "peaks", "peaks.v1", 1, "{}").for_group(group),
                )?;
                Ok(run)
            })
            .unwrap();

        let (run_level, per_group) = fx
            .store
            .read(|conn| {
                Ok((
                    stage_artifacts(conn, run, None, 0)?,
                    stage_artifacts(conn, run, Some(group), 0)?,
                ))
            })
            .unwrap();
        assert_eq!(run_level.len(), 1);
        assert_eq!(run_level[0].kind, "roc.v1");
        assert_eq!(per_group.len(), 1);
        assert_eq!(per_group[0].kind, "peaks.v1");
    }

    #[test]
    fn cached_output_is_copied_into_the_new_run_verbatim() {
        let fx = fixture(2);
        let pipeline = saved_pipeline(&fx.store);
        let (first_group, second_group) = (fx.groups[0], fx.groups[1]);

        let first = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-7"))?;
                record_signal(
                    conn,
                    run,
                    &NewRunSignal::new(first_group, 0, 0, "filtered", Timebase::regular(10.0, 0.0))
                        .with_disposition(Disposition::Replaced)
                        .with_samples(buffer(&[0.5, 1.5])),
                )?;
                insert_artifact(
                    conn,
                    run,
                    &NewArtifact::new(0, "stats", "stats.v1", 1, r#"{"rms":1.1}"#)
                        .for_group(first_group),
                )?;
                cache_put(
                    conn,
                    "key-a",
                    CacheHit {
                        run_id: run,
                        group_id: first_group,
                        stage_ordinal: 0,
                    },
                )?;
                finish_run(conn, run, RunStatus::Ok)?;
                Ok(run)
            })
            .unwrap();

        let second = fx
            .store
            .write(move |conn| {
                let hit = cache_lookup(conn, "key-a")?.expect("the key was published");
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-7"))?;
                let copied = copy_cached_output(conn, hit, run, second_group, 0)?;
                assert_eq!(copied, 2, "one signal and one artifact");
                Ok(run)
            })
            .unwrap();

        let (signals, artifacts) = fx
            .store
            .read(|conn| {
                Ok((
                    stage_signals(conn, second, second_group, 0)?,
                    stage_artifacts(conn, second, Some(second_group), 0)?,
                ))
            })
            .unwrap();
        assert_eq!(signals[0].name, "filtered");
        assert_eq!(signals[0].sample_count, 2);
        assert_eq!(artifacts[0].kind, "stats.v1");

        // The copy shares the original's blob rather than duplicating samples.
        let source = fx
            .store
            .read(|conn| stage_signals(conn, first, first_group, 0))
            .unwrap();
        assert_eq!(signals[0].blob_id, source[0].blob_id);
        assert_eq!(fx.store.read(blob::list).unwrap().len(), 1);
    }

    #[test]
    fn a_cache_key_of_a_deleted_run_stops_resolving() {
        // A stale key would point at rows the cascade took with the run.
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        let run = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-8"))?;
                cache_put(
                    conn,
                    "key-b",
                    CacheHit {
                        run_id: run,
                        group_id: group,
                        stage_ordinal: 0,
                    },
                )?;
                Ok(run)
            })
            .unwrap();
        assert!(fx
            .store
            .read(|conn| cache_lookup(conn, "key-b"))
            .unwrap()
            .is_some());

        fx.store.write(move |conn| delete_run(conn, run)).unwrap();
        assert!(fx
            .store
            .read(|conn| cache_lookup(conn, "key-b"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_a_pipeline_takes_its_runs_with_it() {
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        fx.store
            .write(move |conn| {
                begin_run(conn, &NewRun::new(pipeline, "hash-9"))?;
                delete_pipeline(conn, pipeline)
            })
            .unwrap();
        assert!(fx
            .store
            .read(|conn| list_runs(conn, None))
            .unwrap()
            .is_empty());
        assert!(fx.store.read(list_pipelines).unwrap().is_empty());
    }

    #[test]
    fn a_group_row_survives_the_group_being_deleted_only_as_a_cascade() {
        // Runs reference live groups; deleting the group takes its run rows,
        // which is what keeps `PRAGMA foreign_key_check` clean.
        let fx = fixture(1);
        let pipeline = saved_pipeline(&fx.store);
        let group = fx.groups[0];
        let train = fx
            .store
            .read(move |conn| library::get_group(conn, group))
            .unwrap()
            .train_id;
        let run = fx
            .store
            .write(move |conn| {
                let run = begin_run(conn, &NewRun::new(pipeline, "hash-10"))?;
                record_group(
                    conn,
                    run,
                    &RunGroupRow {
                        group_id: group,
                        status: RunStatus::Ok,
                        wall_ms: None,
                        message: None,
                    },
                )?;
                Ok(run)
            })
            .unwrap();

        fx.store
            .write(move |conn| trains::delete_train(conn, train))
            .unwrap();
        assert!(fx
            .store
            .read(move |conn| run_groups(conn, run))
            .unwrap()
            .is_empty());
    }
}
