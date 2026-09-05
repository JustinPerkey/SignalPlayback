//! Comparing two stage outputs on the same axes (`docs/DESIGN.md` §10.2,
//! §10.4).
//!
//! Pinning a stage on the results rail draws A and B overlaid with a residual
//! trace beneath. The residual is computed from what the two traces already
//! reduced to, not from a third pass over the samples.
//!
//! Zoomed in past one sample per pixel both sides are individual samples and
//! the residual is the exact per-sample difference. Zoomed out, each pixel
//! column is a `(min, max)` envelope, and the residual is the difference of
//! the two envelopes — `(minA − minB, maxA − maxB)`, which is the difference
//! between the traces *as drawn*. Two identical signals therefore residual to
//! zero at every zoom, and any sample that moves the min or the max of its
//! pixel column shows up immediately; a change that leaves both bounds of a
//! column untouched is invisible until the user zooms into it, which the
//! readout says by reporting whether the number is per-sample or per-envelope.
//!
//! The alternative — the interval `[minA − maxB, maxA − minB]` bounding every
//! difference the column could hold — is rejected because it is nonzero for
//! two identical signals, which makes the one question the comparison exists
//! to answer unanswerable at a glance.

use sp_core::stats::MinMax;

use crate::error::Result;
use crate::pyramid::Reduction;
use crate::reduce::{self, ColumnReader, TraceDescriptor, TraceForm, TraceGeometry, TraceSnapshot};
use crate::viewport::Viewport;

/// A pinned comparison over one viewport.
#[derive(Debug, Clone, PartialEq)]
pub struct Residual {
    /// The residual itself, drawn like any other trace.
    pub snapshot: TraceSnapshot,
    /// Whether the residual is the per-sample difference (true) or the
    /// difference of the two envelopes, one pair per pixel column (false).
    pub exact: bool,
    /// The largest absolute difference over the visible window — of samples
    /// when `exact`, of envelope bounds otherwise.
    pub max_abs: f64,
    /// The earliest visible time at which the two differ, when they do.
    pub first_divergence_s: Option<f64>,
}

impl Residual {
    /// One line for the comparison readout.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.max_abs == 0.0 {
            return "identical over this window".to_owned();
        }
        let bound = if self.exact {
            "max |A−B|"
        } else {
            "max envelope |A−B|"
        };
        match self.first_divergence_s {
            Some(t) => format!("{bound} {:.6}, first differs at {t:.6} s", self.max_abs),
            None => format!("{bound} {:.6}", self.max_abs),
        }
    }
}

/// Reduces both columns over `viewport` and returns A, B and their residual.
///
/// The two are reduced independently, so a pinned stage whose signal has a
/// different length, rate or alignment still compares — the residual is over
/// the pixel columns, which both sides share by construction.
pub fn compare(
    a: &dyn ColumnReader,
    a_descriptor: TraceDescriptor,
    b: &dyn ColumnReader,
    b_descriptor: TraceDescriptor,
    viewport: &Viewport,
    style: reduce::TraceStyle,
) -> Result<(TraceSnapshot, TraceSnapshot, Residual)> {
    let left = reduce::trace(a, a_descriptor, viewport, style)?;
    let right = reduce::trace(b, b_descriptor, viewport, style)?;
    let residual = residual(&left, &right, viewport);
    Ok((left, right, residual))
}

/// The residual of two already-reduced traces.
#[must_use]
pub fn residual(a: &TraceSnapshot, b: &TraceSnapshot, viewport: &Viewport) -> Residual {
    match (&a.geometry, &b.geometry) {
        (TraceGeometry::Points(left), TraceGeometry::Points(right)) => {
            exact_residual(left, right, a, b)
        }
        _ => bounded_residual(a, b, viewport),
    }
}

