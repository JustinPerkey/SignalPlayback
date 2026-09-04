//! Multi-resolution min/max pyramids (`docs/DESIGN.md` §5.4).
//!
//! Rendering 100 M points a frame is not feasible, so every column gets a
//! derived reduction: level 0 stores one `(min, max)` pair per 64 source
//! values and each level above halves the resolution again. The renderer picks
//! the level where one cell lands within a pixel or two, reads a few kilobytes
//! of it, and draws vertical bars — so draw cost tracks viewport *width*, not
//! signal length, which is what makes G2 reachable.
//!
//! Level 0 is `count / 64` cells of 8 bytes — 3.1% of an `f32` column — and
//! the levels above it sum to one more of the same, so a whole pyramid costs
//! about 6% of what it reduces.
//!
//! The bytes are stored as an ordinary content-addressed blob with
//! `kind = 'pyramid'`; `sp_store::pyramid` maps a column's checksum to it.
//! Pyramids are derived data — deleting one is always safe.

use sp_core::stats::MinMax;

use crate::error::{EngineError, Result};

/// Magic bytes at offset 0 of a pyramid blob.
pub const MAGIC: [u8; 4] = *b"SGP1";
/// Header layout version.
pub const FORMAT_VERSION: u16 = 1;
/// Fixed header length; level 0's cells start here.
pub const HEADER_LEN: usize = 32;
/// Level 0 reduces `1 << BASE_SHIFT` source values into one cell — 64:1.
pub const BASE_SHIFT: u32 = 6;
/// Bytes per cell: two little-endian `f32`.
pub const CELL_BYTES: usize = 8;

/// Cells at `level` for a column of `count` values.
#[must_use]
pub fn cells_at(count: u64, base_shift: u32, level: u32) -> u64 {
    let shift = base_shift + level;
    if count == 0 || shift >= 64 {
        return u64::from(count > 0);
    }
    count.div_ceil(1 << shift)
}

/// Source values one cell of `level` covers.
#[must_use]
pub fn span_at(base_shift: u32, level: u32) -> u64 {
    let shift = base_shift + level;
    if shift >= 64 {
        u64::MAX
    } else {
        1 << shift
    }
}

/// Levels a column of `count` values needs: enough that the coarsest level is
/// a single cell, so any viewport has a level that fits.
#[must_use]
pub fn level_count(count: u64, base_shift: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    let mut levels = 1;
    while cells_at(count, base_shift, levels - 1) > 1 {
        levels += 1;
    }
    levels
}

/// Byte offset of `level`'s first cell, measured from the start of the blob.
#[must_use]
pub fn level_offset(count: u64, base_shift: u32, level: u32) -> u64 {
    let cells: u64 = (0..level)
        .map(|k| cells_at(count, base_shift, k))
        .sum::<u64>();
    HEADER_LEN as u64 + cells * CELL_BYTES as u64
}

/// Total blob length for a pyramid over `count` values.
#[must_use]
pub fn byte_len(count: u64, base_shift: u32) -> u64 {
    level_offset(count, base_shift, level_count(count, base_shift))
}

/// A pyramid blob's header — everything needed to address a cell without
/// reading one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyramidHeader {
    pub base_shift: u32,
    pub level_count: u32,
    pub source_count: u64,
}

