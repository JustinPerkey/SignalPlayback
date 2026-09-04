//! Datasets, groups, signals and tags: the metadata half of the library
//! (`docs/DESIGN.md` §5.2).
//!
//! Every function takes a connection and does one thing; callers compose
//! them inside a transaction on the writer.

use rusqlite::{params, Connection, OptionalExtension, Row};
use sp_core::group::DatasetId;
use sp_core::time::{now_utc, Timestamp};
use sp_core::{
    Attributes, DType, Dataset, Domain, GroupId, Provenance, RunId, SampleBuffer, SampleRange,
    Signal, SignalGroup, SignalId, SignalStats, SourceKind, TimeUnit, Timebase,
};
use time::format_description::well_known::Rfc3339;

use crate::blob::{self, BlobId, DEFAULT_CHUNK_SIZE};
use crate::error::{Result, StoreError};
use crate::props;

/// What to insert as a dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct NewDataset {
    pub name: String,
    pub source_kind: SourceKind,
    pub source_uri: Option<String>,
    pub notes: Option<String>,
    pub attributes: Attributes,
}

impl NewDataset {
    #[must_use]
    pub fn new(name: impl Into<String>, source_kind: SourceKind) -> Self {
        Self {
            name: name.into(),
            source_kind,
            source_uri: None,
            notes: None,
            attributes: Attributes::new(),
        }
    }

    #[must_use]
    pub fn with_source_uri(mut self, uri: impl Into<String>) -> Self {
        self.source_uri = Some(uri.into());
        self
    }
}

/// What to insert as a group of sampled signals. Pulse groups go through
/// [`crate::pulses::insert_pulse_group`], which also writes the columns.
#[derive(Debug, Clone, PartialEq)]
pub struct NewGroup {
    pub dataset_id: DatasetId,
    pub ordinal: u32,
    pub name: Option<String>,
    pub declared_count: u32,
    pub actual_count: u32,
    pub attributes: Attributes,
}

impl NewGroup {
    #[must_use]
    pub fn new(dataset_id: DatasetId, ordinal: u32, count: u32) -> Self {
        Self {
            dataset_id,
            ordinal,
            name: None,
            declared_count: count,
            actual_count: count,
            attributes: Attributes::new(),
        }
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// What to insert as a signal, samples included.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSignal {
    pub group_id: GroupId,
    pub ordinal: u32,
    pub name: String,
    pub units: Option<String>,
    pub domain: Domain,
    pub provenance: Provenance,
    pub timebase: Timebase,
    pub samples: SampleBuffer,
    pub attributes: Attributes,
    /// JSON `GenSpec` for a generated signal.
    pub gen_spec: Option<String>,
}

impl NewSignal {
    #[must_use]
    pub fn new(
        group_id: GroupId,
        ordinal: u32,
        name: impl Into<String>,
        timebase: Timebase,
        samples: SampleBuffer,
    ) -> Self {
        Self {
            group_id,
            ordinal,
            name: name.into(),
            units: None,
            domain: Domain::Analog,
            provenance: Provenance::Imported,
            timebase,
            samples,
            attributes: Attributes::new(),
            gen_spec: None,
        }
    }

    #[must_use]
    pub fn with_domain(mut self, domain: Domain) -> Self {
        self.domain = domain;
        self
    }

    #[must_use]
    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = provenance;
        self
    }

    #[must_use]
    pub fn with_units(mut self, units: impl Into<String>) -> Self {
        self.units = Some(units.into());
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }
}

/// Counts for the status bar and the library statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LibrarySummary {
    pub datasets: u64,
    pub groups: u64,
    pub signals: u64,
    pub pulse_fields: u64,
    pub blobs: u64,
    /// Payload bytes across every blob, before SQLite's own overhead.
    pub blob_bytes: u64,
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

pub(crate) fn format_timestamp(ts: Timestamp) -> Result<String> {
    ts.format(&Rfc3339)
        .map_err(|e| StoreError::invalid(format!("timestamp: {e}")))
}

pub(crate) fn parse_timestamp(text: &str) -> Result<Timestamp> {
    Timestamp::parse(text, &Rfc3339)
        .map_err(|e| StoreError::corrupt(format!("timestamp '{text}': {e}")))
}