/// Both sides are individual samples: pair them by time and subtract.
///
/// Samples are paired within half the wider trace's sample spacing, so two
/// signals that were resampled or nudged still line up; a sample only one side
/// has is skipped rather than compared against nothing.
fn exact_residual(
    left: &[(f64, f64)],
    right: &[(f64, f64)],
    a: &TraceSnapshot,
    b: &TraceSnapshot,
) -> Residual {
    let tolerance = pairing_tolerance(left, right);
    let mut points = Vec::with_capacity(left.len().min(right.len()));
    let mut max_abs: f64 = 0.0;
    let mut first_divergence_s = None;

    let mut j = 0usize;
    for (t, value) in left {
        while j + 1 < right.len() && (right[j].0 - t).abs() > (right[j + 1].0 - t).abs() {
            j += 1;
        }
        let Some((other_t, other)) = right.get(j) else {
            break;
        };
        if (other_t - t).abs() > tolerance {
            continue;
        }
        let difference = value - other;
        if difference != 0.0 && first_divergence_s.is_none() {
            first_divergence_s = Some(*t);
        }
        max_abs = max_abs.max(difference.abs());
        points.push((*t, difference));
    }

    Residual {
        snapshot: TraceSnapshot {
            geometry: TraceGeometry::Points(points),
            form: TraceForm::Analog,
            reduction: Reduction::Raw,
            samples_per_pixel: a.samples_per_pixel.max(b.samples_per_pixel),
            note: None,
        },
        exact: true,
        max_abs,
        first_divergence_s,
    }
}

/// At least one side is an envelope per pixel column: the residual is the
/// difference of the two envelopes, bound by bound.
fn bounded_residual(a: &TraceSnapshot, b: &TraceSnapshot, viewport: &Viewport) -> Residual {
    let width = viewport.width_px().max(0.0) as u32;
    let (first_px, columns) = span_of(a, b, width);
    let mut cells = Vec::with_capacity(columns as usize);
    let mut max_abs: f64 = 0.0;
    let mut first_divergence_s = None;

    for column in 0..columns {
        let px = first_px + column;
        let left = cell_at(a, px);
        let right = cell_at(b, px);
        let cell = match (left, right) {
            (Some(left), Some(right)) if !left.is_empty() && !right.is_empty() => {
                let (low, high) = (left.min - right.min, left.max - right.max);
                MinMax::new(low.min(high), low.max(high))
            }
            // A column only one side reaches is a gap, not a residual of the
            // whole signal against zero: the other trace says nothing there.
            _ => MinMax::EMPTY,
        };
        if !cell.is_empty() {
            let extent = f64::from(cell.min.abs().max(cell.max.abs()));
            max_abs = max_abs.max(extent);
            if first_divergence_s.is_none() && (cell.min != 0.0 || cell.max != 0.0) {
                first_divergence_s = Some(viewport.time_at(px as f32));
            }
        }
        cells.push(cell);
    }

    Residual {
        snapshot: TraceSnapshot {
            geometry: TraceGeometry::Bars { first_px, cells },
            form: TraceForm::Analog,
            // The coarser of the two levels is what the bound was computed
            // at; saying otherwise would overstate its resolution.
            reduction: coarser(a.reduction, b.reduction),
            samples_per_pixel: a.samples_per_pixel.max(b.samples_per_pixel),
            note: None,
        },
        exact: false,
        max_abs,
        first_divergence_s,
    }
}

/// The coarser of two reductions — a raw read is the finest there is.
fn coarser(a: Reduction, b: Reduction) -> Reduction {
    match (a, b) {
        (Reduction::Level(x), Reduction::Level(y)) => Reduction::Level(x.max(y)),
        (Reduction::Level(level), Reduction::Raw) | (Reduction::Raw, Reduction::Level(level)) => {
            Reduction::Level(level)
        }
        (Reduction::Raw, Reduction::Raw) => Reduction::Raw,
    }
}

