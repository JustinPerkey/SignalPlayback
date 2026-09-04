//! Content-addressed column blobs stored as chunked SQLite BLOBs
//! (`docs/DESIGN.md` §5.3).
//!
//! A blob's logical payload is a 64-byte [`ColumnHeader`] followed by packed
//! little-endian values. The payload is split across `sample_chunk` rows of a
//! fixed chunk size; reads use incremental blob I/O so a slice costs one copy
//! of exactly that span, and writes fill a zero-blob in place so a chunk is
//! never held twice in memory.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OptionalExtension, MAIN_DB};
use sp_core::signal::ParseTokenError;
use sp_core::{DType, SampleBuffer, SampleRange, Samples, Scaling, Timebase, C64};

use crate::error::{Result, StoreError};

sp_core::id_newtype! {
    /// Identifies a `sample_blob` row.
    BlobId
}

/// Magic bytes at offset 0 of every column blob.
pub const MAGIC: [u8; 4] = *b"SGB1";
/// Header layout version.
pub const FORMAT_VERSION: u16 = 1;
/// Fixed header length; values start here.
pub const HEADER_LEN: usize = 64;
/// Bytes per `sample_chunk` row unless a caller overrides it (§5.3).
pub const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// What a blob's bytes are, for `Verify Library` and maintenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobKind {
    Samples,
    Pyramid,
    Artifact,
}

impl BlobKind {
    pub const ALL: [Self; 3] = [Self::Samples, Self::Pyramid, Self::Artifact];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Samples => "samples",
            Self::Pyramid => "pyramid",
            Self::Artifact => "artifact",
        }
    }
}

impl fmt::Display for BlobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BlobKind {
    type Err = ParseTokenError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| ParseTokenError {
                kind: "blob kind",
                token: s.to_owned(),
            })
    }
}

/// The 64-byte header at the front of every column blob.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnHeader {
    pub dtype: DType,
    /// Always 1 in v1.
    pub channels: u8,
    /// `sample_rate_hz = None` is written as 0.0 and read back as irregular.
    pub timebase: Timebase,
    pub count: u64,
    pub scaling: Scaling,
}

impl ColumnHeader {
    #[must_use]
    pub fn new(dtype: DType, timebase: Timebase, count: u64, scaling: Scaling) -> Self {
        Self {
            dtype,
            channels: 1,
            timebase,
            count,
            scaling,
        }
    }

    /// A header describing `buffer` on `timebase`.
    #[must_use]
    pub fn for_buffer(buffer: &SampleBuffer, timebase: Timebase) -> Self {
        Self::new(
            buffer.dtype(),
            timebase,
            buffer.len() as u64,
            buffer.scaling(),
        )
    }

    /// Bytes the values occupy after the header.
    #[must_use]
    pub fn payload_len(&self) -> u64 {
        self.count * self.dtype.size_bytes() as u64
    }

    /// Header plus values.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        HEADER_LEN as u64 + self.payload_len()
    }

    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[6] = self.dtype.code();
        out[7] = self.channels;
        let rate = self.timebase.sample_rate_hz.unwrap_or(0.0);
        out[8..16].copy_from_slice(&rate.to_le_bytes());
        out[16..24].copy_from_slice(&self.timebase.t0_s.to_le_bytes());
        out[24..32].copy_from_slice(&self.count.to_le_bytes());
        out[32..40].copy_from_slice(&self.scaling.scale.to_le_bytes());
        out[40..48].copy_from_slice(&self.scaling.offset.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(StoreError::corrupt(format!(
                "column header is {} bytes, expected {HEADER_LEN}",
                bytes.len()
            )));
        }
        if bytes[0..4] != MAGIC {
            return Err(StoreError::corrupt("column header magic is not SGB1"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != FORMAT_VERSION {
            return Err(StoreError::corrupt(format!(
                "column header version {version} is not supported"
            )));
        }
        let dtype = DType::from_code(bytes[6])
            .ok_or_else(|| StoreError::corrupt(format!("unknown dtype code {}", bytes[6])))?;
        let channels = bytes[7];
        if channels != 1 {
            return Err(StoreError::corrupt(format!(
                "{channels} channels per column is not supported"
            )));
        }
        let f64_at = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[at..at + 8]);
            f64::from_le_bytes(b)
        };
        let rate = f64_at(8);
        let t0_s = f64_at(16);
        let mut count_bytes = [0u8; 8];
        count_bytes.copy_from_slice(&bytes[24..32]);
        let count = u64::from_le_bytes(count_bytes);
        let scaling = Scaling::new(f64_at(32), f64_at(40));
        let timebase = if rate > 0.0 && rate.is_finite() {
            Timebase::regular(rate, t0_s)
        } else {
            Timebase::irregular(t0_s)
        };
        Ok(Self {
            dtype,
            channels,
            timebase,
            count,
            scaling,
        })
    }
}