pub(crate) fn attributes_json(attributes: &Attributes) -> Result<String> {
    Ok(serde_json::to_string(attributes)?)
}

pub(crate) fn parse_attributes(text: &str) -> Result<Attributes> {
    Ok(serde_json::from_str(text)?)
}

// ---------------------------------------------------------------------------
// Datasets
// ---------------------------------------------------------------------------

pub fn insert_dataset(conn: &Connection, dataset: &NewDataset) -> Result<DatasetId> {
    conn.execute(
        "INSERT INTO dataset (name, source_kind, source_uri, created_utc, notes, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            dataset.name,
            dataset.source_kind.as_str(),
            dataset.source_uri,
            format_timestamp(now_utc())?,
            dataset.notes,
            attributes_json(&dataset.attributes)?,
        ],
    )?;
    Ok(DatasetId::new(conn.last_insert_rowid()))
}

const DATASET_COLUMNS: &str = "id, name, source_kind, source_uri, created_utc, notes, attributes";

fn dataset_from_row(row: &Row<'_>) -> Result<Dataset> {
    Ok(Dataset {
        id: DatasetId::new(row.get(0)?),
        name: row.get(1)?,
        source_kind: row.get::<_, String>(2)?.parse()?,
        source_uri: row.get(3)?,
        created_utc: parse_timestamp(&row.get::<_, String>(4)?)?,
        notes: row.get(5)?,
        attributes: parse_attributes(&row.get::<_, String>(6)?)?,
    })
}

pub fn get_dataset(conn: &Connection, id: DatasetId) -> Result<Dataset> {
    conn.query_row_and_then(
        &format!("SELECT {DATASET_COLUMNS} FROM dataset WHERE id = ?1"),
        [id.get()],
        dataset_from_row,
    )
    .map_err(|e| match e {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
            StoreError::not_found("dataset", id.get())
        }
        other => other,
    })
}

/// Every dataset, newest first.
pub fn list_datasets(conn: &Connection) -> Result<Vec<Dataset>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {DATASET_COLUMNS} FROM dataset ORDER BY id DESC"
    ))?;
    let rows = stmt.query_and_then([], dataset_from_row)?;
    rows.collect()
}

/// Deletes a dataset with everything under it, releasing the blobs its
/// signals and pulse fields referenced.
pub fn delete_dataset(conn: &Connection, id: DatasetId) -> Result<()> {
    let blobs = referenced_blobs(
        conn,
        "SELECT s.blob_id FROM signal s JOIN signal_group g ON g.id = s.group_id WHERE g.dataset_id = ?1
         UNION ALL
         SELECT s.time_blob_id FROM signal s JOIN signal_group g ON g.id = s.group_id WHERE g.dataset_id = ?1
         UNION ALL
         SELECT g.toa_blob_id FROM signal_group g WHERE g.dataset_id = ?1
         UNION ALL
         SELECT f.blob_id FROM pulse_field f JOIN signal_group g ON g.id = f.group_id WHERE g.dataset_id = ?1",
        id.get(),
    )?;
    let deleted = conn.execute("DELETE FROM dataset WHERE id = ?1", [id.get()])?;
    if deleted == 0 {
        return Err(StoreError::not_found("dataset", id.get()));
    }
    for blob_id in blobs {
        blob::release(conn, blob_id)?;
    }
    Ok(())
}

