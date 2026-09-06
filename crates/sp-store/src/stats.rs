//! Statistics computed by reading a column, and the library's own storage
//! figures (`docs/DESIGN.md` §12.1, §15.6).
//!
//! The `signal` and `pulse_field` rows already cache min, max, mean and RMS —
//! that is what the library table sorts on without touching a byte of sample
//! data. What they cannot cache is the shape of the distribution, so the
//! Inspector reads the column itself. It does that in one streaming pass,
//! [`CHUNK`] samples at a time, so profiling a 100 M-sample signal costs a few
//! megabytes of memory rather than a gigabyte.
//!
//! The cached min/max sets the histogram's span before the pass begins. When
//! there is none — an all-NaN column, or one whose statistics were never
//! written — a first pass finds the bounds and a second fills the bins.

use sp_core::stats::{Histogram, Profile, Profiler};
use sp_core::{GroupId, SampleRange, SignalId, SignalStats};

use crate::error::Result;
use crate::{blob, library, pulses, Connection};

/// Samples read per round trip while profiling. Large enough that the
/// per-read overhead disappears, small enough that the buffer stays in cache
/// terms of megabytes rather than the whole column.
const CHUNK: u64 = 1 << 20;

/// Histogram bins when the caller has no opinion.
pub const DEFAULT_BINS: usize = 64;

/// What one column looks like, end to end.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnProfile {
    /// Statistics recomputed from the samples, not the cached row.
    pub profile: Profile,
    /// Samples read, missing values included.
    pub samples: u64,
}

impl ColumnProfile {
    #[must_use]
    pub fn stats(&self) -> SignalStats {
        self.profile.stats
    }

    #[must_use]
    pub fn histogram(&self) -> &Histogram {
        &self.profile.histogram
    }
}

/// Profiles a stored signal: statistics, histogram and zero crossings in one
/// pass over its samples.
pub fn profile_signal(conn: &Connection, id: SignalId, bins: usize) -> Result<ColumnProfile> {
    let signal = library::get_signal(conn, id)?;
    let blob_id = library::signal_blob(conn, id)?;
    let bounds = signal.stats.and_then(span_of);
    profile_column(conn, blob_id, signal.sample_count, bounds, bins)
}

/// Profiles one pulse field's column (§6.6).
pub fn profile_pulse_field(
    conn: &Connection,
    group: GroupId,
    ordinal: u32,
    bins: usize,
) -> Result<ColumnProfile> {
    let field = pulses::list_fields(conn, group)?
        .into_iter()
        .find(|field| field.ordinal == ordinal)
        .ok_or_else(|| crate::StoreError::not_found("pulse field", i64::from(ordinal)))?;
    let group_row = library::get_group(conn, group)?;
    let blob_id = pulses::field_blob(conn, group, ordinal)?;
    let bounds = field.stats.and_then(span_of);
    profile_column(
        conn,
        blob_id,
        u64::from(group_row.actual_count),
        bounds,
        bins,
    )
}

/// The histogram span a cached summary implies, or `None` when it saw no
/// finite sample and so says nothing about where the values are.
fn span_of(stats: SignalStats) -> Option<(f64, f64)> {
    Some((stats.min()?, stats.max()?))
}

/// One streaming pass over a column blob, or two when the span has to be
/// discovered first.
fn profile_column(
    conn: &Connection,
    blob_id: blob::BlobId,
    samples: u64,
    bounds: Option<(f64, f64)>,
    bins: usize,
) -> Result<ColumnProfile> {
    let (low, high) = match bounds {
        Some(span) => span,
        None => {
            let mut stats = SignalStats::new();
            for_each_chunk(conn, blob_id, samples, |values| {
                for value in values {
                    stats.push(*value);
                }
            })?;
            // Nothing finite anywhere: bin an arbitrary unit span so the
            // caller still gets a well-formed, empty histogram.
            span_of(stats).unwrap_or((0.0, 0.0))
        }
    };

    let mut profiler = Profiler::new(low, high, bins);
    let mut read = 0_u64;
    for_each_chunk(conn, blob_id, samples, |values| {
        read += values.len() as u64;
        profiler.extend(values.iter().copied());
    })?;

    Ok(ColumnProfile {
        profile: profiler.finish(),
        samples: read,
    })
}

