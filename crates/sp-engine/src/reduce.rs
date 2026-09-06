//! Turning a stored column into the handful of kilobytes a frame draws
//! (`docs/DESIGN.md` §5.4, §11.4).
//!
//! The UI never sees raw samples. It asks for a [`TraceSnapshot`] over the
//! current viewport and gets back at most one `(min, max)` pair per pixel
//! column — or, when a pixel covers less than a sample, the samples
//! themselves. Draw cost is therefore proportional to viewport width, not to
//! signal length, which is the whole of G2.

use serde::{Deserialize, Serialize};
use sp_core::stats::MinMax;
use sp_core::{Domain, SampleRange, Timebase};

use crate::error::Result;
use crate::pyramid::{self, PyramidHeader, Reduction};
use crate::viewport::Viewport;

/// What the reducer needs from one stored column.
///
/// A trait rather than a connection so the reduction is testable in memory;
/// [`crate::source::ColumnSource`] is the implementation that reads a library.
pub trait ColumnReader {
    /// Scaled values for `range`, in engineering units.
    fn values(&self, range: SampleRange) -> Result<Vec<f64>>;

    /// The column's pyramid, when one is built. `None` forces the raw path,
    /// which is correct but reads more.
    fn pyramid(&self) -> Option<PyramidHeader>;

    /// Cells `[start, end)` of `level`.
    fn cells(&self, level: u32, start: u64, end: u64) -> Result<Vec<MinMax>>;
}

/// How much the reducer is asked to read for a frame (§13, Settings).
///
/// The automatic choice puts one to two pyramid cells in a pixel. This shifts
/// that by a level either way: `Fast` reads half as many cells, which is what
/// a remote or a battery-powered machine wants, and `Fine` reads twice as
/// many, which resolves a spike a coarser level would have averaged into its
/// neighbour. It never promotes a level to a raw read — that is a bound on
/// how much a frame may cost, not a quality setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// One level coarser: fewer cells per frame.
    Fast,
    #[default]
    Balanced,
    /// One level finer: more cells per frame, more detail per pixel.
    Fine,
}

impl Quality {
    pub const ALL: [Self; 3] = [Self::Fast, Self::Balanced, Self::Fine];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Fine => "Fine",
        }
    }

    /// Levels to shift the automatic choice by.
    #[must_use]
    pub const fn level_bias(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 0,
            Self::Fine => -1,
        }
    }

    /// Applies the bias to a chosen reduction, staying inside the pyramid and
    /// never turning a level into an unbounded raw read.
    #[must_use]
    pub fn apply(self, reduction: Reduction, level_count: u32) -> Reduction {
        match reduction {
            Reduction::Raw => Reduction::Raw,
            Reduction::Level(level) => {
                let top = level_count.saturating_sub(1);
                let biased = i64::from(level) + i64::from(self.level_bias());
                Reduction::Level(biased.clamp(0, i64::from(top)) as u32)
            }
        }
    }
}

/// Per-trace display transform (§11.4). Colour and visibility are the UI's
/// business; gain and offset change what the geometry *is*, so they are
/// applied here, once, rather than per drawn primitive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceStyle {
    pub gain: f64,
    pub offset_v: f64,
    /// Shifts this trace along the timeline for manual alignment — the
    /// playlist's `t_offset_s`.
    pub t_offset_s: f64,
    /// How hard the reducer works for this frame.
    pub quality: Quality,
}

impl Default for TraceStyle {
    fn default() -> Self {
        Self {
            gain: 1.0,
            offset_v: 0.0,
            t_offset_s: 0.0,
            quality: Quality::Balanced,
        }
    }
}

impl TraceStyle {
    /// Applies gain and offset to a value.
    #[must_use]
    pub fn apply(&self, value: f64) -> f64 {
        value * self.gain + self.offset_v
    }

    /// Applies gain and offset to a cell. A negative gain turns the pair over,
    /// so the bounds are re-ordered rather than left crossed.
    #[must_use]
    pub fn apply_cell(&self, cell: MinMax) -> MinMax {
        if cell.is_empty() {
            return cell;
        }
        let a = self.apply(f64::from(cell.min)) as f32;
        let b = self.apply(f64::from(cell.max)) as f32;
        MinMax::new(a.min(b), a.max(b))
    }
}

