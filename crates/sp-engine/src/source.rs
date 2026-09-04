//! Reading a stored column the way a frame needs it (`docs/DESIGN.md` §5.4).
//!
//! [`ColumnSource`] is the [`ColumnReader`] a library provides: it opens one
//! column, finds the pyramid built over it, and answers a viewport's questions
//! with one bounded incremental blob read each. [`build`] is the job that
//! makes that pyramid, lazily, on a worker thread.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sp_core::stats::MinMax;
use sp_core::SampleRange;
use sp_store::blob::{self, BlobId, BlobKind, ColumnHeader};
use sp_store::{pyramid as index, Connection, PyramidRef, Store, DEFAULT_CHUNK_SIZE};

use crate::error::{EngineError, Result};
use crate::pyramid::{decode_cells, PyramidBuilder, PyramidHeader, BASE_SHIFT};
use crate::reduce::ColumnReader;

/// Values read per pass while building a pyramid. One chunk is the only part
/// of the column ever held in memory, and the boundary is where cancellation
/// is checked (§4.2).
pub const BUILD_CHUNK: u64 = 262_144;

/// How far a pyramid build has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildProgress {
    pub values_done: u64,
    pub values_total: u64,
}

impl BuildProgress {
    /// Completion in `0.0..=1.0`.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        if self.values_total == 0 {
            return None;
        }
        Some((self.values_done as f32 / self.values_total as f32).clamp(0.0, 1.0))
    }
}

/// The cancel flag and progress sink handed to a pyramid build.
///
/// Same shape as `sp_gen::GenControl` and `sp_csv::ImportControl`, and for the
/// same reason: the builder owns no threads, it calls into a control at chunk
/// boundaries (G5).
#[derive(Clone, Default)]
pub struct BuildControl {
    cancel: Option<Arc<AtomicBool>>,
    progress: Option<Arc<dyn Fn(BuildProgress) + Send + Sync>>,
}

impl fmt::Debug for BuildControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildControl")
            .field("cancelled", &self.is_cancelled())
            .field("reports_progress", &self.progress.is_some())
            .finish()
    }
}

impl BuildControl {
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
    pub fn with_progress(mut self, progress: Arc<dyn Fn(BuildProgress) + Send + Sync>) -> Self {
        self.progress = Some(progress);
        self
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// `Err(Cancelled)` once the flag is set, so a `?` unwinds before anything
    /// is written.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        Ok(())
    }

    pub fn report(&self, progress: BuildProgress) {
        if let Some(sink) = &self.progress {
            sink(progress);
        }
    }
}

/// One column of a library, opened for drawing.
///
/// Holds metadata only: the column header, the blob row, and the pyramid row
/// if there is one. Samples are read per call, one span at a time.
#[derive(Debug)]
pub struct ColumnSource<'c> {
    conn: &'c Connection,
    blob: BlobId,
    column: ColumnHeader,
    /// The pyramid over this column, when one is built.
    pyramid: Option<(PyramidRef, PyramidHeader)>,
    checksum: String,
}

impl<'c> ColumnSource<'c> {
    /// Opens the column stored in `blob`, picking up its pyramid if one has
    /// been built. A missing pyramid is not an error: reads fall back to the
    /// raw path until [`build`] has run.
    pub fn open(conn: &'c Connection, blob: BlobId) -> Result<Self> {
        let meta = blob::info(conn, blob)?;
        let column = blob::read_header(conn, blob)?;
        let pyramid = match index::find(conn, &meta.checksum)? {
            Some(row) => {
                let bytes = blob::read_bytes(conn, row.blob_id, 0, crate::pyramid::HEADER_LEN)?;
                match PyramidHeader::decode(&bytes) {
                    Ok(header) if header.source_count == column.count => Some((row, header)),
                    // A pyramid that does not describe this column is stale
                    // derived data, not a fault to propagate: read raw and
                    // leave the rebuild to maintenance.
                    Ok(_) => {
                        tracing::warn!(
                            blob = blob.get(),
                            "pyramid does not match its column; ignoring it"
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(%error, blob = blob.get(), "ignoring a corrupt pyramid");
                        None
                    }
                }
            }
            None => None,
        };
        Ok(Self {
            conn,
            blob,
            column,
            pyramid,
            checksum: meta.checksum,
        })
    }

    /// The column's own header — dtype, timebase and value count.
    #[must_use]
    pub fn column(&self) -> ColumnHeader {
        self.column
    }

    /// The content address of the column, which is what its pyramid is filed
    /// under.
    #[must_use]
    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    #[must_use]
    pub fn has_pyramid(&self) -> bool {
        self.pyramid.is_some()
    }

    /// The header of the pyramid over this column, if one is built.
    #[must_use]
    pub fn pyramid_header(&self) -> Option<PyramidHeader> {
        self.pyramid.as_ref().map(|(_, header)| *header)
    }
}

impl ColumnReader for ColumnSource<'_> {
    fn values(&self, range: SampleRange) -> Result<Vec<f64>> {
        Ok(blob::read_column(self.conn, self.blob, range)?.to_f64())
    }