/// Reads the column in order, handing each chunk to `visit`.
fn for_each_chunk(
    conn: &Connection,
    blob_id: blob::BlobId,
    samples: u64,
    mut visit: impl FnMut(&[f64]),
) -> Result<()> {
    let mut start = 0_u64;
    while start < samples {
        let end = (start + CHUNK).min(samples);
        let buffer = blob::read_column(conn, blob_id, SampleRange::new(start, end))?;
        let values = buffer.to_f64();
        if values.is_empty() {
            break;
        }
        start += values.len() as u64;
        visit(&values);
    }
    Ok(())
}

/// What the library holds, in bytes and rows — the figures the Settings
/// screen reports (§15.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StorageStats {
    /// Payload bytes of the columns holding samples and pulse fields.
    pub sample_bytes: u64,
    /// Payload bytes of render pyramids, which are rebuildable.
    pub pyramid_bytes: u64,
    /// Payload bytes of artifact payloads too large for their JSON row.
    pub artifact_bytes: u64,
    /// Bytes reachable from no row at all, which `Verify Library` sweeps.
    pub unreferenced_bytes: u64,
    pub pipelines: u64,
    pub runs: u64,
    pub artifacts: u64,
    pub baselines: u64,
    pub tags: u64,
    pub property_defs: u64,
    /// Size of the library file on disk, WAL excluded.
    pub file_bytes: u64,
}

impl StorageStats {
    /// Total payload across every blob kind.
    #[must_use]
    pub fn blob_bytes(&self) -> u64 {
        self.sample_bytes + self.pyramid_bytes + self.artifact_bytes
    }
}

/// Storage and row counts for the whole library.
pub fn storage(conn: &Connection) -> Result<StorageStats> {
    let count = |sql: &str| -> Result<u64> {
        let n: i64 = conn.query_row(sql, [], |row| row.get(0))?;
        Ok(n.max(0) as u64)
    };
    let bytes_of_kind = |kind: &str| -> Result<u64> {
        let n: i64 = conn.query_row(
            "SELECT COALESCE(SUM(byte_len), 0) FROM sample_blob WHERE kind = ?1",
            [kind],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as u64)
    };

    let page_size = count("PRAGMA page_size")?;
    let page_count = count("PRAGMA page_count")?;

    Ok(StorageStats {
        sample_bytes: bytes_of_kind("samples")?,
        pyramid_bytes: bytes_of_kind("pyramid")?,
        artifact_bytes: bytes_of_kind("artifact")?,
        unreferenced_bytes: count(
            "SELECT COALESCE(SUM(byte_len), 0) FROM sample_blob WHERE refcount <= 0",
        )?,
        pipelines: count("SELECT COUNT(*) FROM pipeline")?,
        runs: count("SELECT COUNT(*) FROM run")?,
        artifacts: count("SELECT COUNT(*) FROM artifact")?,
        baselines: count("SELECT COUNT(*) FROM baseline")?,
        tags: count("SELECT COUNT(*) FROM tag")?,
        property_defs: count("SELECT COUNT(*) FROM property_def")?,
        file_bytes: page_size * page_count,
    })
}

#[cfg(test)]
mod tests {
    use sp_core::{Attributes, DType, Domain, Provenance, SampleBuffer, SourceKind, Timebase};

    use super::*;
    use crate::library::{NewDataset, NewGroup, NewSignal};
    use crate::trains::{self, NewTrain};

    fn library() -> (tempfile::TempDir, crate::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::Store::open(dir.path().join("library.db")).unwrap();
        (dir, store)
    }

    fn insert_signal(conn: &Connection, values: &[f64]) -> Result<SignalId> {
        let dataset =
            library::insert_dataset(conn, &NewDataset::new("stats", SourceKind::Generated))?;
        let train = trains::insert_train(conn, &NewTrain::new(dataset, 0))?;
        let group = library::insert_group(conn, &NewGroup::new(train, 0, 1))?;
        library::insert_signal(
            conn,
            &NewSignal::new(
                group,
                0,
                "wave",
                Timebase::regular(1_000.0, 0.0),
                SampleBuffer::from_f64(DType::F64, values),
            )
            .with_domain(Domain::Analog)
            .with_provenance(Provenance::Generated)
            .with_attributes(Attributes::new()),
        )
    }