/// How a domain is drawn (§11.4). The renderer is a property of the signal,
/// not of the user's choice, which is what makes a bitstream look like a
/// bitstream the moment it is added to the scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceForm {
    /// Min/max bars, or a polyline when zoomed past one sample per pixel.
    Analog,
    /// Logic lanes: a high band, a low band and the transitions between.
    Logic,
    /// Magnitude envelope, with I/Q available once zoomed in.
    Iq,
    /// Labelled stems, one per symbol.
    Stems,
}

impl TraceForm {
    #[must_use]
    pub const fn for_domain(domain: Domain) -> Self {
        match domain {
            Domain::Analog => Self::Analog,
            Domain::DigitalLogic | Domain::Bits => Self::Logic,
            Domain::BasebandIq => Self::Iq,
            Domain::Symbols => Self::Stems,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Analog => "Analog",
            Self::Logic => "Logic",
            Self::Iq => "I/Q",
            Self::Stems => "Stems",
        }
    }
}

/// Where a logic cell sits relative to the slicing threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicLevel {
    Low,
    High,
    /// The cell holds values on both sides: an edge lands inside this pixel.
    Transition,
    /// The cell is a gap.
    Unknown,
}

impl LogicLevel {
    #[must_use]
    pub fn of(cell: MinMax, threshold: f64) -> Self {
        if cell.is_empty() {
            return Self::Unknown;
        }
        let threshold = threshold as f32;
        match (cell.min >= threshold, cell.max < threshold) {
            (true, _) => Self::High,
            (_, true) => Self::Low,
            _ => Self::Transition,
        }
    }
}

/// Geometry for one trace over one viewport.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceGeometry {
    /// One cell per pixel column, the first at `first_px`. An empty cell is a
    /// gap in the trace — no samples, or only non-finite ones (§7.3).
    Bars { first_px: u32, cells: Vec<MinMax> },
    /// Individual `(time_s, value)` samples, for a viewport zoomed past one
    /// sample per pixel. Drawn as a polyline with point markers.
    Points(Vec<(f64, f64)>),
}

impl TraceGeometry {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Bars { cells, .. } => cells.is_empty(),
            Self::Points(points) => points.is_empty(),
        }
    }

    /// The vertical extent of what was reduced, for an amplitude auto-fit.
    #[must_use]
    pub fn extent(&self) -> MinMax {
        match self {
            Self::Bars { cells, .. } => cells.iter().fold(MinMax::EMPTY, |acc, c| acc.merge(*c)),
            Self::Points(points) => points.iter().fold(MinMax::EMPTY, |mut acc, (_, v)| {
                acc.include(*v);
                acc
            }),
        }
    }
}

/// One trace's geometry plus what it cost to produce.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceSnapshot {
    pub geometry: TraceGeometry,
    pub form: TraceForm,
    /// Which level of the pyramid this came from, or that it read raw.
    pub reduction: Reduction,
    pub samples_per_pixel: f64,
    /// Why the trace is empty, when it is for a reason worth showing.
    pub note: Option<&'static str>,
}

impl TraceSnapshot {
    fn nothing(form: TraceForm, note: Option<&'static str>) -> Self {
        Self {
            geometry: TraceGeometry::Bars {
                first_px: 0,
                cells: Vec::new(),
            },
            form,
            reduction: Reduction::Raw,
            samples_per_pixel: 0.0,
            note,
        }
    }
}

/// What a column is, for the reducer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceDescriptor {
    pub timebase: Timebase,
    pub count: u64,
    pub domain: Domain,
}

