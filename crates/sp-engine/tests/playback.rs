//! End-to-end playback tests: a signal in a library, a pyramid built over it,
//! and a viewport reduced from it (`docs/DESIGN.md` §5.4, §11, goal G2).

use std::f64::consts::TAU;

use proptest::prelude::*;
use sp_core::stats::MinMax;
use sp_core::{
    Attributes, DType, Domain, Provenance, SampleBuffer, Samples, SignalId, TimeRange, Timebase,
};
use sp_engine::pyramid::{cells_at, Pyramid, PyramidBuilder, Reduction, BASE_SHIFT};
use sp_engine::reduce::{TraceDescriptor, TraceGeometry, TraceStyle};
use sp_engine::source::{self, ColumnSource};
use sp_engine::viewport::Amplitude;
use sp_engine::{reduce, LoopMode, Transport, Viewport};
use sp_store::{library, trains, NewDataset, NewGroup, NewSignal, NewTrain, Store};

/// Samples the fixture signal holds. Big enough that several pyramid levels
/// exist and the reduction has to choose between them; small enough that the
/// test suite stays quick.
const COUNT: u64 = 1_000_000;
const RATE_HZ: f64 = 1_000_000.0;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    signal: SignalId,
    values: Vec<f64>,
}

fn fixture(domain: Domain) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();

    // A tone with a slow envelope: min/max cells differ level to level, so a
    // wrong level shows up as a visibly wrong trace.
    let values: Vec<f64> = (0..COUNT)
        .map(|i| {
            let t = i as f64 / RATE_HZ;
            (TAU * 1_000.0 * t).sin() * (1.0 + 0.5 * (TAU * 3.0 * t).sin())
        })
        .collect();

    let buffer = SampleBuffer::new(Samples::F64(values.clone()));
    let signal = store
        .write(move |conn| {
            let tx = conn.transaction()?;
            let dataset = library::insert_dataset(
                &tx,
                &NewDataset::new("fixture", sp_core::SourceKind::Generated),
            )?;
            let train = trains::insert_train(&tx, &NewTrain::new(dataset, 0))?;
            let group = library::insert_group(&tx, &NewGroup::new(train, 0, 1))?;
            let signal = library::insert_signal(
                &tx,
                &NewSignal {
                    group_id: group,
                    ordinal: 0,
                    name: "tone".to_owned(),
                    units: None,
                    domain,
                    provenance: Provenance::Generated,
                    timebase: Timebase::regular(RATE_HZ, 0.0),
                    samples: buffer,
                    attributes: Attributes::new(),
                    gen_spec: None,
                },
            )?;
            tx.commit()?;
            Ok(signal)
        })
        .unwrap();

    Fixture {
        _dir: dir,
        store,
        signal,
        values,
    }
}

impl Fixture {
    fn blob(&self) -> sp_store::BlobId {
        let id = self.signal;
        self.store
            .read(move |conn| library::signal_blob(conn, id))
            .unwrap()
    }

    /// Reduces the whole signal over `viewport`, the way a frame does.
    fn reduce(&self, viewport: &Viewport, domain: Domain) -> sp_engine::TraceSnapshot {
        let blob = self.blob();
        self.store
            .read(move |conn| {
                let column = ColumnSource::open(conn, blob).unwrap();
                Ok(reduce::trace(
                    &column,
                    TraceDescriptor {
                        timebase: Timebase::regular(RATE_HZ, 0.0),
                        count: COUNT,
                        domain,
                    },
                    viewport,
                    TraceStyle::default(),
                )
                .unwrap())
            })
            .unwrap()
    }
}

fn viewport(range: TimeRange, width: f32) -> Viewport {
    Viewport::new(range, Amplitude::new(-2.0, 2.0), width)
}

/// The whole signal, at 1080p width.
fn full_view() -> Viewport {
    viewport(TimeRange::new(0.0, COUNT as f64 / RATE_HZ), 1_920.0)
}

#[test]
fn a_frame_costs_one_cell_per_pixel_however_long_the_signal_is() {
    let fixture = fixture(Domain::Analog);
    source::ensure(&fixture.store, fixture.blob()).unwrap();

    let view = full_view();
    let snapshot = fixture.reduce(&view, Domain::Analog);
    let TraceGeometry::Bars { cells, .. } = &snapshot.geometry else {
        panic!("a whole-signal view draws bars");
    };

    // The entire million-sample signal arrives as one screen's worth of cells.
    assert!(cells.len() <= view.width_px() as usize + 1);
    assert!(matches!(snapshot.reduction, Reduction::Level(_)));
    // Which is a few tens of kilobytes, not megabytes (G2).
    let bytes = cells.len() * size_of::<MinMax>();
    assert!(bytes < 64 * 1024, "a frame moved {bytes} bytes");
}