/// The `sample_blob` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    pub id: BlobId,
    pub checksum: String,
    pub byte_len: u64,
    pub chunk_size: usize,
    pub kind: BlobKind,
    pub refcount: i64,
}

impl BlobInfo {
    /// Number of `sample_chunk` rows the payload occupies.
    #[must_use]
    pub fn chunk_count(&self) -> u64 {
        self.byte_len.div_ceil(self.chunk_size as u64)
    }
}

/// Reads a blob's row.
pub fn info(conn: &Connection, id: BlobId) -> Result<BlobInfo> {
    conn.query_row(
        "SELECT id, checksum, byte_len, chunk_size, kind, refcount FROM sample_blob WHERE id = ?1",
        [id.get()],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    )
    .optional()?
    .ok_or_else(|| StoreError::not_found("blob", id.get()))
    .and_then(|(id, checksum, byte_len, chunk_size, kind, refcount)| {
        Ok(BlobInfo {
            id: BlobId::new(id),
            checksum,
            byte_len: u64::try_from(byte_len)
                .map_err(|_| StoreError::corrupt("negative blob byte_len"))?,
            chunk_size: usize::try_from(chunk_size)
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| StoreError::corrupt("blob chunk_size must be positive"))?,
            kind: kind.parse()?,
            refcount,
        })
    })
}

/// Every blob row, in id order.
pub fn list(conn: &Connection) -> Result<Vec<BlobInfo>> {
    let mut stmt =
        conn.prepare("SELECT id FROM sample_blob WHERE checksum NOT LIKE 'pending:%' ORDER BY id")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| info(conn, BlobId::new(id)))
        .collect()
}

/// Finds a blob by its content address.
pub fn find_by_checksum(conn: &Connection, checksum: &str) -> Result<Option<BlobId>> {
    Ok(conn
        .query_row(
            "SELECT id FROM sample_blob WHERE checksum = ?1",
            [checksum],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(BlobId::new))
}

static PENDING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Streams bytes into a new blob one chunk at a time.
///
/// The blob row is created up front with a placeholder checksum so chunk rows
/// have something to reference; [`BlobWriter::finish`] replaces it with the
/// blake3 address, or, if a blob with that address already exists, discards
/// the new chunks and bumps the existing blob's refcount instead. Either way
/// the returned id carries one reference owned by the caller.
///
/// Must be used inside a transaction: an abandoned writer leaves a pending
/// row behind that only a rollback removes.
#[derive(Debug)]
pub struct BlobWriter<'c> {
    conn: &'c Connection,
    id: i64,
    chunk_size: usize,
    buf: Vec<u8>,
    next_ordinal: i64,
    hasher: blake3::Hasher,
    total: u64,
}

