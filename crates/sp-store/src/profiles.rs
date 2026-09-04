//! Saved import profiles (`docs/DESIGN.md` §7.3, §15.1).
//!
//! The rules themselves belong to `sp-csv`; this crate only stores the JSON
//! and the name it is filed under, so a recurring file shape is one pick in
//! the import screen.

use rusqlite::{params, Connection, OptionalExtension, Row};
use sp_core::group::DatasetId;

use crate::error::{Result, StoreError};
use crate::library::{format_timestamp, parse_timestamp};
use sp_core::time::{now_utc, Timestamp};

/// A stored profile: a name, the serialised rules, and when it was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedProfile {
    pub id: i64,
    pub name: String,
    /// Opaque here; `sp_csv::ImportProfile` is what reads it.
    pub rules_json: String,
    pub created_utc: Timestamp,
}

const COLUMNS: &str = "id, name, rules_json, created_utc";

fn from_row(row: &Row<'_>) -> Result<SavedProfile> {
    Ok(SavedProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        rules_json: row.get(2)?,
        created_utc: parse_timestamp(&row.get::<_, String>(3)?)?,
    })
}

/// Saves a profile, replacing any profile of the same name. Returns its id.
pub fn save(conn: &Connection, name: &str, rules_json: &str) -> Result<i64> {
    let name = name.trim();
    if name.is_empty() {
        return Err(StoreError::invalid("an import profile needs a name"));
    }
    conn.execute(
        "INSERT INTO import_profile (name, rules_json, created_utc)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET rules_json = excluded.rules_json,
                                         created_utc = excluded.created_utc",
        params![name, rules_json, format_timestamp(now_utc())?],
    )?;
    get_by_name(conn, name)?
        .map(|profile| profile.id)
        .ok_or_else(|| StoreError::corrupt(format!("import profile '{name}' vanished on save")))
}

pub fn get(conn: &Connection, id: i64) -> Result<SavedProfile> {
    conn.query_row_and_then(
        &format!("SELECT {COLUMNS} FROM import_profile WHERE id = ?1"),
        [id],
        from_row,
    )
    .map_err(|error| match error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
            StoreError::not_found("import profile", id)
        }
        other => other,
    })
}

pub fn get_by_name(conn: &Connection, name: &str) -> Result<Option<SavedProfile>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM import_profile WHERE name = ?1"),
        [name],
        |row| Ok(from_row(row)),
    )
    .optional()?
    .transpose()
}

/// Every saved profile, newest first.
pub fn list(conn: &Connection) -> Result<Vec<SavedProfile>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM import_profile ORDER BY created_utc DESC, name"
    ))?;
    let rows = stmt.query_and_then([], from_row)?;
    rows.collect()
}

/// Deletes a profile. Datasets that used it keep their own copy of the layout,
/// so the reference is cleared rather than the delete refused.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    conn.execute(
        "UPDATE dataset SET profile_id = NULL WHERE profile_id = ?1",
        [id],
    )?;
    Ok(conn.execute("DELETE FROM import_profile WHERE id = ?1", [id])? > 0)
}

/// Records which saved profile a dataset was imported with.
pub fn link_dataset(conn: &Connection, dataset_id: DatasetId, profile_id: i64) -> Result<()> {
    let changed = conn.execute(
        "UPDATE dataset SET profile_id = ?2 WHERE id = ?1",
        params![dataset_id.get(), profile_id],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("dataset", dataset_id.get()));
    }
    Ok(())
}

/// The profile a dataset was imported with, if it is still stored.
pub fn dataset_profile(conn: &Connection, dataset_id: DatasetId) -> Result<Option<SavedProfile>> {
    let id: Option<i64> = conn.query_row(
        "SELECT profile_id FROM dataset WHERE id = ?1",
        [dataset_id.get()],
        |row| row.get(0),
    )?;
    match id {
        Some(id) => get(conn, id).map(Some),
        None => Ok(None),
    }
}