#[test]
fn the_reduction_covers_every_sample_it_stands_for() {
    let fixture = fixture(Domain::Analog);
    source::ensure(&fixture.store, fixture.blob()).unwrap();
    let snapshot = fixture.reduce(&full_view(), Domain::Analog);

    let extent = snapshot.geometry.extent();
    let (min, max) = fixture
        .values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    // Peaks survive decimation: that is the point of a min/max pyramid.
    assert!(
        (f64::from(extent.min) - min).abs() < 1e-3,
        "{extent:?} vs {min}"
    );
    assert!(
        (f64::from(extent.max) - max).abs() < 1e-3,
        "{extent:?} vs {max}"
    );
}

#[test]
fn zooming_in_walks_down_the_levels_and_ends_at_raw_samples() {
    let fixture = fixture(Domain::Analog);
    source::ensure(&fixture.store, fixture.blob()).unwrap();

    // At 1 MHz over 1 920 pixels, these windows are 520, 260, 130 and 65
    // samples per pixel — one level apart each time.
    let mut previous = u32::MAX;
    for duration in [1.0, 0.5, 0.25, 0.125] {
        let snapshot = fixture.reduce(
            &viewport(TimeRange::new(0.0, duration), 1_920.0),
            Domain::Analog,
        );
        let Reduction::Level(level) = snapshot.reduction else {
            panic!("{duration} s should still come from the pyramid");
        };
        assert!(level < previous, "{duration} s chose level {level}");
        previous = level;
    }

    // Below one level-0 cell per pixel the samples themselves are drawn.
    let close = fixture.reduce(
        &viewport(TimeRange::new(0.0, 0.000_1), 1_920.0),
        Domain::Analog,
    );
    assert_eq!(close.reduction, Reduction::Raw);
    assert!(matches!(close.geometry, TraceGeometry::Points(_)));
}

#[test]
fn a_pyramid_leaves_the_library_verifiable() {
    let fixture = fixture(Domain::Analog);
    source::ensure(&fixture.store, fixture.blob()).unwrap();

    let report = fixture.store.read(sp_store::verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.blobs_checked, 2, "the column and its pyramid");

    // Dropping every pyramid is always safe, and leaves the library clean.
    let dropped = fixture
        .store
        .write(|conn| sp_store::pyramid::clear_all(conn))
        .unwrap();
    assert_eq!(dropped, 1);
    let report = fixture.store.read(sp_store::verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");

    // And the signal still draws, from raw samples.
    let snapshot = fixture.reduce(&full_view(), Domain::Analog);
    assert_eq!(snapshot.reduction, Reduction::Raw);
    assert!(!snapshot.geometry.is_empty());
}

#[test]
fn a_domain_picks_its_renderer_all_the_way_through() {
    for (domain, form) in [
        (Domain::Analog, sp_engine::TraceForm::Analog),
        (Domain::DigitalLogic, sp_engine::TraceForm::Logic),
        (Domain::BasebandIq, sp_engine::TraceForm::Iq),
        (Domain::Symbols, sp_engine::TraceForm::Stems),
    ] {
        let fixture = fixture(domain);
        source::ensure(&fixture.store, fixture.blob()).unwrap();
        assert_eq!(fixture.reduce(&full_view(), domain).form, form);
    }
}

#[test]
fn a_signal_and_its_pyramid_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");

    let blob = {
        let store = Store::open(&path).unwrap();
        let buffer =
            SampleBuffer::from_f64(DType::F32, &(0..10_000).map(f64::from).collect::<Vec<_>>());
        let blob = store
            .write(move |conn| {
                let tx = conn.transaction()?;
                let id = sp_store::blob::write_column(
                    &tx,
                    &buffer,
                    Timebase::regular(1_000.0, 0.0),
                    sp_store::DEFAULT_CHUNK_SIZE,
                )?;
                tx.commit()?;
                Ok(id)
            })
            .unwrap();
        source::ensure(&store, blob).unwrap();
        blob
    };

    let store = Store::open(&path).unwrap();
    store
        .read(move |conn| {
            let column = ColumnSource::open(conn, blob).unwrap();
            assert!(column.has_pyramid());
            let header = column.pyramid_header().unwrap();
            assert_eq!(header.source_count, 10_000);
            assert_eq!(header.base_shift, BASE_SHIFT);
            Ok(())
        })
        .unwrap();
}