/// Half the wider sample spacing of the two point sets, which is how far
/// apart two samples may be and still be the same instant.
fn pairing_tolerance(left: &[(f64, f64)], right: &[(f64, f64)]) -> f64 {
    let spacing = |points: &[(f64, f64)]| match points {
        [first, second, ..] => (second.0 - first.0).abs(),
        _ => 0.0,
    };
    let widest = spacing(left).max(spacing(right));
    if widest > 0.0 {
        widest / 2.0
    } else {
        f64::INFINITY
    }
}

/// The pixel columns the pair between them covers, clipped to the viewport.
fn span_of(a: &TraceSnapshot, b: &TraceSnapshot, width: u32) -> (u32, u32) {
    let bounds = |snapshot: &TraceSnapshot| match &snapshot.geometry {
        TraceGeometry::Bars { first_px, cells } if !cells.is_empty() => {
            Some((*first_px, *first_px + cells.len() as u32))
        }
        _ => None,
    };
    match (bounds(a), bounds(b)) {
        (Some((a0, a1)), Some((b0, b1))) => {
            let start = a0.min(b0);
            (start, a1.max(b1).saturating_sub(start).min(width))
        }
        (Some((start, end)), None) | (None, Some((start, end))) => {
            (start, end.saturating_sub(start).min(width))
        }
        (None, None) => (0, 0),
    }
}

/// One trace's cell at a pixel column, whatever geometry it holds.
fn cell_at(snapshot: &TraceSnapshot, px: u32) -> Option<MinMax> {
    match &snapshot.geometry {
        TraceGeometry::Bars { first_px, cells } => {
            let offset = px.checked_sub(*first_px)? as usize;
            cells.get(offset).copied()
        }
        // Points against bars: the sample nearest the column, treated as a
        // degenerate interval. Rare — it needs one trace far denser than the
        // other — but it draws rather than vanishing.
        TraceGeometry::Points(points) => points.get(px as usize).map(|(_, value)| {
            let value = *value as f32;
            MinMax::new(value, value)
        }),
    }
}

#[cfg(test)]
mod tests {
    use sp_core::{Domain, TimeRange, Timebase};

    use super::*;
    use crate::pyramid::Pyramid;
    use crate::viewport::Amplitude;

    /// A column held in memory, with a pyramid over it.
    struct Column {
        values: Vec<f64>,
        pyramid: Pyramid,
    }

    impl Column {
        fn new(values: Vec<f64>) -> Self {
            let pyramid = Pyramid::build(values.iter().copied());
            Self { values, pyramid }
        }
    }

    impl ColumnReader for Column {
        fn values(&self, range: sp_core::SampleRange) -> Result<Vec<f64>> {
            let start = (range.start as usize).min(self.values.len());
            let end = (range.end as usize).min(self.values.len());
            Ok(self.values[start..end].to_vec())
        }

        fn pyramid(&self) -> Option<crate::pyramid::PyramidHeader> {
            Some(self.pyramid.header())
        }

        fn cells(&self, level: u32, start: u64, end: u64) -> Result<Vec<MinMax>> {
            let cells = self.pyramid.level(level).unwrap_or(&[]);
            let start = (start as usize).min(cells.len());
            let end = (end as usize).min(cells.len());
            Ok(cells[start..end].to_vec())
        }
    }

    fn descriptor(count: u64) -> TraceDescriptor {
        TraceDescriptor {
            timebase: Timebase::regular(1000.0, 0.0),
            count,
            domain: Domain::Analog,
        }
    }

    fn viewport(width: f32, range: TimeRange) -> Viewport {
        let mut viewport = Viewport::default();
        viewport.set_width(width);
        viewport.fit_time(range);
        viewport.amplitude = Amplitude::new(-2.0, 2.0);
        viewport
    }

