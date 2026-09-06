//! Pulse groups: one column per field plus a shared time-of-arrival column,
//! and cross-group search over them (`docs/DESIGN.md` §6.6).

use rusqlite::{params, Connection, Row};
use sp_core::{
    Attributes, DType, FieldRange, GroupId, PulseField, PulseRef, SampleBuffer, SampleRange,
    Samples, TimeUnit, Timebase, TrainId,
};

use crate::blob::{self, BlobId, DEFAULT_CHUNK_SIZE};
use crate::error::{Result, StoreError};
use crate::library::{attributes_json, stats_columns, stats_from_columns};

/// One field column to store with a pulse group.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPulseField {
    /// Header text as written, e.g. `pulse width`.
    pub name: String,
    pub unit: Option<String>,
    /// Storage type. `F32` is the default: four bytes per value keeps a
    /// cross-group scan at memory bandwidth.
    pub dtype: DType,
    /// One value per pulse, in row order. `NaN` marks a missing cell.
    pub values: Vec<f64>,
}

impl NewPulseField {
    #[must_use]
    pub fn new(name: impl Into<String>, values: Vec<f64>) -> Self {
        Self {
            name: name.into(),
            unit: None,
            dtype: DType::F32,
            values,
        }
    }

    #[must_use]
    pub fn with_dtype(mut self, dtype: DType) -> Self {
        self.dtype = dtype;
        self
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }
}

/// A group of pulse records to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPulseGroup {
    /// The train this group is a segment of (§6.6).
    pub train_id: TrainId,
    pub ordinal: u32,
    pub name: Option<String>,
    /// The `count` field from the group row; the actual count is the TOA
    /// column's length.
    pub declared_count: u32,
    /// Unit the source file expressed TOA in; the stored column is seconds.
    pub toa_unit: TimeUnit,
    /// Times of arrival in seconds, one per pulse.
    pub toa_seconds: Vec<f64>,
    pub fields: Vec<NewPulseField>,
    pub attributes: Attributes,
}