/// Reduces one column over `viewport` to what a frame draws.
///
/// The read is bounded by the viewport width: at most one pyramid slice of
/// roughly two cells per pixel, or — below one level-0 cell per pixel — the
/// raw span, which is then at most `64 × width` samples.
pub fn trace(
    reader: &dyn ColumnReader,
    descriptor: TraceDescriptor,
    viewport: &Viewport,
    style: TraceStyle,
) -> Result<TraceSnapshot> {
    let form = TraceForm::for_domain(descriptor.domain);
    let Some(rate) = descriptor.timebase.sample_rate_hz else {
        return Ok(TraceSnapshot::nothing(
            form,
            Some("irregular timebase: this signal's times come from a time column"),
        ));
    };
    if descriptor.count == 0 {
        return Ok(TraceSnapshot::nothing(form, None));
    }

    // The trace's own timeline, with the alignment offset folded in, so a
    // nudged trace reduces exactly like an unnudged one.
    let timebase = Timebase::regular(rate, descriptor.timebase.t0_s + style.t_offset_s);
    let Some(range) = viewport.sample_range(timebase, descriptor.count) else {
        return Ok(TraceSnapshot::nothing(form, None));
    };
    if range.is_empty() {
        return Ok(TraceSnapshot::nothing(form, None));
    }

    let samples_per_pixel = viewport.samples_per_pixel(rate);
    let header = reader.pyramid();
    let reduction = match header.as_ref() {
        Some(header) => style.quality.apply(
            pyramid::choose(samples_per_pixel, header),
            header.level_count,
        ),
        None => Reduction::Raw,
    };

    // The pixel columns the visible samples land in.
    let first_px = viewport
        .x_of(timebase.time_of(range.start).unwrap_or(0.0))
        .floor()
        .clamp(0.0, viewport.width_px()) as u32;
    let last_px = viewport
        .x_of(timebase.time_of(range.end).unwrap_or(0.0))
        .ceil()
        .clamp(0.0, viewport.width_px()) as u32;
    if last_px <= first_px {
        return Ok(TraceSnapshot::nothing(form, None));
    }

    // Below one sample per pixel there is nothing to reduce: the samples
    // themselves are fewer than the columns, and their shape is the point.
    if samples_per_pixel < 1.0 {
        let values = reader.values(range)?;
        let points = values
            .iter()
            .enumerate()
            .filter(|(_, value)| value.is_finite())
            .map(|(offset, value)| {
                let index = range.start + offset as u64;
                (
                    timebase.time_of(index).unwrap_or(f64::NAN),
                    style.apply(*value),
                )
            })
            .collect();
        return Ok(TraceSnapshot {
            geometry: TraceGeometry::Points(points),
            form,
            reduction: Reduction::Raw,
            samples_per_pixel,
            note: None,
        });
    }

    let columns = (last_px - first_px) as usize;
    let mut cells = Vec::with_capacity(columns);

    match reduction {
        Reduction::Raw => {
            let values = reader.values(range)?;
            for column in 0..columns {
                let span = column_samples(viewport, timebase, first_px + column as u32, range);
                let mut cell = MinMax::EMPTY;
                for index in span.start..span.end {
                    if let Some(value) = values.get((index - range.start) as usize) {
                        cell.include(*value);
                    }
                }
                cells.push(style.apply_cell(cell));
            }
        }
        Reduction::Level(level) => {
            let header = header.as_ref().expect("a level implies a pyramid");
            let (first_cell, last_cell) = header.cells_covering(level, range.start, range.end);
            let read = reader.cells(level, first_cell, last_cell)?;
            let cell_span = header.span_at(level);
            for column in 0..columns {
                let span = column_samples(viewport, timebase, first_px + column as u32, range);
                let mut cell = MinMax::EMPTY;
                if !span.is_empty() {
                    let from = span.start / cell_span;
                    let to = span.end.div_ceil(cell_span).max(from + 1);
                    for index in from..to {
                        if let Some(source) = index
                            .checked_sub(first_cell)
                            .and_then(|offset| read.get(offset as usize))
                        {
                            cell = cell.merge(*source);
                        }
                    }
                }
                cells.push(style.apply_cell(cell));
            }
        }
    }

    Ok(TraceSnapshot {
        geometry: TraceGeometry::Bars { first_px, cells },
        form,
        reduction,
        samples_per_pixel,
        note: None,
    })
}