fn referenced_blobs(conn: &Connection, sql: &str, id: i64) -> Result<Vec<BlobId>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([id], |row| row.get::<_, Option<i64>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        if let Some(raw) = row? {
            out.push(BlobId::new(raw));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

pub fn insert_group(conn: &Connection, group: &NewGroup) -> Result<GroupId> {
    conn.execute(
        "INSERT INTO signal_group
             (dataset_id, ordinal, name, declared_count, actual_count, toa_blob_id, toa_unit, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6)",
        params![
            group.dataset_id.get(),
            group.ordinal,
            group.name,
            group.declared_count,
            group.actual_count,
            attributes_json(&group.attributes)?,
        ],
    )?;
    Ok(GroupId::new(conn.last_insert_rowid()))
}

const GROUP_COLUMNS: &str =
    "id, dataset_id, ordinal, name, declared_count, actual_count, toa_unit, attributes";

fn group_from_row(row: &Row<'_>) -> Result<SignalGroup> {
    let toa_unit: Option<String> = row.get(6)?;
    Ok(SignalGroup {
        id: GroupId::new(row.get(0)?),
        dataset_id: DatasetId::new(row.get(1)?),
        ordinal: row.get(2)?,
        name: row.get(3)?,
        declared_count: row.get(4)?,
        actual_count: row.get(5)?,
        toa_unit: toa_unit.map(|s| s.parse::<TimeUnit>()).transpose()?,
        attributes: parse_attributes(&row.get::<_, String>(7)?)?,
    })
}

pub fn get_group(conn: &Connection, id: GroupId) -> Result<SignalGroup> {
    conn.query_row_and_then(
        &format!("SELECT {GROUP_COLUMNS} FROM signal_group WHERE id = ?1"),
        [id.get()],
        group_from_row,
    )
    .map_err(|e| match e {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
            StoreError::not_found("group", id.get())
        }
        other => other,
    })
}

/// A dataset's groups in block order.
pub fn list_groups(conn: &Connection, dataset_id: DatasetId) -> Result<Vec<SignalGroup>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {GROUP_COLUMNS} FROM signal_group WHERE dataset_id = ?1 ORDER BY ordinal"
    ))?;
    let rows = stmt.query_and_then([dataset_id.get()], group_from_row)?;
    rows.collect()
}

/// The TOA column of a pulse group, if it has one.
pub(crate) fn group_toa_blob(conn: &Connection, id: GroupId) -> Result<Option<BlobId>> {
    let raw: Option<i64> = conn
        .query_row(
            "SELECT toa_blob_id FROM signal_group WHERE id = ?1",
            [id.get()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::not_found("group", id.get()))?;
    Ok(raw.map(BlobId::new))
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// Writes the samples as a column blob, summarises them, inserts the row and
/// mirrors its attributes into the property index.
pub fn insert_signal(conn: &Connection, signal: &NewSignal) -> Result<SignalId> {
    insert_signal_chunked(conn, signal, DEFAULT_CHUNK_SIZE)
}

/// [`insert_signal`] with an explicit chunk size, for tests that want to
/// exercise chunk boundaries without writing megabytes.
pub fn insert_signal_chunked(
    conn: &Connection,
    signal: &NewSignal,
    chunk_size: usize,
) -> Result<SignalId> {
    let stats = sp_core::stats::summarise(&signal.samples);
    let blob_id = blob::write_column(conn, &signal.samples, signal.timebase, chunk_size)?;
    let (min, max, mean, rms) = stats_columns(&stats);

    conn.execute(
        "INSERT INTO signal
             (group_id, ordinal, name, units, dtype, domain, provenance,
              sample_rate_hz, t0_s, sample_count, blob_id, time_blob_id, gen_spec,
              min_value, max_value, mean_value, rms_value, nan_count, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, ?12,
                 ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            signal.group_id.get(),
            signal.ordinal,
            signal.name,
            signal.units,
            signal.samples.dtype().as_str(),
            signal.domain.as_str(),
            signal.provenance.as_str(),
            signal.timebase.sample_rate_hz,
            signal.timebase.t0_s,
            signal.samples.len() as i64,
            blob_id.get(),
            signal.gen_spec,
            min,
            max,
            mean,
            rms,
            stats.non_finite() as i64,
            attributes_json(&signal.attributes)?,
        ],
    )?;
    let id = SignalId::new(conn.last_insert_rowid());
    props::mirror_signal_attributes(conn, id, &signal.attributes)?;
    Ok(id)
}

pub(crate) fn stats_columns(
    stats: &SignalStats,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    (stats.min(), stats.max(), stats.mean(), stats.rms())
}

pub(crate) fn stats_from_columns(
    min: Option<f64>,
    max: Option<f64>,
    mean: Option<f64>,
    rms: Option<f64>,
    count: u64,
    nan_count: u64,
) -> SignalStats {
    match (min, max, mean, rms) {
        (Some(min), Some(max), Some(mean), Some(rms)) => SignalStats::from_stored(
            min,
            max,
            mean,
            rms,
            count.saturating_sub(nan_count),
            nan_count,
        ),
        _ => SignalStats::from_stored(0.0, 0.0, 0.0, 0.0, 0, nan_count),
    }
}

const SIGNAL_COLUMNS: &str = "id, group_id, ordinal, name, units, dtype, domain, provenance,
     sample_rate_hz, t0_s, sample_count, min_value, max_value, mean_value, rms_value,
     nan_count, attributes";

fn signal_from_row(row: &Row<'_>) -> Result<Signal> {
    let dtype: DType = row.get::<_, String>(5)?.parse()?;
    let domain: Domain = row.get::<_, String>(6)?.parse()?;
    let provenance = match row.get::<_, String>(7)?.as_str() {
        "imported" => Provenance::Imported,
        "generated" => Provenance::Generated,
        // The run/stage detail arrives with the run tables (M5); until then a
        // derived signal is recorded as derived from nothing in particular.
        "derived" => Provenance::Derived {
            run_id: RunId::new(0),
            stage_ordinal: 0,
        },
        other => {
            return Err(StoreError::corrupt(format!("unknown provenance '{other}'")));
        }
    };
    let sample_rate_hz: Option<f64> = row.get(8)?;
    let t0_s: f64 = row.get(9)?;
    let timebase = match sample_rate_hz {
        Some(rate) if rate > 0.0 => Timebase::regular(rate, t0_s),
        _ => Timebase::irregular(t0_s),
    };
    let sample_count: i64 = row.get(10)?;
    let sample_count =
        u64::try_from(sample_count).map_err(|_| StoreError::corrupt("negative sample_count"))?;
    let nan_count: i64 = row.get(15)?;
    let stats = stats_from_columns(
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        sample_count,
        nan_count.max(0) as u64,
    );
    Ok(Signal {
        id: SignalId::new(row.get(0)?),
        group_id: GroupId::new(row.get(1)?),
        ordinal: row.get(2)?,
        name: row.get(3)?,
        units: row.get(4)?,
        dtype,
        domain,
        provenance,
        timebase,
        sample_count,
        stats: Some(stats),
        attributes: parse_attributes(&row.get::<_, String>(16)?)?,
    })
}

pub fn get_signal(conn: &Connection, id: SignalId) -> Result<Signal> {
    conn.query_row_and_then(
        &format!("SELECT {SIGNAL_COLUMNS} FROM signal WHERE id = ?1"),
        [id.get()],
        signal_from_row,
    )
    .map_err(|e| match e {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
            StoreError::not_found("signal", id.get())
        }
        other => other,
    })
}

/// A group's signals in ordinal order.
pub fn list_signals(conn: &Connection, group_id: GroupId) -> Result<Vec<Signal>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SIGNAL_COLUMNS} FROM signal WHERE group_id = ?1 ORDER BY ordinal"
    ))?;
    let rows = stmt.query_and_then([group_id.get()], signal_from_row)?;
    rows.collect()
}