impl<'c> BlobWriter<'c> {
    pub fn begin(conn: &'c Connection, kind: BlobKind, chunk_size: usize) -> Result<Self> {
        if chunk_size == 0 {
            return Err(StoreError::invalid("chunk size must be positive"));
        }
        let placeholder = format!(
            "pending:{}:{}",
            std::process::id(),
            PENDING_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        conn.execute(
            "INSERT INTO sample_blob (checksum, byte_len, chunk_size, kind, refcount)
             VALUES (?1, 0, ?2, ?3, 0)",
            params![placeholder, chunk_size as i64, kind.as_str()],
        )?;
        Ok(Self {
            conn,
            id: conn.last_insert_rowid(),
            chunk_size,
            buf: Vec::with_capacity(chunk_size.min(DEFAULT_CHUNK_SIZE)),
            next_ordinal: 0,
            hasher: blake3::Hasher::new(),
            total: 0,
        })
    }

    /// Appends bytes, flushing a chunk row each time `chunk_size` fills.
    pub fn write(&mut self, mut bytes: &[u8]) -> Result<()> {
        self.hasher.update(bytes);
        self.total += bytes.len() as u64;
        while !bytes.is_empty() {
            let room = self.chunk_size - self.buf.len();
            let take = room.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() == self.chunk_size {
                self.flush_chunk()?;
            }
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO sample_chunk (blob_id, ordinal, data) VALUES (?1, ?2, zeroblob(?3))",
            params![self.id, self.next_ordinal, self.buf.len() as i64],
        )?;
        let rowid = self.conn.last_insert_rowid();
        let mut blob = self
            .conn
            .blob_open(MAIN_DB, c"sample_chunk", c"data", rowid, false)?;
        blob.write_at(&self.buf, 0)?;
        drop(blob);
        self.next_ordinal += 1;
        self.buf.clear();
        Ok(())
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Finalises the blob and returns its id with one reference held.
    pub fn finish(mut self) -> Result<BlobId> {
        self.flush_chunk()?;
        let checksum = self.hasher.finalize().to_hex().to_string();

        if let Some(existing) = find_by_checksum(self.conn, &checksum)? {
            // Identical bytes already stored: drop ours, share theirs.
            self.conn
                .execute("DELETE FROM sample_blob WHERE id = ?1", [self.id])?;
            self.conn.execute(
                "UPDATE sample_blob SET refcount = refcount + 1 WHERE id = ?1",
                [existing.get()],
            )?;
            tracing::debug!(blob = existing.get(), "deduplicated blob write");
            return Ok(existing);
        }

        self.conn.execute(
            "UPDATE sample_blob SET checksum = ?1, byte_len = ?2, refcount = 1 WHERE id = ?3",
            params![checksum, self.total as i64, self.id],
        )?;
        Ok(BlobId::new(self.id))
    }
}

/// Takes one reference off a blob. Deletes it, chunks included, when the
/// last reference goes.
pub fn release(conn: &Connection, id: BlobId) -> Result<()> {
    conn.execute(
        "UPDATE sample_blob SET refcount = refcount - 1 WHERE id = ?1",
        [id.get()],
    )?;
    conn.execute(
        "DELETE FROM sample_blob WHERE id = ?1 AND refcount <= 0",
        [id.get()],
    )?;
    Ok(())
}

/// Adds a reference to an existing blob, for a second row that shares it.
pub fn retain(conn: &Connection, id: BlobId) -> Result<()> {
    let changed = conn.execute(
        "UPDATE sample_blob SET refcount = refcount + 1 WHERE id = ?1",
        [id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("blob", id.get()));
    }
    Ok(())
}

/// Deletes every blob nothing references. Returns how many went.
pub fn sweep_unreferenced(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM sample_blob WHERE refcount <= 0", [])?)
}

/// Reads `[start, start + len)` of a blob's payload, spanning chunks as
/// needed. The range is clamped to the payload.
pub fn read_bytes(conn: &Connection, id: BlobId, start: u64, len: usize) -> Result<Vec<u8>> {
    let meta = info(conn, id)?;
    read_bytes_with(conn, &meta, start, len)
}

/// [`read_bytes`] for a caller that already holds the blob's row.
pub fn read_bytes_with(
    conn: &Connection,
    meta: &BlobInfo,
    start: u64,
    len: usize,
) -> Result<Vec<u8>> {
    let start = start.min(meta.byte_len);
    let end = start.saturating_add(len as u64).min(meta.byte_len);
    let mut out = vec![0u8; (end - start) as usize];
    if out.is_empty() {
        return Ok(out);
    }

    let chunk_size = meta.chunk_size as u64;
    let first_chunk = start / chunk_size;
    let last_chunk = (end - 1) / chunk_size;

    let mut stmt =
        conn.prepare_cached("SELECT id FROM sample_chunk WHERE blob_id = ?1 AND ordinal = ?2")?;
    let mut handle: Option<rusqlite::blob::Blob<'_>> = None;
    let mut filled = 0usize;

    for ordinal in first_chunk..=last_chunk {
        let rowid: i64 = stmt
            .query_row(params![meta.id.get(), ordinal as i64], |row| row.get(0))
            .optional()?
            .ok_or_else(|| {
                StoreError::corrupt(format!("blob {} is missing chunk {ordinal}", meta.id.get()))
            })?;

        let chunk_start = ordinal * chunk_size;
        let chunk_end = (chunk_start + chunk_size).min(meta.byte_len);
        let read_from = start.max(chunk_start);
        let read_to = end.min(chunk_end);
        let span = (read_to - read_from) as usize;
        let offset = (read_from - chunk_start) as usize;

        let blob = match handle.as_mut() {
            Some(blob) => {
                blob.reopen(rowid)?;
                blob
            }
            None => {
                handle = Some(conn.blob_open(MAIN_DB, c"sample_chunk", c"data", rowid, true)?);
                handle.as_mut().expect("just set")
            }
        };
        if blob.size() < 0 || (blob.size() as u64) < (offset + span) as u64 {
            return Err(StoreError::corrupt(format!(
                "blob {} chunk {ordinal} is {} bytes, expected at least {}",
                meta.id.get(),
                blob.size(),
                offset + span
            )));
        }
        blob.read_at_exact(&mut out[filled..filled + span], offset)?;
        filled += span;
    }
    Ok(out)
}

/// Reads and decodes a column blob's header.
pub fn read_header(conn: &Connection, id: BlobId) -> Result<ColumnHeader> {
    let bytes = read_bytes(conn, id, 0, HEADER_LEN)?;
    ColumnHeader::decode(&bytes)
}

/// Writes a sample buffer as a column blob and returns its id.
pub fn write_column(
    conn: &Connection,
    buffer: &SampleBuffer,
    timebase: Timebase,
    chunk_size: usize,
) -> Result<BlobId> {
    let header = ColumnHeader::for_buffer(buffer, timebase);
    let mut writer = BlobWriter::begin(conn, BlobKind::Samples, chunk_size)?;
    writer.write(&header.encode())?;
    // Encode in bounded pieces so a 100 M-sample column never exists twice.
    const PIECE: usize = 1 << 16;
    let len = buffer.len();
    let mut at = 0;
    while at < len {
        let end = (at + PIECE).min(len);
        writer.write(&encode_samples(buffer.samples(), at..end))?;
        at = end;
    }
    writer.finish()
}

/// Reads `range` of a column blob back as a sample buffer, clamped to the
/// column. The buffer carries the column's scaling.
pub fn read_column(conn: &Connection, id: BlobId, range: SampleRange) -> Result<SampleBuffer> {
    let meta = info(conn, id)?;
    let header = ColumnHeader::decode(&read_bytes_with(conn, &meta, 0, HEADER_LEN)?)?;
    let start = range.start.min(header.count);
    let end = range.end.min(header.count);
    let size = header.dtype.size_bytes() as u64;
    let bytes = read_bytes_with(
        conn,
        &meta,
        HEADER_LEN as u64 + start * size,
        ((end - start) * size) as usize,
    )?;
    let samples = decode_samples(header.dtype, &bytes)?;
    Ok(SampleBuffer::new(samples).with_scaling(header.scaling))
}

/// Little-endian packing of one slice of samples.
#[must_use]
pub fn encode_samples(samples: &Samples, range: std::ops::Range<usize>) -> Vec<u8> {
    fn pack<T: Copy, const N: usize>(values: &[T], f: impl Fn(T) -> [u8; N]) -> Vec<u8> {
        let mut out = Vec::with_capacity(values.len() * N);
        for &v in values {
            out.extend_from_slice(&f(v));
        }
        out
    }
    match samples {
        Samples::F32(v) => pack(&v[range], f32::to_le_bytes),
        Samples::F64(v) => pack(&v[range], f64::to_le_bytes),
        Samples::I16(v) => pack(&v[range], i16::to_le_bytes),
        Samples::I32(v) => pack(&v[range], i32::to_le_bytes),
        Samples::U8(v) => v[range].to_vec(),
        Samples::C64(v) => pack(&v[range], |c: C64| {
            let mut b = [0u8; 8];
            b[..4].copy_from_slice(&c.re.to_le_bytes());
            b[4..].copy_from_slice(&c.im.to_le_bytes());
            b
        }),
    }
}

/// Inverse of [`encode_samples`].
pub fn decode_samples(dtype: DType, bytes: &[u8]) -> Result<Samples> {
    let size = dtype.size_bytes();
    if bytes.len() % size != 0 {
        return Err(StoreError::corrupt(format!(
            "{} payload bytes is not a multiple of {size}",
            bytes.len()
        )));
    }
    fn unpack<T, const N: usize>(bytes: &[u8], f: impl Fn([u8; N]) -> T) -> Vec<T> {
        bytes
            .chunks_exact(N)
            .map(|c| {
                let mut b = [0u8; N];
                b.copy_from_slice(c);
                f(b)
            })
            .collect()
    }
    Ok(match dtype {
        DType::F32 => Samples::F32(unpack(bytes, f32::from_le_bytes)),
        DType::F64 => Samples::F64(unpack(bytes, f64::from_le_bytes)),
        DType::I16 => Samples::I16(unpack(bytes, i16::from_le_bytes)),
        DType::I32 => Samples::I32(unpack(bytes, i32::from_le_bytes)),
        DType::U8 => Samples::U8(bytes.to_vec()),
        DType::C64 => Samples::C64(unpack(bytes, |b: [u8; 8]| {
            C64::new(
                f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                f32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            )
        })),
    })
}

/// Recomputes a blob's blake3 over its stored chunks, streaming.
pub fn rehash(conn: &Connection, meta: &BlobInfo) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut at = 0u64;
    let step = meta.chunk_size;
    while at < meta.byte_len {
        let piece = read_bytes_with(conn, meta, at, step)?;
        if piece.is_empty() {
            break;
        }
        hasher.update(&piece);
        at += piece.len() as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_every_dtype() {
        for dtype in DType::ALL {
            let header = ColumnHeader::new(
                dtype,
                Timebase::regular(48_000.0, 1.5),
                123_456,
                Scaling::new(0.5, -1.0),
            );
            let decoded = ColumnHeader::decode(&header.encode()).unwrap();
            assert_eq!(decoded, header);
        }
    }

    #[test]
    fn irregular_timebase_is_zero_rate_on_disk() {
        let header = ColumnHeader::new(DType::F64, Timebase::irregular(2.0), 10, Scaling::IDENTITY);
        let bytes = header.encode();
        assert_eq!(&bytes[8..16], &0.0f64.to_le_bytes());
        let decoded = ColumnHeader::decode(&bytes).unwrap();
        assert!(!decoded.timebase.is_regular());
        assert_eq!(decoded.timebase.t0_s, 2.0);
    }

    #[test]
    fn bad_headers_are_reported_not_panicked() {
        assert!(matches!(
            ColumnHeader::decode(&[0u8; 10]),
            Err(StoreError::Corrupt(_))
        ));
        let mut bytes =
            ColumnHeader::new(DType::F32, Timebase::irregular(0.0), 0, Scaling::IDENTITY).encode();
        bytes[0] = b'X';
        assert!(matches!(
            ColumnHeader::decode(&bytes),
            Err(StoreError::Corrupt(_))
        ));
        bytes[0] = b'S';
        bytes[6] = 42;
        assert!(matches!(
            ColumnHeader::decode(&bytes),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn samples_pack_and_unpack_for_every_dtype() {
        let cases = [
            Samples::F32(vec![1.5, -2.0, f32::NAN]),
            Samples::F64(vec![1.5, -2.0]),
            Samples::I16(vec![-1, 2, i16::MAX]),
            Samples::I32(vec![-1, i32::MIN]),
            Samples::U8(vec![0, 255]),
            Samples::C64(vec![C64::new(1.0, -1.0)]),
        ];
        for samples in cases {
            let bytes = encode_samples(&samples, 0..samples.len());
            let back = decode_samples(samples.dtype(), &bytes).unwrap();
            // NaN != NaN, so compare via the bytes.
            assert_eq!(encode_samples(&back, 0..back.len()), bytes);
        }
        assert!(decode_samples(DType::F32, &[1, 2, 3]).is_err());
    }
}