/// The samples one pixel column covers, clipped to what is visible.
fn column_samples(
    viewport: &Viewport,
    timebase: Timebase,
    column: u32,
    visible: SampleRange,
) -> SampleRange {
    let rate = timebase.sample_rate_hz.unwrap_or(1.0);
    let left = viewport.time_at(column as f32);
    let right = viewport.time_at(column as f32 + 1.0);
    let start = ((left - timebase.t0_s) * rate).ceil().max(0.0) as u64;
    let end = ((right - timebase.t0_s) * rate).ceil().max(0.0) as u64;
    // A column narrower than one sample still reads the sample under it, so a
    // trace never breaks into stripes at the raw/level boundary.
    SampleRange::new(start, end.max(start + 1)).intersect(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pyramid::Pyramid;
    use sp_core::TimeRange;

    use crate::viewport::Amplitude;

    /// A column held in memory, with a pyramid built over it.
    #[derive(Debug)]
    struct Column {
        values: Vec<f64>,
        pyramid: Option<Pyramid>,
    }

    impl Column {
        fn new(values: Vec<f64>) -> Self {
            let pyramid = Pyramid::build(values.iter().copied());
            Self {
                values,
                pyramid: Some(pyramid),
            }
        }

        fn without_pyramid(values: Vec<f64>) -> Self {
            Self {
                values,
                pyramid: None,
            }
        }
    }

    impl ColumnReader for Column {
        fn values(&self, range: SampleRange) -> Result<Vec<f64>> {
            let start = (range.start as usize).min(self.values.len());
            let end = (range.end as usize).min(self.values.len());
            Ok(self.values[start..end].to_vec())
        }

        fn pyramid(&self) -> Option<PyramidHeader> {
            self.pyramid.as_ref().map(Pyramid::header)
        }

        fn cells(&self, level: u32, start: u64, end: u64) -> Result<Vec<MinMax>> {
            let pyramid = self.pyramid.as_ref().expect("no pyramid built");
            let cells = pyramid.level(level).unwrap_or(&[]);
            let start = (start as usize).min(cells.len());
            let end = (end as usize).min(cells.len());
            Ok(cells[start..end].to_vec())
        }
    }

    fn descriptor(count: u64, rate: f64) -> TraceDescriptor {
        TraceDescriptor {
            timebase: Timebase::regular(rate, 0.0),
            count,
            domain: Domain::Analog,
        }
    }

    fn viewport(start: f64, end: f64, width: f32) -> Viewport {
        Viewport::new(TimeRange::new(start, end), Amplitude::UNIT, width)
    }

    fn ramp(n: u64) -> Vec<f64> {
        (0..n).map(|i| i as f64).collect()
    }

    #[test]
    fn a_long_column_reduces_to_one_cell_per_pixel() {
        let column = Column::new(ramp(1_000_000));
        // A 1 s window over a 1 MHz signal: 1 000 samples per pixel.
        let view = viewport(0.0, 1.0, 1_000.0);
        let snapshot = trace(
            &column,
            descriptor(1_000_000, 1_000_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();

        assert!(matches!(snapshot.reduction, Reduction::Level(_)));
        let TraceGeometry::Bars { first_px, cells } = &snapshot.geometry else {
            panic!("a reduced trace draws bars");
        };
        assert_eq!(*first_px, 0);
        assert_eq!(cells.len(), 1_000);
        // The ramp is monotonic, so each column's bounds are its own span.
        assert!((f64::from(cells[0].min) - 0.0).abs() < 1.0);
        assert!((f64::from(cells[999].max) - 999_999.0).abs() < 2_000.0);
        assert!(cells.windows(2).all(|w| w[0].max <= w[1].max));
    }

    #[test]
    fn decimation_quality_shifts_the_level_the_frame_reads() {
        let column = Column::new(ramp(1_000_000));
        let view = viewport(0.0, 1.0, 1_000.0);
        let reduce_at = |quality| {
            trace(
                &column,
                descriptor(1_000_000, 1_000_000.0),
                &view,
                TraceStyle {
                    quality,
                    ..TraceStyle::default()
                },
            )
            .unwrap()
            .reduction
        };

        let (Reduction::Level(fast), Reduction::Level(balanced), Reduction::Level(fine)) = (
            reduce_at(Quality::Fast),
            reduce_at(Quality::Balanced),
            reduce_at(Quality::Fine),
        ) else {
            panic!("a million samples over a thousand pixels reduces through the pyramid");
        };
        assert_eq!(fast, balanced + 1);
        assert_eq!(fine, balanced - 1);
    }

    #[test]
    fn quality_never_promotes_a_level_to_a_raw_read() {
        // At level 0 there is nothing finer inside the pyramid, and a raw read
        // at this zoom is what the bounded-read rule exists to prevent.
        assert_eq!(
            Quality::Fine.apply(Reduction::Level(0), 8),
            Reduction::Level(0)
        );
        assert_eq!(Quality::Fine.apply(Reduction::Raw, 8), Reduction::Raw);
        // Nor past the top of the pyramid.
        assert_eq!(
            Quality::Fast.apply(Reduction::Level(7), 8),
            Reduction::Level(7)
        );
    }

    #[test]
    fn the_pyramid_and_the_raw_path_agree() {
        // 800 samples per pixel, so the level path is used; compare against the
        // same viewport reduced without a pyramid.
        let values: Vec<f64> = (0..800_000).map(|i| ((i as f64) * 0.001).sin()).collect();
        let view = viewport(0.0, 1.0, 1_000.0);
        let with = trace(
            &Column::new(values.clone()),
            descriptor(800_000, 800_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        let without = trace(
            &Column::without_pyramid(values),
            descriptor(800_000, 800_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();

        assert!(matches!(with.reduction, Reduction::Level(_)));
        assert_eq!(without.reduction, Reduction::Raw);
        let (TraceGeometry::Bars { cells: a, .. }, TraceGeometry::Bars { cells: b, .. }) =
            (&with.geometry, &without.geometry)
        else {
            panic!("both reduce to bars");
        };
        assert_eq!(a.len(), b.len());
        for (index, (level, raw)) in a.iter().zip(b).enumerate() {
            // The pyramid over-covers by up to a cell, so it may be wider —
            // never narrower, which would drop a peak the user must see.
            assert!(
                level.min <= raw.min + 1e-6 && level.max >= raw.max - 1e-6,
                "column {index}: pyramid {level:?} does not cover raw {raw:?}"
            );
        }
    }

    #[test]
    fn zooming_past_one_sample_per_pixel_returns_the_samples() {
        let column = Column::new(ramp(1_000));
        // 100 samples over 1 000 pixels.
        let view = viewport(0.0, 0.1, 1_000.0);
        let snapshot = trace(
            &column,
            descriptor(1_000, 1_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        let TraceGeometry::Points(points) = &snapshot.geometry else {
            panic!("a zoomed-in trace draws points");
        };
        assert!(snapshot.samples_per_pixel < 1.0);
        assert!((100..=103).contains(&points.len()), "{}", points.len());
        assert!((points[0].0 - 0.0).abs() < 1e-12);
        assert!((points[1].1 - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_window_off_the_end_of_the_signal_draws_nothing() {
        let column = Column::new(ramp(1_000));
        let view = viewport(50.0, 51.0, 1_000.0);
        let snapshot = trace(
            &column,
            descriptor(1_000, 1_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        assert!(snapshot.geometry.is_empty());
    }

    #[test]
    fn a_signal_narrower_than_the_window_starts_at_its_own_pixel() {
        let column = Column::new(ramp(100_000));
        // The signal runs 0–1 s; the window is 0–4 s, so it occupies the first
        // quarter of the canvas.
        let view = viewport(0.0, 4.0, 1_000.0);
        let snapshot = trace(
            &column,
            descriptor(100_000, 100_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        let TraceGeometry::Bars { first_px, cells } = &snapshot.geometry else {
            panic!("bars");
        };
        assert_eq!(*first_px, 0);
        assert!((249..=252).contains(&cells.len()), "{}", cells.len());
    }

    #[test]
    fn a_trace_offset_in_time_shifts_its_pixels() {
        let column = Column::new(ramp(100_000));
        let view = viewport(0.0, 4.0, 1_000.0);
        let style = TraceStyle {
            t_offset_s: 2.0,
            ..TraceStyle::default()
        };
        let snapshot = trace(&column, descriptor(100_000, 100_000.0), &view, style).unwrap();
        let TraceGeometry::Bars { first_px, .. } = &snapshot.geometry else {
            panic!("bars");
        };
        assert_eq!(*first_px, 500);
    }

    #[test]
    fn gain_and_offset_are_applied_once_to_the_cells() {
        let column = Column::new(vec![1.0; 10_000]);
        let view = viewport(0.0, 1.0, 100.0);
        let style = TraceStyle {
            gain: -2.0,
            offset_v: 1.0,
            ..TraceStyle::default()
        };
        let snapshot = trace(&column, descriptor(10_000, 10_000.0), &view, style).unwrap();
        let TraceGeometry::Bars { cells, .. } = &snapshot.geometry else {
            panic!("bars");
        };
        // 1.0 × −2 + 1 = −1, and a negative gain must not leave min above max.
        assert!(cells.iter().all(|c| (c.min + 1.0).abs() < 1e-6));
        assert!(cells.iter().all(|c| c.min <= c.max));
    }

    #[test]
    fn gaps_stay_gaps() {
        let mut values = vec![0.0; 10_000];
        values[5_000..6_000].fill(f64::NAN);
        let column = Column::new(values);
        let view = viewport(0.0, 1.0, 100.0);
        let snapshot = trace(
            &column,
            descriptor(10_000, 10_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        let TraceGeometry::Bars { cells, .. } = &snapshot.geometry else {
            panic!("bars");
        };
        assert!(cells[55].is_empty(), "the gap is drawn as a gap");
        assert!(!cells[0].is_empty());
    }

    #[test]
    fn an_irregular_signal_says_why_it_is_not_drawn() {
        let column = Column::new(ramp(1_000));
        let view = viewport(0.0, 1.0, 1_000.0);
        let snapshot = trace(
            &column,
            TraceDescriptor {
                timebase: Timebase::irregular(0.0),
                count: 1_000,
                domain: Domain::Analog,
            },
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        assert!(snapshot.geometry.is_empty());
        assert!(snapshot.note.is_some());
    }

    #[test]
    fn an_empty_column_reduces_to_nothing() {
        let column = Column::new(Vec::new());
        let view = viewport(0.0, 1.0, 1_000.0);
        let snapshot = trace(
            &column,
            descriptor(0, 1_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        assert!(snapshot.geometry.is_empty());
        assert!(snapshot.note.is_none());
    }

    #[test]
    fn domains_pick_their_renderer() {
        assert_eq!(TraceForm::for_domain(Domain::Analog), TraceForm::Analog);
        assert_eq!(
            TraceForm::for_domain(Domain::DigitalLogic),
            TraceForm::Logic
        );
        assert_eq!(TraceForm::for_domain(Domain::Bits), TraceForm::Logic);
        assert_eq!(TraceForm::for_domain(Domain::BasebandIq), TraceForm::Iq);
        assert_eq!(TraceForm::for_domain(Domain::Symbols), TraceForm::Stems);
    }

    #[test]
    fn logic_levels_split_at_the_threshold() {
        assert_eq!(LogicLevel::of(MinMax::new(0.9, 1.0), 0.5), LogicLevel::High);
        assert_eq!(LogicLevel::of(MinMax::new(0.0, 0.1), 0.5), LogicLevel::Low);
        assert_eq!(
            LogicLevel::of(MinMax::new(0.0, 1.0), 0.5),
            LogicLevel::Transition
        );
        assert_eq!(LogicLevel::of(MinMax::EMPTY, 0.5), LogicLevel::Unknown);
    }

    #[test]
    fn the_extent_of_a_snapshot_bounds_its_values() {
        let column = Column::new(ramp(10_000));
        let view = viewport(0.0, 1.0, 100.0);
        let snapshot = trace(
            &column,
            descriptor(10_000, 10_000.0),
            &view,
            TraceStyle::default(),
        )
        .unwrap();
        let extent = snapshot.geometry.extent();
        assert_eq!(extent.min, 0.0);
        assert_eq!(extent.max, 9_999.0);
    }
}