/// Every signal in the library, by group then ordinal. Metadata only.
pub fn list_all_signals(conn: &Connection) -> Result<Vec<Signal>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SIGNAL_COLUMNS} FROM signal ORDER BY group_id, ordinal"
    ))?;
    let rows = stmt.query_and_then([], signal_from_row)?;
    rows.collect()
}

/// The blob holding a signal's samples.
pub fn signal_blob(conn: &Connection, id: SignalId) -> Result<BlobId> {
    let raw: Option<i64> = conn
        .query_row(
            "SELECT blob_id FROM signal WHERE id = ?1",
            [id.get()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::not_found("signal", id.get()))?;
    raw.map(BlobId::new)
        .ok_or_else(|| StoreError::corrupt(format!("signal {} has no sample blob", id.get())))
}

/// Reads a span of a signal's samples; the range is clamped to the signal.
pub fn read_samples(conn: &Connection, id: SignalId, range: SampleRange) -> Result<SampleBuffer> {
    let blob_id = signal_blob(conn, id)?;
    blob::read_column(conn, blob_id, range)
}

/// Replaces a signal's attributes and refreshes the property index.
pub fn update_signal_attributes(
    conn: &Connection,
    id: SignalId,
    attributes: &Attributes,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE signal SET attributes = ?1 WHERE id = ?2",
        params![attributes_json(attributes)?, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("signal", id.get()));
    }
    props::mirror_signal_attributes(conn, id, attributes)
}

/// Renames a signal.
pub fn rename_signal(conn: &Connection, id: SignalId, name: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE signal SET name = ?1 WHERE id = ?2",
        params![name, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("signal", id.get()));
    }
    Ok(())
}

