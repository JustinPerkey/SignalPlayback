//! Signal trains: the capture a set of groups belongs to
//! (`docs/DESIGN.md` §6.6).
//!
//! A train sits between a dataset and its groups because groups are segments
//! of one capture rather than independent ones. One imported file is one
//! train; a generation batch is one train too, so both paths produce the same
//! shape in the library.

use rusqlite::{params, Connection, OptionalExtension, Row};
use sp_core::{Attributes, DatasetId, SignalTrain, TimeUnit, TrainId};

use crate::error::{Result, StoreError};
use crate::library::{attributes_json, parse_attributes};

/// What to insert as a train.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTrain {
    pub dataset_id: DatasetId,
    /// Position within the dataset; zero for a single-file import.
    pub ordinal: u32,
    pub name: Option<String>,
    /// Set for a train of pulse records; `None` for sampled signals.
    pub toa_unit: Option<TimeUnit>,
    pub attributes: Attributes,
}

impl NewTrain {
    #[must_use]
    pub fn new(dataset_id: DatasetId, ordinal: u32) -> Self {
        Self {
            dataset_id,
            ordinal,
            name: None,
            toa_unit: None,
            attributes: Attributes::new(),
        }
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_toa_unit(mut self, unit: TimeUnit) -> Self {
        self.toa_unit = Some(unit);
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }
}

const TRAIN_COLUMNS: &str = "id, dataset_id, ordinal, name, toa_unit, attributes";

fn train_from_row(row: &Row<'_>) -> Result<SignalTrain> {
    let toa_unit: Option<String> = row.get(4)?;
    Ok(SignalTrain {
        id: TrainId::new(row.get(0)?),
        dataset_id: DatasetId::new(row.get(1)?),
        ordinal: row.get(2)?,
        name: row.get(3)?,
        toa_unit: toa_unit.map(|s| s.parse::<TimeUnit>()).transpose()?,
        attributes: parse_attributes(&row.get::<_, String>(5)?)?,
    })
}

pub fn insert_train(conn: &Connection, train: &NewTrain) -> Result<TrainId> {
    conn.execute(
        "INSERT INTO signal_train (dataset_id, ordinal, name, toa_unit, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            train.dataset_id.get(),
            train.ordinal,
            train.name,
            train.toa_unit.map(|unit| unit.as_str()),
            attributes_json(&train.attributes)?,
        ],
    )?;
    Ok(TrainId::new(conn.last_insert_rowid()))
}

pub fn get_train(conn: &Connection, id: TrainId) -> Result<SignalTrain> {
    conn.query_row_and_then(
        &format!("SELECT {TRAIN_COLUMNS} FROM signal_train WHERE id = ?1"),
        [id.get()],
        train_from_row,
    )
    .map_err(|e| match e {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
            StoreError::not_found("train", id.get())
        }
        other => other,
    })
}

/// A dataset's trains, in order.
pub fn list_trains(conn: &Connection, dataset_id: DatasetId) -> Result<Vec<SignalTrain>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {TRAIN_COLUMNS} FROM signal_train WHERE dataset_id = ?1 ORDER BY ordinal"
    ))?;
    let rows = stmt.query_and_then([dataset_id.get()], train_from_row)?;
    rows.collect()
}

/// Every train in the library, newest dataset first.
pub fn list_all_trains(conn: &Connection) -> Result<Vec<SignalTrain>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {TRAIN_COLUMNS} FROM signal_train ORDER BY dataset_id DESC, ordinal"
    ))?;
    let rows = stmt.query_and_then([], train_from_row)?;
    rows.collect()
}

/// The train a group belongs to.
pub fn train_of_group(conn: &Connection, group_id: sp_core::GroupId) -> Result<TrainId> {
    let raw: i64 = conn
        .query_row(
            "SELECT train_id FROM signal_group WHERE id = ?1",
            [group_id.get()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::not_found("group", group_id.get()))?;
    Ok(TrainId::new(raw))
}

pub fn rename_train(conn: &Connection, id: TrainId, name: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE signal_train SET name = ?1 WHERE id = ?2",
        params![name, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("train", id.get()));
    }
    Ok(())
}

/// Replaces a train's attributes.
pub fn update_train_attributes(
    conn: &Connection,
    id: TrainId,
    attributes: &Attributes,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE signal_train SET attributes = ?1 WHERE id = ?2",
        params![attributes_json(attributes)?, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("train", id.get()));
    }
    Ok(())
}

/// Deletes a train with every group under it, releasing the blobs those
/// groups referenced.
pub fn delete_train(conn: &Connection, id: TrainId) -> Result<()> {
    let blobs = crate::library::blobs_under_train(conn, id)?;
    let deleted = conn.execute("DELETE FROM signal_train WHERE id = ?1", [id.get()])?;
    if deleted == 0 {
        return Err(StoreError::not_found("train", id.get()));
    }
    for blob_id in blobs {
        crate::blob::release(conn, blob_id)?;
    }
    Ok(())
}

/// How many pulses a train holds across its groups, and how many groups that
/// is. Both come from the group rows, so neither reads a column.
pub fn train_extent(conn: &Connection, id: TrainId) -> Result<(u32, u64)> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(actual_count), 0) FROM signal_group WHERE train_id = ?1",
        [id.get()],
        |row| {
            let groups: i64 = row.get(0)?;
            let pulses: i64 = row.get(1)?;
            Ok((groups.max(0) as u32, pulses.max(0) as u64))
        },
    )
    .map_err(Into::into)
}
