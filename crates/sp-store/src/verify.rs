//! `Verify Library`: rehash every blob and reconcile references
//! (`docs/DESIGN.md` §14, Integrity).

use std::collections::HashMap;

use rusqlite::Connection;

use crate::blob::{self, BlobId};
use crate::error::Result;

/// A row whose `blob_id` points at nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingRef {
    pub table: &'static str,
    pub row_id: i64,
    pub blob_id: BlobId,
}

/// What a verification pass found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport {
    pub blobs_checked: usize,
    /// Stored checksum differs from the bytes.
    pub checksum_mismatches: Vec<BlobId>,
    /// Chunk bytes do not add up to `byte_len`, or a chunk is missing.
    pub length_mismatches: Vec<BlobId>,
    /// `refcount` disagrees with the rows actually referencing the blob.
    pub refcount_mismatches: Vec<(BlobId, i64, i64)>,
    /// Blobs no row references.
    pub unreferenced: Vec<BlobId>,
    /// Blob rows still carrying a placeholder checksum from an interrupted
    /// write that somehow committed.
    pub pending: Vec<BlobId>,
    pub dangling: Vec<DanglingRef>,
}

impl VerifyReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.checksum_mismatches.is_empty()
            && self.length_mismatches.is_empty()
            && self.refcount_mismatches.is_empty()
            && self.unreferenced.is_empty()
            && self.pending.is_empty()
            && self.dangling.is_empty()
    }

    /// Total problems found.
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.checksum_mismatches.len()
            + self.length_mismatches.len()
            + self.refcount_mismatches.len()
            + self.unreferenced.len()
            + self.pending.len()
            + self.dangling.len()
    }
}

/// Every column that owns a reference to a blob. The row identity is read as
/// `rowid`, which is the same value as `id` for the tables that declare one
/// and the only identity `render_pyramid` has (its key is the checksum).
const REFERENCING_COLUMNS: &[(&str, &str)] = &[
    ("signal", "blob_id"),
    ("signal", "time_blob_id"),
    ("signal_group", "toa_blob_id"),
    ("pulse_field", "blob_id"),
    ("render_pyramid", "blob_id"),
];

/// Runs every check. Read-only; safe on a pooled reader.
pub fn verify(conn: &Connection) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();

    // References actually present, per blob.
    let mut refs: HashMap<i64, i64> = HashMap::new();
    for (table, column) in REFERENCING_COLUMNS {
        let mut stmt = conn.prepare(&format!(
            "SELECT rowid, {column} FROM {table} WHERE {column} IS NOT NULL"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
        for row in rows {
            let (row_id, blob_id) = row?;
            *refs.entry(blob_id).or_default() += 1;
            let exists: bool = conn.query_row(
                "SELECT COUNT(*) FROM sample_blob WHERE id = ?1",
                [blob_id],
                |r| r.get::<_, i64>(0).map(|n| n > 0),
            )?;
            if !exists {
                report.dangling.push(DanglingRef {
                    table,
                    row_id,
                    blob_id: BlobId::new(blob_id),
                });
            }
        }
    }

    // Pending rows are never listed by `blob::list`; find them directly.
    {
        let mut stmt =
            conn.prepare("SELECT id FROM sample_blob WHERE checksum LIKE 'pending:%' ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        for row in rows {
            report.pending.push(BlobId::new(row?));
        }
    }

    for meta in blob::list(conn)? {
        report.blobs_checked += 1;

        let stored_len: Option<i64> = conn.query_row(
            "SELECT SUM(LENGTH(data)) FROM sample_chunk WHERE blob_id = ?1",
            [meta.id.get()],
            |row| row.get(0),
        )?;
        let chunk_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sample_chunk WHERE blob_id = ?1",
            [meta.id.get()],
            |row| row.get(0),
        )?;
        let expected_chunks = meta.chunk_count() as i64;
        if stored_len.unwrap_or(0) != meta.byte_len as i64 || chunk_rows != expected_chunks {
            report.length_mismatches.push(meta.id);
            // The hash would fail for the same reason; do not double-report.
        } else {
            match blob::rehash(conn, &meta) {
                Ok(hash) if hash == meta.checksum => {}
                Ok(_) => report.checksum_mismatches.push(meta.id),
                Err(_) => report.length_mismatches.push(meta.id),
            }
        }

        let actual = refs.get(&meta.id.get()).copied().unwrap_or(0);
        if actual == 0 {
            report.unreferenced.push(meta.id);
        } else if actual != meta.refcount {
            report
                .refcount_mismatches
                .push((meta.id, meta.refcount, actual));
        }
    }

    Ok(report)
}

/// Repairs what can be repaired without data: sets every refcount to the
/// observed reference count and deletes unreferenced and pending blobs.
/// Checksum and length mismatches are reported, never silently fixed.
pub fn repair_references(conn: &Connection, report: &VerifyReport) -> Result<usize> {
    let mut fixed = 0;
    for (id, _, actual) in &report.refcount_mismatches {
        conn.execute(
            "UPDATE sample_blob SET refcount = ?1 WHERE id = ?2",
            [*actual, id.get()],
        )?;
        fixed += 1;
    }
    for id in report.unreferenced.iter().chain(&report.pending) {
        conn.execute("DELETE FROM sample_blob WHERE id = ?1", [id.get()])?;
        fixed += 1;
    }
    Ok(fixed)
}