    #[test]
    fn a_pulse_field_profiles_like_a_signal_does() {
        use crate::pulses::{self, NewPulseField, NewPulseGroup};
        use sp_core::TimeUnit;

        let (_dir, store) = library();
        let group = store
            .write(|conn| {
                let dataset = library::insert_dataset(
                    conn,
                    &NewDataset::new("pulses", SourceKind::CsvImport),
                )?;
                let train = trains::insert_train(
                    conn,
                    &NewTrain::new(dataset, 0).with_toa_unit(TimeUnit::Microseconds),
                )?;
                pulses::insert_pulse_group(
                    conn,
                    &NewPulseGroup::new(train, 0, vec![0.0, 1e-5, 2e-5, 3e-5]).with_field(
                        NewPulseField::new("pulse width", vec![1.0, 2.0, f64::NAN, 4.0]),
                    ),
                )
            })
            .unwrap();

        let profile = store
            .read(move |conn| profile_pulse_field(conn, group, 0, 16))
            .unwrap();
        assert_eq!(profile.samples, 4);
        assert_eq!(profile.stats().count(), 3);
        assert_eq!(profile.stats().non_finite(), 1);
        assert_eq!(profile.stats().min(), Some(1.0));
        assert_eq!(profile.stats().max(), Some(4.0));
        // Three finite values binned, the missing one counted apart.
        assert_eq!(profile.histogram().total(), 3);
        assert_eq!(profile.histogram().outside(), (0, 0, 1));

        // A column that is not there is a not-found, not a panic.
        assert!(store
            .read(move |conn| profile_pulse_field(conn, group, 9, 16))
            .is_err());
    }

    #[test]
    fn a_signal_profiles_in_one_pass() {
        let (_dir, store) = library();
        let values: Vec<f64> = (0..1_000).map(|i| (f64::from(i) * 0.01).sin()).collect();
        let id = store
            .write(move |conn| insert_signal(conn, &values))
            .unwrap();

        let profile = store
            .read(move |conn| profile_signal(conn, id, 32))
            .unwrap();
        assert_eq!(profile.samples, 1_000);
        assert_eq!(profile.stats().count(), 1_000);
        assert_eq!(profile.histogram().bins(), 32);
        assert_eq!(profile.histogram().total(), 1_000);
        // The cached bounds are the histogram's span, so nothing falls outside.
        assert_eq!(profile.histogram().outside(), (0, 0, 0));
        // sin over 0..10 rad crosses zero three times.
        assert_eq!(profile.profile.zero_crossings, 3);
    }

    #[test]
    fn profiling_reads_every_chunk_of_a_long_column() {
        let (_dir, store) = library();
        // Longer than one read, so the chunk loop has to come round again.
        let count = (CHUNK + 1_234) as usize;
        let values: Vec<f64> = (0..count)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let id = store
            .write(move |conn| insert_signal(conn, &values))
            .unwrap();

        let profile = store.read(move |conn| profile_signal(conn, id, 8)).unwrap();
        assert_eq!(profile.samples, count as u64);
        assert_eq!(profile.histogram().total(), count as u64);
        assert_eq!(profile.profile.zero_crossings, count as u64 - 1);
    }

    #[test]
    fn an_all_missing_column_still_profiles() {
        let (_dir, store) = library();
        let values = vec![f64::NAN; 16];
        let id = store
            .write(move |conn| insert_signal(conn, &values))
            .unwrap();

        let profile = store.read(move |conn| profile_signal(conn, id, 8)).unwrap();
        assert_eq!(profile.samples, 16);
        assert!(profile.stats().is_empty());
        assert!(profile.histogram().is_empty());
        assert_eq!(profile.histogram().outside(), (0, 0, 16));
    }

    #[test]
    fn storage_counts_what_the_library_holds() {
        let (_dir, store) = library();
        let values: Vec<f64> = (0..500).map(f64::from).collect();
        store
            .write(move |conn| insert_signal(conn, &values))
            .unwrap();

        let stats = store.read(storage).unwrap();
        assert!(stats.sample_bytes >= 500 * 8, "{stats:?}");
        assert_eq!(stats.pyramid_bytes, 0);
        assert_eq!(stats.runs, 0);
        assert!(stats.file_bytes > 0);
        assert_eq!(stats.blob_bytes(), stats.sample_bytes);
    }
}