impl NewPulseGroup {
    #[must_use]
    pub fn new(train_id: TrainId, ordinal: u32, toa_seconds: Vec<f64>) -> Self {
        Self {
            train_id,
            ordinal,
            name: None,
            declared_count: toa_seconds.len() as u32,
            toa_unit: TimeUnit::default(),
            toa_seconds,
            fields: Vec::new(),
            attributes: Attributes::new(),
        }
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_field(mut self, field: NewPulseField) -> Self {
        self.fields.push(field);
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }
}

/// A numeric predicate over one pulse field, matched by key.
#[derive(Debug, Clone, PartialEq)]
pub struct PulsePredicate {
    pub key: String,
    pub range: FieldRange,
}

impl PulsePredicate {
    #[must_use]
    pub fn new(key: impl Into<String>, range: FieldRange) -> Self {
        Self {
            key: key.into(),
            range,
        }
    }
}

/// Writes the TOA column and every field column, inserts the group and its
/// `pulse_field` rows with their zone maps.
pub fn insert_pulse_group(conn: &Connection, group: &NewPulseGroup) -> Result<GroupId> {
    insert_pulse_group_chunked(conn, group, DEFAULT_CHUNK_SIZE)
}

/// [`insert_pulse_group`] with an explicit chunk size.
pub fn insert_pulse_group_chunked(
    conn: &Connection,
    group: &NewPulseGroup,
    chunk_size: usize,
) -> Result<GroupId> {
    let count = group.toa_seconds.len();
    for field in &group.fields {
        if field.values.len() != count {
            return Err(StoreError::invalid(format!(
                "pulse field '{}' has {} values but the group has {count} pulses",
                field.name,
                field.values.len()
            )));
        }
    }

    let t0_s = group.toa_seconds.first().copied().unwrap_or(0.0);
    let timebase = Timebase::irregular(t0_s);
    let toa = SampleBuffer::new(Samples::F64(group.toa_seconds.clone()));
    let toa_blob = blob::write_column(conn, &toa, timebase, chunk_size)?;

    conn.execute(
        "INSERT INTO signal_group
             (train_id, ordinal, name, declared_count, actual_count, toa_blob_id, toa_unit, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            group.train_id.get(),
            group.ordinal,
            group.name,
            group.declared_count,
            count as i64,
            toa_blob.get(),
            group.toa_unit.as_str(),
            attributes_json(&group.attributes)?,
        ],
    )?;
    let group_id = GroupId::new(conn.last_insert_rowid());

    let mut stmt = conn.prepare_cached(
        "INSERT INTO pulse_field
             (group_id, ordinal, name, key, unit, dtype, blob_id,
              min_value, max_value, mean_value, rms_value, nan_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for (ordinal, field) in group.fields.iter().enumerate() {
        let buffer = SampleBuffer::from_f64(field.dtype, &field.values);
        let stats = sp_core::stats::summarise(&buffer);
        let blob_id = blob::write_column(conn, &buffer, timebase, chunk_size)?;
        let (min, max, mean, rms) = stats_columns(&stats);
        let key = sp_core::pulse::normalise_key(&field.name);
        stmt.execute(params![
            group_id.get(),
            ordinal as i64,
            field.name,
            key,
            field.unit,
            field.dtype.as_str(),
            blob_id.get(),
            min,
            max,
            mean,
            rms,
            stats.non_finite() as i64,
        ])?;
    }
    Ok(group_id)
}

const FIELD_COLUMNS: &str = "group_id, ordinal, name, key, unit, dtype, blob_id,
     min_value, max_value, mean_value, rms_value, nan_count";

type RawStats = (Option<f64>, Option<f64>, Option<f64>, Option<f64>, u64);

fn field_from_row(row: &Row<'_>) -> Result<(PulseField, RawStats)> {
    let nan_count: i64 = row.get(11)?;
    let mut field = PulseField::new(
        GroupId::new(row.get(0)?),
        row.get(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(5)?.parse::<DType>()?,
    );
    field.key = row.get(3)?;
    field.unit = row.get(4)?;
    // The finite count is not on the row: it is the group's actual_count
    // less the NaN count, which the caller fills in once it knows the group.
    let raw = (
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        nan_count.max(0) as u64,
    );
    Ok((field, raw))
}

/// A group's pulse fields in column order, with statistics.
pub fn list_fields(conn: &Connection, group_id: GroupId) -> Result<Vec<PulseField>> {
    let actual_count: i64 = conn.query_row(
        "SELECT actual_count FROM signal_group WHERE id = ?1",
        [group_id.get()],
        |row| row.get(0),
    )?;
    let actual_count = actual_count.max(0) as u64;

    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FIELD_COLUMNS} FROM pulse_field WHERE group_id = ?1 ORDER BY ordinal"
    ))?;
    let rows = stmt.query_and_then([group_id.get()], field_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        let (mut field, (min, max, mean, rms, nan_count)) = row?;
        field.stats = Some(stats_from_columns(
            min,
            max,
            mean,
            rms,
            actual_count,
            nan_count,
        ));
        out.push(field);
    }
    Ok(out)
}

/// The blob holding one field's column.
pub fn field_blob(conn: &Connection, group_id: GroupId, ordinal: u32) -> Result<BlobId> {
    let raw: Option<i64> = conn.query_row(
        "SELECT blob_id FROM pulse_field WHERE group_id = ?1 AND ordinal = ?2",
        params![group_id.get(), ordinal],
        |row| row.get(0),
    )?;
    raw.map(BlobId::new).ok_or_else(|| {
        StoreError::corrupt(format!(
            "pulse field {ordinal} of group {} has no column blob",
            group_id.get()
        ))
    })
}

/// Reads a span of one field column. The buffer carries no scaling for
/// float dtypes; values are in the field's own unit.
pub fn read_field(
    conn: &Connection,
    group_id: GroupId,
    ordinal: u32,
    range: SampleRange,
) -> Result<SampleBuffer> {
    let blob_id = field_blob(conn, group_id, ordinal)?;
    blob::read_column(conn, blob_id, range)
}

/// Reads a span of a group's TOA column, in seconds.
pub fn read_toa(conn: &Connection, group_id: GroupId, range: SampleRange) -> Result<Vec<f64>> {
    let blob_id = crate::library::group_toa_blob(conn, group_id)?.ok_or_else(|| {
        StoreError::invalid(format!("group {} is not a pulse group", group_id.get()))
    })?;
    Ok(blob::read_column(conn, blob_id, range)?.to_f64())
}

/// One pulse as a record: its TOA plus every field's value at that index.
pub fn read_record(conn: &Connection, pulse: PulseRef) -> Result<(f64, Vec<(PulseField, f64)>)> {
    let range = SampleRange::new(u64::from(pulse.index), u64::from(pulse.index) + 1);
    let toa = read_toa(conn, pulse.group, range)?
        .first()
        .copied()
        .ok_or_else(|| StoreError::not_found("pulse", i64::from(pulse.index)))?;
    let mut values = Vec::new();
    for field in list_fields(conn, pulse.group)? {
        let value = read_field(conn, pulse.group, field.ordinal, range)?
            .value(0)
            .unwrap_or(f64::NAN);
        values.push((field, value));
    }
    Ok((toa, values))
}

/// Values scanned per column read during a search; bounds memory to a few
/// MB per predicate regardless of group size.
const SCAN_STRIDE: u64 = 1 << 20;

/// Cross-group pulse search (§6.6, goal G10).
///
/// Every predicate must hold. Groups are first eliminated in SQL by zone map
/// — a group whose `[min, max]` for a key cannot overlap the predicate is
/// never read — and the survivors' columns are then scanned in strides.
pub fn search(conn: &Connection, predicates: &[PulsePredicate]) -> Result<Vec<PulseRef>> {
    if predicates.is_empty() {
        return Err(StoreError::invalid(
            "a pulse search needs at least one predicate",
        ));
    }

    // Candidate (group, column ordinal) per predicate, from the zone map.
    let mut stmt = conn.prepare_cached(
        "SELECT group_id, ordinal FROM pulse_field
         WHERE key = ?1 AND min_value IS NOT NULL
           AND (?2 IS NULL OR max_value >= ?2)
           AND (?3 IS NULL OR min_value <= ?3)
         ORDER BY group_id",
    )?;
    let mut candidates: Option<Vec<(i64, Vec<u32>)>> = None;
    for predicate in predicates {
        let rows = stmt.query_map(
            params![predicate.key, predicate.range.min, predicate.range.max],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u32>(1)?)),
        )?;
        let hits: Vec<(i64, u32)> = rows.collect::<rusqlite::Result<_>>()?;
        candidates = Some(match candidates.take() {
            None => hits.into_iter().map(|(g, o)| (g, vec![o])).collect(),
            Some(previous) => previous
                .into_iter()
                .filter_map(|(g, mut ordinals)| {
                    let (_, o) = hits.iter().find(|(hg, _)| *hg == g)?;
                    ordinals.push(*o);
                    Some((g, ordinals))
                })
                .collect(),
        });
    }

    let mut out = Vec::new();
    for (group_raw, ordinals) in candidates.unwrap_or_default() {
        let group = GroupId::new(group_raw);
        let count: i64 = conn.query_row(
            "SELECT actual_count FROM signal_group WHERE id = ?1",
            [group_raw],
            |row| row.get(0),
        )?;
        let count = count.max(0) as u64;
        let mut start = 0u64;
        while start < count {
            let range = SampleRange::new(start, (start + SCAN_STRIDE).min(count));
            let columns: Vec<SampleBuffer> = ordinals
                .iter()
                .map(|&ordinal| read_field(conn, group, ordinal, range))
                .collect::<Result<_>>()?;
            for i in 0..range.len() {
                let all = predicates.iter().zip(&columns).all(|(predicate, column)| {
                    column.value(i).is_some_and(|v| predicate.range.contains(v))
                });
                if all {
                    out.push(PulseRef::new(group, (range.start + i) as u32));
                }
            }
            start = range.end;
        }
    }
    Ok(out)
}