proptest! {
    /// Every cell is exactly the min and max of the span it stands for. This
    /// is what makes a decimated trace trustworthy: no peak is ever lost.
    #[test]
    fn cells_bound_their_span(
        values in proptest::collection::vec(-1e6f64..1e6, 1..3_000),
    ) {
        let pyramid = Pyramid::build(values.iter().copied());
        let header = pyramid.header();
        for level in 0..header.level_count {
            let span = header.span_at(level) as usize;
            let cells = pyramid.level(level).expect("level exists");
            prop_assert_eq!(cells.len() as u64, cells_at(values.len() as u64, BASE_SHIFT, level));
            for (index, cell) in cells.iter().enumerate() {
                let start = index * span;
                let end = (start + span).min(values.len());
                let mut expected = MinMax::EMPTY;
                for value in &values[start..end] {
                    expected.include(*value);
                }
                prop_assert_eq!(*cell, expected);
            }
        }
    }

    /// Chunking the source — which is how a 100 M-sample column is read — does
    /// not change the pyramid.
    #[test]
    fn the_build_is_independent_of_chunking(
        values in proptest::collection::vec(-1.0f64..1.0, 1..2_000),
        chunk in 1usize..500,
    ) {
        let whole = Pyramid::build(values.iter().copied());
        let mut builder = PyramidBuilder::new();
        for piece in values.chunks(chunk) {
            builder.extend(piece.iter().copied());
        }
        prop_assert_eq!(builder.finish(), whole);
    }

    /// However long it plays and whatever the loop policy, the playhead stays
    /// inside the loop range (§11.1).
    #[test]
    fn the_playhead_never_leaves_the_range(
        steps in proptest::collection::vec(0.0f64..3.0, 1..40),
        rate in -20.0f64..20.0,
        mode in 0usize..3,
    ) {
        prop_assume!(rate.abs() > 0.001);
        let mut transport = Transport::new(TimeRange::new(-1.0, 4.0));
        transport.set_loop_mode(LoopMode::ALL[mode]);
        transport.set_rate(rate);
        transport.play();
        for step in steps {
            transport.advance(step);
            prop_assert!(transport.playhead_s() >= -1.0);
            prop_assert!(transport.playhead_s() <= 4.0);
            prop_assert!((0.0..=1.0).contains(&transport.progress()));
        }
    }

    /// A reduction never returns more cells than the canvas has columns, at
    /// any zoom — the budget the frame time depends on (§13).
    #[test]
    fn a_reduction_is_bounded_by_the_canvas_width(
        start in 0.0f64..1.0,
        duration in 0.000_01f64..1.0,
        width in 64u32..2_000,
    ) {
        use sp_engine::reduce::ColumnReader;

        /// The fixture column, in memory: the reduction's bound has nothing to
        /// do with where the samples are stored.
        struct Column {
            values: Vec<f64>,
            pyramid: Pyramid,
        }

        impl ColumnReader for Column {
            fn values(&self, range: sp_core::SampleRange) -> sp_engine::Result<Vec<f64>> {
                let start = (range.start as usize).min(self.values.len());
                let end = (range.end as usize).min(self.values.len());
                Ok(self.values[start..end].to_vec())
            }

            fn pyramid(&self) -> Option<sp_engine::PyramidHeader> {
                Some(self.pyramid.header())
            }

            fn cells(&self, level: u32, start: u64, end: u64) -> sp_engine::Result<Vec<MinMax>> {
                let cells = self.pyramid.level(level).unwrap_or(&[]);
                let start = (start as usize).min(cells.len());
                let end = (end as usize).min(cells.len());
                Ok(cells[start..end].to_vec())
            }
        }

        let values: Vec<f64> = (0..100_000).map(|i| (i as f64 * 0.01).sin()).collect();
        let column = Column {
            pyramid: Pyramid::build(values.iter().copied()),
            values,
        };

        let view = viewport(TimeRange::from_duration(start, duration), width as f32);
        let snapshot = reduce::trace(
            &column,
            TraceDescriptor {
                timebase: Timebase::regular(100_000.0, 0.0),
                count: 100_000,
                domain: Domain::Analog,
            },
            &view,
            TraceStyle::default(),
        )
        .unwrap();

        let drawn = match &snapshot.geometry {
            TraceGeometry::Bars { cells, .. } => cells.len(),
            TraceGeometry::Points(points) => points.len(),
        };
        prop_assert!(
            drawn <= width as usize + 2,
            "{drawn} primitives for {width} pixels"
        );
    }
}