impl PyramidHeader {
    #[must_use]
    pub fn new(source_count: u64, base_shift: u32) -> Self {
        Self {
            base_shift,
            level_count: level_count(source_count, base_shift),
            source_count,
        }
    }

    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[6] = self.base_shift as u8;
        out[7] = self.level_count as u8;
        out[8..16].copy_from_slice(&self.source_count.to_le_bytes());
        // 16..32 reserved, zeroed.
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(EngineError::corrupt(format!(
                "pyramid header is {} bytes, expected {HEADER_LEN}",
                bytes.len()
            )));
        }
        if bytes[0..4] != MAGIC {
            return Err(EngineError::corrupt("pyramid magic bytes do not match"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != FORMAT_VERSION {
            return Err(EngineError::corrupt(format!(
                "pyramid format version {version} is not supported"
            )));
        }
        let base_shift = u32::from(bytes[6]);
        let level_count = u32::from(bytes[7]);
        let source_count = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        if base_shift == 0 || base_shift >= 63 {
            return Err(EngineError::corrupt(format!(
                "pyramid base shift {base_shift} is out of range"
            )));
        }
        let expected = self::level_count(source_count, base_shift);
        if level_count != expected {
            return Err(EngineError::corrupt(format!(
                "pyramid claims {level_count} levels over {source_count} values, expected {expected}"
            )));
        }
        Ok(Self {
            base_shift,
            level_count,
            source_count,
        })
    }

    /// Cells at `level`, or `None` when the level is past the top.
    #[must_use]
    pub fn cells_at(&self, level: u32) -> Option<u64> {
        (level < self.level_count).then(|| cells_at(self.source_count, self.base_shift, level))
    }

    /// Source values one cell of `level` covers.
    #[must_use]
    pub fn span_at(&self, level: u32) -> u64 {
        span_at(self.base_shift, level)
    }

    /// Byte offset of `level`'s first cell.
    #[must_use]
    pub fn level_offset(&self, level: u32) -> u64 {
        level_offset(self.source_count, self.base_shift, level)
    }

    /// Byte offset and length of cells `[start, end)` of `level`, clamped to
    /// the level. `None` when the level or the range is empty.
    #[must_use]
    pub fn cell_bytes(&self, level: u32, start: u64, end: u64) -> Option<(u64, usize)> {
        let cells = self.cells_at(level)?;
        let start = start.min(cells);
        let end = end.min(cells);
        if end <= start {
            return None;
        }
        let offset = self.level_offset(level) + start * CELL_BYTES as u64;
        Some((offset, ((end - start) as usize) * CELL_BYTES))
    }

    /// Total bytes of the blob this header describes.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.level_offset(self.level_count)
    }

    /// The cells of `level` covering source values `[start, end)`.
    #[must_use]
    pub fn cells_covering(&self, level: u32, start: u64, end: u64) -> (u64, u64) {
        let span = self.span_at(level);
        let first = start / span;
        let last = end.div_ceil(span);
        (first, last.max(first))
    }
}

/// How a viewport should read a column at a given zoom (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reduction {
    /// Read the raw span: at this zoom a pixel covers fewer values than one
    /// level-0 cell, so the pyramid would be coarser than the screen.
    Raw,
    /// Read this pyramid level, where a cell covers one to two pixels.
    Level(u32),
}

/// Picks the level whose cells land within a pixel or two (§5.4).
///
/// Below one level-0 cell per pixel the answer is [`Reduction::Raw`]: the span
/// is then at most `64 × width` values, a bounded read of a few hundred
/// kilobytes, and drawing from it is what gives a zoomed-in view its real
/// sample shape.
#[must_use]
pub fn choose(samples_per_pixel: f64, header: &PyramidHeader) -> Reduction {
    if !samples_per_pixel.is_finite() || header.level_count == 0 {
        return Reduction::Raw;
    }
    let base = span_at(header.base_shift, 0) as f64;
    if samples_per_pixel < base {
        return Reduction::Raw;
    }
    // `2^(k + shift) <= spp < 2^(k + shift + 1)` puts one to two cells in a
    // pixel, so the level is the exponent less the base shift.
    let exponent = samples_per_pixel.log2().floor();
    let level = (exponent as i64 - i64::from(header.base_shift)).max(0) as u64;
    Reduction::Level(level.min(u64::from(header.level_count - 1)) as u32)
}