/// Deletes a signal and releases its blobs.
pub fn delete_signal(conn: &Connection, id: SignalId) -> Result<()> {
    let blobs = referenced_blobs(
        conn,
        "SELECT blob_id FROM signal WHERE id = ?1
         UNION ALL SELECT time_blob_id FROM signal WHERE id = ?1",
        id.get(),
    )?;
    let deleted = conn.execute("DELETE FROM signal WHERE id = ?1", [id.get()])?;
    if deleted == 0 {
        return Err(StoreError::not_found("signal", id.get()));
    }
    for blob_id in blobs {
        blob::release(conn, blob_id)?;
    }
    Ok(())
}

/// Full-text search over signal names, units and attribute text. Accepts
/// FTS5 query syntax; a plain word matches as a prefix.
pub fn search_signals(conn: &Connection, query: &str) -> Result<Vec<SignalId>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    // Quote each term so punctuation in a name cannot break the FTS grammar.
    let fts_query = query
        .split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let mut stmt = conn
        .prepare_cached("SELECT rowid FROM signal_fts WHERE signal_fts MATCH ?1 ORDER BY rank")?;
    let rows = stmt.query_map([fts_query], |row| row.get::<_, i64>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(SignalId::new)
        .collect())
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

fn tag_id(conn: &Connection, name: &str, create: bool) -> Result<Option<i64>> {
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM tag WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .optional()?;
    if existing.is_some() || !create {
        return Ok(existing);
    }
    conn.execute("INSERT INTO tag (name) VALUES (?1)", [name])?;
    Ok(Some(conn.last_insert_rowid()))
}

/// Tags a signal, creating the tag if it is new. Idempotent.
pub fn tag_signal(conn: &Connection, id: SignalId, tag: &str) -> Result<()> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Err(StoreError::invalid("tag name is empty"));
    }
    let tag_id = tag_id(conn, tag, true)?.expect("created");
    conn.execute(
        "INSERT OR IGNORE INTO signal_tag (signal_id, tag_id) VALUES (?1, ?2)",
        params![id.get(), tag_id],
    )?;
    Ok(())
}

pub fn untag_signal(conn: &Connection, id: SignalId, tag: &str) -> Result<()> {
    if let Some(tag_id) = tag_id(conn, tag.trim(), false)? {
        conn.execute(
            "DELETE FROM signal_tag WHERE signal_id = ?1 AND tag_id = ?2",
            params![id.get(), tag_id],
        )?;
    }
    Ok(())
}

/// A signal's tags, alphabetically.
pub fn signal_tags(conn: &Connection, id: SignalId) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT t.name FROM tag t JOIN signal_tag st ON st.tag_id = t.id
         WHERE st.signal_id = ?1 ORDER BY t.name",
    )?;
    let rows = stmt.query_map([id.get()], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every tag in use, alphabetically.
pub fn list_tags(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached("SELECT name FROM tag ORDER BY name")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Signals carrying `tag`.
pub fn signals_with_tag(conn: &Connection, tag: &str) -> Result<Vec<SignalId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT st.signal_id FROM signal_tag st JOIN tag t ON t.id = st.tag_id
         WHERE t.name = ?1 ORDER BY st.signal_id",
    )?;
    let rows = stmt.query_map([tag.trim()], |row| row.get::<_, i64>(0))?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(SignalId::new)
        .collect())
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

pub fn summary(conn: &Connection) -> Result<LibrarySummary> {
    let count = |sql: &str| -> Result<u64> {
        let n: i64 = conn.query_row(sql, [], |row| row.get(0))?;
        Ok(n.max(0) as u64)
    };
    Ok(LibrarySummary {
        datasets: count("SELECT COUNT(*) FROM dataset")?,
        groups: count("SELECT COUNT(*) FROM signal_group")?,
        signals: count("SELECT COUNT(*) FROM signal")?,
        pulse_fields: count("SELECT COUNT(*) FROM pulse_field")?,
        blobs: count("SELECT COUNT(*) FROM sample_blob WHERE checksum NOT LIKE 'pending:%'")?,
        blob_bytes: count("SELECT COALESCE(SUM(byte_len), 0) FROM sample_blob")?,
    })
}