    fn pyramid(&self) -> Option<PyramidHeader> {
        self.pyramid.as_ref().map(|(_, header)| *header)
    }

    fn cells(&self, level: u32, start: u64, end: u64) -> Result<Vec<MinMax>> {
        let Some((row, header)) = self.pyramid.as_ref() else {
            return Ok(Vec::new());
        };
        let Some((offset, len)) = header.cell_bytes(level, start, end) else {
            return Ok(Vec::new());
        };
        let bytes = blob::read_bytes(self.conn, row.blob_id, offset, len)?;
        decode_cells(&bytes)
    }
}

/// Builds the pyramid for `source` and files it, or returns the one already
/// filed (§5.4).
///
/// The source is streamed a chunk at a time, so a 100 M-sample column is
/// reduced without ever holding more than a chunk of it; what the build does
/// hold is the pyramid it is producing, about 3% of the column.
pub fn build(store: &Store, source: BlobId, control: &BuildControl) -> Result<PyramidRef> {
    let (checksum, count, existing) = store.read(|conn| {
        let meta = blob::info(conn, source)?;
        let header = blob::read_header(conn, source)?;
        let existing = index::find(conn, &meta.checksum)?;
        Ok((meta.checksum, header.count, existing))
    })?;
    if let Some(existing) = existing {
        return Ok(existing);
    }

    let mut builder = PyramidBuilder::new();
    let mut at = 0u64;
    while at < count {
        control.check()?;
        let end = (at + BUILD_CHUNK).min(count);
        let chunk =
            store.read(|conn| blob::read_column(conn, source, SampleRange::new(at, end)))?;
        builder.extend(chunk.values());
        at = end;
        control.report(BuildProgress {
            values_done: at,
            values_total: count,
        });
    }
    control.check()?;

    let pyramid = builder.finish();
    if pyramid.header().source_count != count {
        return Err(EngineError::corrupt(format!(
            "column {} changed under a pyramid build",
            source.get()
        )));
    }
    let bytes = pyramid.encode();
    let header = pyramid.header();

    let row = store.write(move |conn| {
        let tx = conn.transaction()?;
        let mut writer = blob::BlobWriter::begin(&tx, BlobKind::Pyramid, DEFAULT_CHUNK_SIZE)?;
        writer.write(&bytes)?;
        let blob_id = writer.finish()?;
        let row = index::link(
            &tx,
            &checksum,
            blob_id,
            header.level_count,
            header.base_shift,
            header.source_count,
        )?;
        tx.commit()?;
        Ok(row)
    })?;

    tracing::debug!(
        blob = source.get(),
        levels = row.level_count,
        values = count,
        "pyramid built"
    );
    Ok(row)
}

