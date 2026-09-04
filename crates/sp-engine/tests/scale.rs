//! The G2 exit criterion for M4: a 100 M-sample signal plays at 60 fps.
//!
//! Ignored by default — it writes a ~400 MB library and is only meaningful in
//! a release build. Run it with:
//!
//! ```text
//! cargo test -p sp-engine --release --test scale -- --ignored --nocapture
//! ```
//!
//! What it measures is the per-frame cost the scope actually pays: reduce the
//! visible span to one `(min, max)` pair per pixel column at 1080p, once per
//! frame, at several zoom levels. Drawing is Iced's; everything before it is
//! this crate's, and it is what the budget in §13 (8 ms a frame) is spent on.

use std::time::{Duration, Instant};

use sp_core::{Domain, SampleBuffer, Samples, TimeRange, Timebase};
use sp_engine::reduce::{TraceDescriptor, TraceStyle};
use sp_engine::source::{self, ColumnSource};
use sp_engine::viewport::Amplitude;
use sp_engine::{reduce, Viewport};
use sp_store::{blob, Store, DEFAULT_CHUNK_SIZE};

/// The scale G2 names.
const COUNT: u64 = 100_000_000;
const RATE_HZ: f64 = 100_000_000.0;
/// Values written per transaction while filling the fixture.
const WRITE_CHUNK: u64 = 4_000_000;
/// Frames measured per zoom level.
const FRAMES: u32 = 60;
/// The per-frame budget from §13.
const FRAME_BUDGET: Duration = Duration::from_millis(8);

#[test]
#[ignore = "writes a ~400 MB library; run in release with --ignored"]
fn a_hundred_million_samples_reduce_inside_the_frame_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();

    // The column is written in pieces so the fixture itself never holds the
    // whole 100 M samples in memory either.
    let started = Instant::now();
    let blob_id = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let mut writer =
                blob::BlobWriter::begin(&tx, blob::BlobKind::Samples, DEFAULT_CHUNK_SIZE)?;
            let header = blob::ColumnHeader::new(
                sp_core::DType::F32,
                Timebase::regular(RATE_HZ, 0.0),
                COUNT,
                sp_core::Scaling::IDENTITY,
            );
            writer.write(&header.encode())?;
            let mut at = 0;
            while at < COUNT {
                let end = (at + WRITE_CHUNK).min(COUNT);
                let values: Vec<f32> = (at..end)
                    .map(|i| {
                        let t = i as f64 / RATE_HZ;
                        ((std::f64::consts::TAU * 1_000.0 * t).sin()
                            * (1.0 + 0.5 * (std::f64::consts::TAU * 3.0 * t).sin()))
                            as f32
                    })
                    .collect();
                let buffer = SampleBuffer::new(Samples::F32(values));
                writer.write(&blob::encode_samples(buffer.samples(), 0..buffer.len()))?;
                at = end;
            }
            let id = writer.finish()?;
            tx.commit()?;
            Ok(id)
        })
        .unwrap();
    println!("wrote {COUNT} samples in {:.1?}", started.elapsed());

    let started = Instant::now();
    let row = source::ensure(&store, blob_id).unwrap();
    let build = started.elapsed();
    println!("built {} pyramid levels in {build:.1?}", row.level_count);

    let descriptor = TraceDescriptor {
        timebase: Timebase::regular(RATE_HZ, 0.0),
        count: COUNT,
        domain: Domain::Analog,
    };
    let whole = COUNT as f64 / RATE_HZ;

    for (label, duration) in [
        ("whole signal", whole),
        ("a tenth", whole / 10.0),
        ("a thousandth", whole / 1_000.0),
        ("raw samples", 1e-6),
    ] {
        let viewport = Viewport::new(
            TimeRange::from_duration(whole / 4.0, duration),
            Amplitude::new(-2.0, 2.0),
            1_920.0,
        );
        let started = Instant::now();
        let mut reduction = None;
        store
            .read(|conn| {
                let column = ColumnSource::open(conn, blob_id).unwrap();
                for _ in 0..FRAMES {
                    let snapshot =
                        reduce::trace(&column, descriptor, &viewport, TraceStyle::default())
                            .unwrap();
                    reduction = Some(snapshot.reduction);
                }
                Ok(())
            })
            .unwrap();
        let per_frame = started.elapsed() / FRAMES;
        println!(
            "{label:>14}: {per_frame:>9.3?} a frame  ({:?})",
            reduction.unwrap()
        );
        assert!(
            per_frame < FRAME_BUDGET,
            "{label} took {per_frame:?} a frame, over the {FRAME_BUDGET:?} budget"
        );
    }
}