/// Decodes cells from a slice of a pyramid blob.
pub fn decode_cells(bytes: &[u8]) -> Result<Vec<MinMax>> {
    if bytes.len() % CELL_BYTES != 0 {
        return Err(EngineError::corrupt(format!(
            "pyramid slice of {} bytes is not a whole number of cells",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(CELL_BYTES)
        .map(|cell| {
            let min = f32::from_le_bytes(cell[0..4].try_into().expect("4 bytes"));
            let max = f32::from_le_bytes(cell[4..8].try_into().expect("4 bytes"));
            MinMax::new(min, max)
        })
        .collect())
}

/// Encodes one cell.
#[must_use]
pub fn encode_cell(cell: MinMax) -> [u8; CELL_BYTES] {
    let mut out = [0u8; CELL_BYTES];
    out[0..4].copy_from_slice(&cell.min.to_le_bytes());
    out[4..8].copy_from_slice(&cell.max.to_le_bytes());
    out
}

/// Builds a pyramid in one linear pass over the source values.
///
/// Values are pushed in order — from whatever chunking the caller reads with,
/// which does not affect the result — and only level 0 is accumulated as they
/// arrive. The levels above it are folded pairwise out of level 0 at
/// [`PyramidBuilder::finish`], which costs one more pass over a sixty-fourth
/// of the data and keeps the layout obviously right for a source whose last
/// cell is short.
#[derive(Debug)]
pub struct PyramidBuilder {
    base_shift: u32,
    /// Completed level-0 cells.
    base: Vec<MinMax>,
    /// The level-0 cell being accumulated.
    open: MinMax,
    /// Values folded into `open` so far.
    filled: u64,
    count: u64,
}

impl Default for PyramidBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PyramidBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_shift(BASE_SHIFT)
    }

    /// A builder whose level 0 covers `1 << base_shift` values. Only tests
    /// need anything but [`BASE_SHIFT`].
    #[must_use]
    pub fn with_base_shift(base_shift: u32) -> Self {
        debug_assert!(base_shift > 0 && base_shift < 63);
        Self {
            base_shift,
            base: Vec::new(),
            open: MinMax::EMPTY,
            filled: 0,
            count: 0,
        }
    }

    /// Folds one source value in. Non-finite values are ignored, so a gap in
    /// an imported column (§7.3) does not poison the cell around it.
    pub fn push(&mut self, value: f64) {
        self.open.include(value);
        self.filled += 1;
        self.count += 1;
        if self.filled == span_at(self.base_shift, 0) {
            self.close_cell();
        }
    }

    /// Folds a slice in.
    pub fn extend(&mut self, values: impl IntoIterator<Item = f64>) {
        for value in values {
            self.push(value);
        }
    }

    /// Source values pushed so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    fn close_cell(&mut self) {
        let cell = std::mem::replace(&mut self.open, MinMax::EMPTY);
        self.filled = 0;
        self.base.push(cell);
    }

    /// Closes the pyramid: flushes the partial level-0 cell, then folds each
    /// level into the one above until the top is a single cell.
    ///
    /// A level's cell `j` covers cells `2j` and `2j + 1` of the level below,
    /// so an odd last cell is carried up alone rather than dropped — which is
    /// what keeps the tail of a column that does not fill its last cell
    /// visible at every zoom.
    #[must_use]
    pub fn finish(mut self) -> Pyramid {
        if self.filled > 0 {
            self.close_cell();
        }
        let header = PyramidHeader::new(self.count, self.base_shift);
        let mut levels: Vec<Vec<MinMax>> = Vec::with_capacity(header.level_count as usize);
        if self.count > 0 {
            levels.push(std::mem::take(&mut self.base));
            while levels.last().map_or(0, Vec::len) > 1 {
                let below = levels.last().expect("just checked");
                let folded = below
                    .chunks(2)
                    .map(|pair| match pair {
                        [a, b] => a.merge(*b),
                        [a] => *a,
                        _ => unreachable!("chunks(2) yields one or two cells"),
                    })
                    .collect();
                levels.push(folded);
            }
        }
        debug_assert_eq!(levels.len(), header.level_count as usize);

        Pyramid { header, levels }
    }
}

/// A built pyramid, ready to encode into a blob.
#[derive(Debug, Clone, PartialEq)]
pub struct Pyramid {
    header: PyramidHeader,
    levels: Vec<Vec<MinMax>>,
}

impl Pyramid {
    /// Builds a pyramid over `values` in one pass.
    #[must_use]
    pub fn build(values: impl IntoIterator<Item = f64>) -> Self {
        let mut builder = PyramidBuilder::new();
        builder.extend(values);
        builder.finish()
    }

    #[must_use]
    pub fn header(&self) -> PyramidHeader {
        self.header
    }

    /// One level's cells, coarsening with `level`.
    #[must_use]
    pub fn level(&self, level: u32) -> Option<&[MinMax]> {
        self.levels.get(level as usize).map(Vec::as_slice)
    }