/// Builds the pyramid for `source` unless one is already filed. The level-0
/// reduction the design fixes at 64:1 is [`BASE_SHIFT`]; nothing else picks it.
pub fn ensure(store: &Store, source: BlobId) -> Result<PyramidRef> {
    debug_assert_eq!(BASE_SHIFT, 6);
    build(store, source, &BuildControl::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pyramid::{cells_at, Pyramid};
    use sp_core::{SampleBuffer, Samples, Timebase};

    fn library() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        (dir, store)
    }

    fn write_column(store: &Store, values: Vec<f64>) -> BlobId {
        store
            .write(move |conn| {
                let tx = conn.transaction()?;
                let buffer = SampleBuffer::new(Samples::F64(values));
                let id = blob::write_column(
                    &tx,
                    &buffer,
                    Timebase::regular(1_000.0, 0.0),
                    DEFAULT_CHUNK_SIZE,
                )?;
                tx.commit()?;
                Ok(id)
            })
            .unwrap()
    }

    fn ramp(n: u64) -> Vec<f64> {
        (0..n).map(|i| i as f64).collect()
    }

    fn pyramid_rows(store: &Store) -> i64 {
        store
            .read(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM render_pyramid", [], |row| row.get(0))?)
            })
            .unwrap()
    }

    fn refcount(store: &Store, id: BlobId) -> i64 {
        store
            .read(move |conn| Ok(blob::info(conn, id)?.refcount))
            .unwrap()
    }

    #[test]
    fn a_built_pyramid_matches_one_built_in_memory() {
        let (_dir, store) = library();
        let values = ramp(5_000);
        let blob_id = write_column(&store, values.clone());
        let row = ensure(&store, blob_id).unwrap();

        let expected = Pyramid::build(values.iter().copied());
        assert_eq!(row.level_count, expected.header().level_count);
        assert_eq!(row.source_count, 5_000);

        store
            .read(|conn| {
                let source = ColumnSource::open(conn, blob_id).unwrap();
                assert!(source.has_pyramid());
                for level in 0..row.level_count {
                    let cells = source
                        .cells(level, 0, cells_at(5_000, BASE_SHIFT, level))
                        .unwrap();
                    assert_eq!(cells, expected.level(level).unwrap(), "level {level}");
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn building_twice_reuses_the_first_pyramid() {
        let (_dir, store) = library();
        let blob_id = write_column(&store, ramp(2_000));
        let first = ensure(&store, blob_id).unwrap();
        let second = ensure(&store, blob_id).unwrap();
        assert_eq!(first, second);

        assert_eq!(pyramid_rows(&store), 1);
        // Filing a pyramid twice must not leave the blob over-referenced.
        assert_eq!(refcount(&store, first.blob_id), 1);
    }

    #[test]
    fn two_columns_with_the_same_bytes_share_one_pyramid() {
        let (_dir, store) = library();
        let first = write_column(&store, ramp(1_000));
        let second = write_column(&store, ramp(1_000));
        // Content addressing means these are already the same blob.
        assert_eq!(first, second);
        let row = ensure(&store, first).unwrap();
        assert_eq!(row, ensure(&store, second).unwrap());
        assert_eq!(pyramid_rows(&store), 1);
        assert_eq!(refcount(&store, row.blob_id), 1);
    }

    #[test]
    fn a_cancelled_build_writes_nothing() {
        let (_dir, store) = library();
        let blob_id = write_column(&store, ramp(1_000_000));
        let flag = Arc::new(AtomicBool::new(true));
        let control = BuildControl::new().with_cancel(flag);
        assert!(matches!(
            build(&store, blob_id, &control),
            Err(EngineError::Cancelled)
        ));
        assert_eq!(pyramid_rows(&store), 0);
    }

    #[test]
    fn progress_covers_the_whole_column() {
        let (_dir, store) = library();
        let blob_id = write_column(&store, ramp(BUILD_CHUNK * 2 + 7));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let control = BuildControl::new().with_progress(Arc::new(move |p: BuildProgress| {
            sink.lock().unwrap().push(p)
        }));
        build(&store, blob_id, &control).unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen.last().unwrap().values_done, BUILD_CHUNK * 2 + 7);
        assert_eq!(seen.last().unwrap().fraction(), Some(1.0));
    }

    #[test]
    fn dropping_a_pyramid_frees_its_blob() {
        let (_dir, store) = library();
        let blob_id = write_column(&store, ramp(2_000));
        let row = ensure(&store, blob_id).unwrap();

        store
            .write(move |conn| {
                assert_eq!(index::clear_all(conn)?, 1);
                Ok(())
            })
            .unwrap();

        store
            .read(move |conn| {
                assert!(index::find(conn, &row.source_checksum)?.is_none());
                assert!(blob::info(conn, row.blob_id).is_err(), "the blob went too");
                // The column itself is untouched.
                assert!(blob::info(conn, blob_id).is_ok());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_column_with_no_pyramid_still_opens() {
        let (_dir, store) = library();
        let blob_id = write_column(&store, ramp(500));
        store
            .read(|conn| {
                let source = ColumnSource::open(conn, blob_id).unwrap();
                assert!(!source.has_pyramid());
                assert_eq!(source.pyramid(), None);
                assert!(source.cells(0, 0, 10).unwrap().is_empty());
                assert_eq!(
                    source.values(SampleRange::new(0, 3)).unwrap(),
                    [0.0, 1.0, 2.0]
                );
                assert_eq!(source.column().count, 500);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn an_empty_column_gets_an_empty_pyramid() {
        let (_dir, store) = library();
        let blob_id = write_column(&store, Vec::new());
        let row = ensure(&store, blob_id).unwrap();
        assert_eq!(row.level_count, 0);
        store
            .read(|conn| {
                let source = ColumnSource::open(conn, blob_id).unwrap();
                assert_eq!(source.pyramid().map(|h| h.level_count), Some(0));
                Ok(())
            })
            .unwrap();
    }
}