    #[test]
    fn an_unchanged_signal_has_a_zero_residual() {
        let values: Vec<f64> = (0..4000).map(|i| (i as f64 / 50.0).sin()).collect();
        let a = Column::new(values.clone());
        let b = Column::new(values);
        let viewport = viewport(400.0, TimeRange::new(0.0, 4.0));
        let (_, _, residual) = compare(
            &a,
            descriptor(4000),
            &b,
            descriptor(4000),
            &viewport,
            reduce::TraceStyle::default(),
        )
        .unwrap();
        assert_eq!(residual.max_abs, 0.0);
        assert_eq!(residual.first_divergence_s, None);
        assert_eq!(residual.describe(), "identical over this window");
    }

    #[test]
    fn a_stage_that_changed_one_sample_is_located_to_its_pixel() {
        let values: Vec<f64> = vec![0.0; 4000];
        let mut changed = values.clone();
        changed[2000] = 1.0;
        let a = Column::new(values);
        let b = Column::new(changed);
        let viewport = viewport(400.0, TimeRange::new(0.0, 4.0));
        let (_, _, residual) = compare(
            &a,
            descriptor(4000),
            &b,
            descriptor(4000),
            &viewport,
            reduce::TraceStyle::default(),
        )
        .unwrap();
        assert!(!residual.exact, "a reduced window compares envelopes");
        assert!((residual.max_abs - 1.0).abs() < 1e-6);
        let at = residual.first_divergence_s.expect("it diverges");
        assert!((at - 2.0).abs() < 0.05, "diverges at {at}");
    }

    #[test]
    fn zoomed_past_one_sample_per_pixel_the_residual_is_exact() {
        let a = Column::new((0..64).map(|i| i as f64).collect());
        let b = Column::new((0..64).map(|i| i as f64 + 0.25).collect());
        // 40 samples across 400 pixels: ten pixels per sample.
        let viewport = viewport(400.0, TimeRange::new(0.0, 0.04));
        let (left, _, residual) = compare(
            &a,
            descriptor(64),
            &b,
            descriptor(64),
            &viewport,
            reduce::TraceStyle::default(),
        )
        .unwrap();
        assert!(matches!(left.geometry, TraceGeometry::Points(_)));
        assert!(residual.exact);
        assert!((residual.max_abs - 0.25).abs() < 1e-9);
        assert_eq!(residual.first_divergence_s, Some(0.0));
        let TraceGeometry::Points(points) = &residual.snapshot.geometry else {
            panic!("an exact residual is points");
        };
        assert!(points.iter().all(|(_, v)| (v + 0.25).abs() < 1e-9));
    }

    #[test]
    fn a_column_only_one_side_reaches_is_a_gap_rather_than_the_whole_signal() {
        // B is half as long, so the right half of the window has nothing to
        // compare against.
        let a = Column::new(vec![1.0; 4000]);
        let b = Column::new(vec![1.0; 2000]);
        let viewport = viewport(400.0, TimeRange::new(0.0, 4.0));
        let (_, _, residual) = compare(
            &a,
            descriptor(4000),
            &b,
            descriptor(2000),
            &viewport,
            reduce::TraceStyle::default(),
        )
        .unwrap();
        let TraceGeometry::Bars { cells, .. } = &residual.snapshot.geometry else {
            panic!("a reduced residual is bars");
        };
        assert!(cells.iter().take(150).all(|cell| !cell.is_empty()));
        assert!(cells.iter().skip(250).all(|cell| cell.is_empty()));
        assert_eq!(residual.max_abs, 0.0);
    }

    #[test]
    fn the_readout_says_whether_it_is_a_bound_or_a_measurement() {
        let bound = Residual {
            snapshot: TraceSnapshot {
                geometry: TraceGeometry::Points(Vec::new()),
                form: TraceForm::Analog,
                reduction: Reduction::Raw,
                samples_per_pixel: 0.0,
                note: None,
            },
            exact: false,
            max_abs: 0.5,
            first_divergence_s: Some(1.0),
        };
        assert!(bound.describe().starts_with("max envelope |A−B| 0.500000"));
        let exact = Residual {
            exact: true,
            ..bound
        };
        assert!(exact.describe().starts_with("max |A−B| 0.500000"));
    }
}