    /// The whole blob payload: header then every level, level 0 first.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.header.byte_len() as usize);
        out.extend_from_slice(&self.header.encode());
        for level in &self.levels {
            for cell in level {
                out.extend_from_slice(&encode_cell(*cell));
            }
        }
        out
    }

    /// Reads a whole pyramid back. The renderer never does this — it reads the
    /// slice of one level — but `Verify Library` and the tests do.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let header = PyramidHeader::decode(bytes)?;
        if bytes.len() as u64 != header.byte_len() {
            return Err(EngineError::corrupt(format!(
                "pyramid is {} bytes, its header describes {}",
                bytes.len(),
                header.byte_len()
            )));
        }
        let mut levels = Vec::with_capacity(header.level_count as usize);
        for level in 0..header.level_count {
            let start = header.level_offset(level) as usize;
            let cells = header.cells_at(level).unwrap_or(0) as usize;
            let end = start + cells * CELL_BYTES;
            levels.push(decode_cells(&bytes[start..end])?);
        }
        Ok(Self { header, levels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: u64) -> Vec<f64> {
        (0..n).map(|i| i as f64).collect()
    }

    /// Folds a level's cells by hand, the slow way.
    fn expected_cell(values: &[f64], span: usize, index: usize) -> MinMax {
        let start = index * span;
        let end = (start + span).min(values.len());
        let mut cell = MinMax::EMPTY;
        for value in &values[start..end] {
            cell.include(*value);
        }
        cell
    }

    #[test]
    fn layout_matches_the_design_ratios() {
        // 64:1 at level 0, halving upwards.
        assert_eq!(span_at(BASE_SHIFT, 0), 64);
        assert_eq!(span_at(BASE_SHIFT, 1), 128);
        assert_eq!(cells_at(6_400, BASE_SHIFT, 0), 100);
        assert_eq!(cells_at(6_400, BASE_SHIFT, 1), 50);
        // A short tail still gets a cell.
        assert_eq!(cells_at(65, BASE_SHIFT, 0), 2);
        assert_eq!(cells_at(0, BASE_SHIFT, 0), 0);
    }

    #[test]
    fn every_level_together_costs_a_few_percent_of_an_f32_column() {
        // Level 0 is one 8-byte cell per 64 values — 3.1% of an f32 column —
        // and the levels above it sum to one more of the same, so the whole
        // pyramid is ~6%.
        let count = 100_000_000u64;
        let source_bytes = (count * 4) as f64;
        let level0 = cells_at(count, BASE_SHIFT, 0) * CELL_BYTES as u64;
        assert!((level0 as f64 / source_bytes - 0.031).abs() < 0.001);

        let whole = byte_len(count, BASE_SHIFT) as f64 / source_bytes;
        assert!(
            (0.055..0.07).contains(&whole),
            "pyramid overhead was {whole}"
        );
    }

    #[test]
    fn the_top_level_is_a_single_cell() {
        for count in [1u64, 63, 64, 65, 1_000, 1_048_576, 99_999_999] {
            let levels = level_count(count, BASE_SHIFT);
            assert_eq!(
                cells_at(count, BASE_SHIFT, levels - 1),
                1,
                "count {count} topped out at {levels} levels"
            );
            assert!(cells_at(count, BASE_SHIFT, levels - 2.min(levels - 1)) >= 1);
        }
    }

    #[test]
    fn cells_hold_the_min_and_max_of_their_span() {
        let values: Vec<f64> = (0..1_000).map(|i| ((i as f64) * 0.37).sin()).collect();
        let pyramid = Pyramid::build(values.iter().copied());
        let level0 = pyramid.level(0).unwrap();
        assert_eq!(level0.len() as u64, cells_at(1_000, BASE_SHIFT, 0));
        for (index, cell) in level0.iter().enumerate() {
            assert_eq!(*cell, expected_cell(&values, 64, index), "cell {index}");
        }
        let level1 = pyramid.level(1).unwrap();
        for (index, cell) in level1.iter().enumerate() {
            assert_eq!(*cell, expected_cell(&values, 128, index), "cell {index}");
        }
    }

    #[test]
    fn a_partial_top_cell_still_covers_the_tail() {
        // 129 values: level 0 has three cells, the last holding one value, and
        // level 1 has two, the last carrying that odd cell up.
        let values = ramp(129);
        let pyramid = Pyramid::build(values.iter().copied());
        assert_eq!(pyramid.level(0).unwrap().len(), 3);
        assert_eq!(pyramid.level(1).unwrap().len(), 2);
        let top = pyramid.level(pyramid.header().level_count - 1).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0], MinMax::new(0.0, 128.0));
    }

    #[test]
    fn chunking_the_source_does_not_change_the_pyramid() {
        let values = ramp(5_000);
        let whole = Pyramid::build(values.iter().copied());
        for chunk in [1usize, 7, 64, 999, 4_096] {
            let mut builder = PyramidBuilder::new();
            for piece in values.chunks(chunk) {
                builder.extend(piece.iter().copied());
            }
            assert_eq!(builder.finish(), whole, "chunk size {chunk}");
        }
    }

    #[test]
    fn non_finite_values_do_not_poison_a_cell() {
        let mut values = ramp(64);
        values[10] = f64::NAN;
        values[11] = f64::INFINITY;
        let pyramid = Pyramid::build(values);
        assert_eq!(pyramid.level(0).unwrap()[0], MinMax::new(0.0, 63.0));
    }

    #[test]
    fn a_cell_of_only_gaps_is_empty_not_zero() {
        let pyramid = Pyramid::build(vec![f64::NAN; 64]);
        let cell = pyramid.level(0).unwrap()[0];
        assert!(cell.is_empty(), "{cell:?}");
    }

    #[test]
    fn encoding_round_trips() {
        for count in [0u64, 1, 64, 1_000, 70_000] {
            let pyramid = Pyramid::build(ramp(count));
            let bytes = pyramid.encode();
            assert_eq!(bytes.len() as u64, byte_len(count, BASE_SHIFT));
            assert_eq!(Pyramid::decode(&bytes).unwrap(), pyramid);
        }
    }

    #[test]
    fn a_slice_of_the_blob_decodes_to_the_same_cells() {
        let pyramid = Pyramid::build(ramp(10_000));
        let bytes = pyramid.encode();
        let header = pyramid.header();
        let (offset, len) = header.cell_bytes(1, 3, 9).unwrap();
        let cells = decode_cells(&bytes[offset as usize..offset as usize + len]).unwrap();
        assert_eq!(cells, pyramid.level(1).unwrap()[3..9]);
    }

    #[test]
    fn corrupt_bytes_are_refused() {
        let bytes = Pyramid::build(ramp(1_000)).encode();
        assert!(Pyramid::decode(&bytes[..HEADER_LEN - 1]).is_err());

        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        assert!(Pyramid::decode(&wrong_magic).is_err());

        let mut wrong_version = bytes.clone();
        wrong_version[4] = 9;
        assert!(Pyramid::decode(&wrong_version).is_err());

        // A truncated blob is not silently read as a shorter pyramid.
        assert!(Pyramid::decode(&bytes[..bytes.len() - CELL_BYTES]).is_err());
    }

    #[test]
    fn the_chosen_level_puts_one_to_two_cells_in_a_pixel() {
        let header = PyramidHeader::new(100_000_000, BASE_SHIFT);
        for spp in [64.0, 100.0, 1_024.0, 5_000.0, 1e6] {
            let Reduction::Level(level) = choose(spp, &header) else {
                panic!("{spp} samples per pixel should use a level");
            };
            let per_pixel = spp / header.span_at(level) as f64;
            assert!(
                (1.0..2.0).contains(&per_pixel),
                "{spp} samples per pixel chose level {level}, {per_pixel} cells per pixel"
            );
        }
    }

    #[test]
    fn a_zoomed_in_viewport_reads_raw_samples() {
        let header = PyramidHeader::new(1_000_000, BASE_SHIFT);
        assert_eq!(choose(0.25, &header), Reduction::Raw);
        assert_eq!(choose(63.9, &header), Reduction::Raw);
        assert_eq!(choose(64.0, &header), Reduction::Level(0));
    }

    #[test]
    fn a_viewport_wider_than_the_signal_clamps_to_the_top_level() {
        let header = PyramidHeader::new(10_000, BASE_SHIFT);
        let Reduction::Level(level) = choose(1e12, &header) else {
            panic!("a huge span should still use a level");
        };
        assert_eq!(level, header.level_count - 1);
    }

    #[test]
    fn an_empty_column_has_no_pyramid_to_choose_from() {
        let header = PyramidHeader::new(0, BASE_SHIFT);
        assert_eq!(header.level_count, 0);
        assert_eq!(choose(1_000.0, &header), Reduction::Raw);
        assert_eq!(header.cells_at(0), None);
    }

    #[test]
    fn cells_covering_a_span_include_its_ends() {
        let header = PyramidHeader::new(10_000, BASE_SHIFT);
        assert_eq!(header.cells_covering(0, 0, 64), (0, 1));
        assert_eq!(header.cells_covering(0, 63, 65), (0, 2));
        assert_eq!(header.cells_covering(0, 128, 128), (2, 2));
    }
}
