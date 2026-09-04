//! The render-pyramid index (`docs/DESIGN.md` §5.4).
//!
//! This module knows *where* a pyramid is, never what its bytes mean: the
//! format, the builder and the level arithmetic live in `sp-engine`, which is
//! the crate that draws. All that is stored here is the mapping from a
//! column's content address to the blob holding its reduction, plus enough of
//! the pyramid's header to choose a level without reading any bytes.
//!
//! Pyramids are derived data. [`clear_all`] drops every one of them, which is
//! always safe — the next viewport that needs one rebuilds it.

use rusqlite::{params, Connection, Row};
use sp_core::time::{now_utc, Timestamp};

use crate::blob::{self, BlobId};
use crate::error::{Result, StoreError};
use crate::library::{format_timestamp, parse_timestamp};

/// One `render_pyramid` row: a reduction of the column addressed by
/// `source_checksum`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyramidRef {
    pub source_checksum: String,
    pub blob_id: BlobId,
    /// Levels the blob holds, coarsest last.
    pub level_count: u32,
    /// Level 0 reduces `1 << base_shift` source values into one cell.
    pub base_shift: u32,
    /// Values the pyramid was built over.
    pub source_count: u64,
    pub built_utc: Timestamp,
}

const COLUMNS: &str = "source_checksum, blob_id, level_count, base_shift, source_count, built_utc";

fn from_row(row: &Row<'_>) -> Result<PyramidRef> {
    Ok(PyramidRef {
        source_checksum: row.get(0)?,
        blob_id: BlobId::new(row.get(1)?),
        level_count: nonneg(row.get::<_, i64>(2)?, "level_count")? as u32,
        base_shift: nonneg(row.get::<_, i64>(3)?, "base_shift")? as u32,
        source_count: nonneg(row.get::<_, i64>(4)?, "source_count")?,
        built_utc: parse_timestamp(&row.get::<_, String>(5)?)?,
    })
}

fn nonneg(value: i64, what: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::corrupt(format!("negative pyramid {what}")))
}

/// The pyramid for the column with this content address, if one is built.
pub fn find(conn: &Connection, source_checksum: &str) -> Result<Option<PyramidRef>> {
    conn.query_row_and_then(
        &format!("SELECT {COLUMNS} FROM render_pyramid WHERE source_checksum = ?1"),
        [source_checksum],
        from_row,
    )
    .map(Some)
    .or_else(|error| match error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        other => Err(other),
    })
}

/// Records `pyramid_blob` as the reduction of the column addressed by
/// `source_checksum`.
///
/// The row takes over the reference [`crate::blob::BlobWriter::finish`] handed
/// the caller, which is the same bargain a `signal` row strikes with its
/// column. A pyramid already indexed for that address wins: the caller's
/// reference is released and the existing row returned, so two viewports
/// racing to build the same pyramid cost one wasted build, never a corrupt
/// index or a leaked blob.
pub fn link(
    conn: &Connection,
    source_checksum: &str,
    pyramid_blob: BlobId,
    level_count: u32,
    base_shift: u32,
    source_count: u64,
) -> Result<PyramidRef> {
    if let Some(existing) = find(conn, source_checksum)? {
        // The stored row already owns a reference; the caller's is surplus,
        // even when the dedup in `finish` handed back the very same blob.
        blob::release(conn, pyramid_blob)?;
        return Ok(existing);
    }

    let built = now_utc();
    conn.execute(
        "INSERT INTO render_pyramid
             (source_checksum, blob_id, level_count, base_shift, source_count, built_utc)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            source_checksum,
            pyramid_blob.get(),
            i64::from(level_count),
            i64::from(base_shift),
            i64::try_from(source_count)
                .map_err(|_| StoreError::invalid("pyramid source count does not fit an i64"))?,
            format_timestamp(built)?,
        ],
    )?;

    Ok(PyramidRef {
        source_checksum: source_checksum.to_owned(),
        blob_id: pyramid_blob,
        level_count,
        base_shift,
        source_count,
        built_utc: built,
    })
}

/// Forgets one pyramid, releasing its blob.
pub fn unlink(conn: &Connection, source_checksum: &str) -> Result<bool> {
    let Some(existing) = find(conn, source_checksum)? else {
        return Ok(false);
    };
    conn.execute(
        "DELETE FROM render_pyramid WHERE source_checksum = ?1",
        [source_checksum],
    )?;
    blob::release(conn, existing.blob_id)?;
    Ok(true)
}

/// Drops every pyramid — the `Rebuild pyramids` maintenance action (§5.4).
/// Returns how many went.
pub fn clear_all(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT source_checksum FROM render_pyramid")?;
    let checksums: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut dropped = 0;
    for checksum in checksums {
        if unlink(conn, &checksum)? {
            dropped += 1;
        }
    }
    Ok(dropped)
}

/// Every pyramid in the library, oldest first.
pub fn list(conn: &Connection) -> Result<Vec<PyramidRef>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM render_pyramid ORDER BY built_utc, source_checksum"
    ))?;
    let rows = stmt.query_and_then([], from_row)?;
    rows.collect()
}
