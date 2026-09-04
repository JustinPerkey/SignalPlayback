//! The M5 exit criterion: a pipeline runs over a dataset group by group, and
//! every stage's output is persisted (`docs/DESIGN.md` §16).
//!
//! These tests go through the whole stack — registry, scheduler, run tables,
//! blob store — with the built-in stages, so they are also the check that
//! `sp-proc` and `sp-dsp` agree on the contract between them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sp_core::run::{Disposition, RunStatus, StageStatus};
use sp_core::time::SampleRange;
use sp_core::{DType, DatasetId, GroupId, PipelineId, RunId, SampleBuffer, SourceKind, Timebase};
use sp_dsp::artifacts::{Detections, Statistics};
use sp_proc::pipeline::{Pipeline, PipelineStage};
use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions, RunProgress, RunSummary};
use sp_proc::StageRegistry;
use sp_store::library::{NewDataset, NewGroup, NewSignal};
use sp_store::runs::{self, SOURCE_STAGE};
use sp_store::trains::NewTrain;
use sp_store::{library, trains, Store};

/// A library holding one dataset of three groups, each with two signals.
struct Library {
    _dir: tempfile::TempDir,
    store: Store,
    dataset: DatasetId,
    groups: Vec<GroupId>,
}

/// A ramp with an offset, so detrending has something to remove.
///
/// The slope differs per group on purpose: two groups whose samples reduce to
/// the same thing share a cache key, which is correct but would make the
/// stage counts below depend on which group finished first.
fn ramp(offset: f64, slope: f64, len: usize) -> SampleBuffer {
    let values: Vec<f64> = (0..len).map(|i| offset + i as f64 * slope).collect();
    SampleBuffer::from_f64(DType::F64, &values)
}

/// Rates differ per group so a stage can fail on one group alone.
fn rate_of(group_ordinal: u32) -> f64 {
    if group_ordinal == 2 {
        100.0
    } else {
        1000.0
    }
}

fn library() -> Library {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    let (dataset, groups) = store
        .write(|conn| {
            let dataset =
                library::insert_dataset(conn, &NewDataset::new("runs", SourceKind::Generated))?;
            let train = trains::insert_train(conn, &NewTrain::new(dataset, 0).named("capture"))?;
            let mut groups = Vec::new();
            for ordinal in 0..3u32 {
                let group = library::insert_group(
                    conn,
                    &NewGroup::new(train, ordinal, 0).named(format!("dwell {ordinal}")),
                )?;
                let timebase = Timebase::regular(rate_of(ordinal), 0.0);
                library::insert_signal(
                    conn,
                    &NewSignal::new(
                        group,
                        0,
                        "rf",
                        timebase,
                        ramp(f64::from(ordinal), 0.5 + f64::from(ordinal), 16),
                    ),
                )?;
                library::insert_signal(
                    conn,
                    &NewSignal::new(
                        group,
                        1,
                        "ref",
                        timebase,
                        ramp(10.0, 0.25 + f64::from(ordinal), 16),
                    ),
                )?;
                groups.push(group);
            }
            Ok((dataset, groups))
        })
        .unwrap();
    Library {
        _dir: dir,
        store,
        dataset,
        groups,
    }
}

/// Saves a pipeline and returns its id, as the pipeline editor would.
fn save(store: &Store, pipeline: &Pipeline) -> PipelineId {
    let name = pipeline.name.clone();
    let rows = pipeline.to_rows();
    store
        .write(move |conn| {
            let id = runs::insert_pipeline(conn, &runs::NewPipeline::new(name))?;
            runs::set_pipeline_stages(conn, id, &rows)?;
            Ok(id)
        })
        .unwrap()
}

/// Passthrough → detrend → gain ×2 → statistics → threshold.
fn conditioning() -> Pipeline {
    Pipeline::new("conditioning")
        .with_stage(PipelineStage::new("dsp.util.passthrough").labelled("Raw"))
        .with_stage(PipelineStage::new("dsp.condition.detrend"))
        .with_stage(PipelineStage::new("dsp.condition.gain").with_param("gain", 2.0))
        .with_stage(PipelineStage::new("dsp.measure.statistics").with_param("write_back", true))
        .with_stage(PipelineStage::new("dsp.detect.threshold").with_param("level", 2.0))
}

fn run(
    lib: &Library,
    registry: &StageRegistry,
    pipeline: &Pipeline,
    options: &RunOptions,
) -> RunSummary {
    let id = save(&lib.store, pipeline);
    run_saved(lib, registry, pipeline, id, options, &RunControl::new())
}

fn run_saved(
    lib: &Library,
    registry: &StageRegistry,
    pipeline: &Pipeline,
    id: PipelineId,
    options: &RunOptions,
    control: &RunControl,
) -> RunSummary {
    run_pipeline(
        &lib.store,
        registry,
        id,
        pipeline,
        &lib.groups,
        options,
        control,
    )
    .expect("the run itself should not fail")
}

fn samples(lib: &Library, run: RunId, group: GroupId, stage: i32, ordinal: usize) -> Vec<f64> {
    let rows = lib
        .store
        .read(|conn| runs::stage_signals(conn, run, group, stage))
        .unwrap();
    let row = &rows[ordinal];
    lib.store
        .read(|conn| runs::read_signal_samples(conn, row, SampleRange::first(row.sample_count)))
        .unwrap()
        .values()
        .collect()
}

#[test]
fn a_pipeline_runs_group_by_group_and_records_every_stage() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();
    let summary = run(
        &lib,
        &registry,
        &pipeline,
        &RunOptions::default().over_dataset(lib.dataset),
    );

    assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());
    assert_eq!(summary.groups_ok, 3);
    assert_eq!(summary.groups_failed, 0);
    assert_eq!(summary.stages_run, 3 * 5, "five stages over three groups");

    let header = lib
        .store
        .read(|conn| runs::get_run(conn, summary.run))
        .unwrap();
    assert_eq!(header.status, RunStatus::Ok);
    assert_eq!(header.dataset_id, Some(lib.dataset));
    assert!(header.finished_utc.is_some());
    assert_eq!(header.pipeline_hash, pipeline.hash(&registry).unwrap());

    let groups = lib
        .store
        .read(|conn| runs::run_groups(conn, summary.run))
        .unwrap();
    assert_eq!(groups.len(), 3);
    assert!(groups.iter().all(|g| g.status == RunStatus::Ok));

    for &group in &lib.groups {
        // The source is recorded like any other stage's output, so "before" is
        // a real row (§9.6).
        let source = lib
            .store
            .read(|conn| runs::stage_signals(conn, summary.run, group, SOURCE_STAGE))
            .unwrap();
        assert_eq!(source.len(), 2);
        assert!(source
            .iter()
            .all(|s| s.disposition == Disposition::Passthrough));
        assert!(source.iter().all(|s| s.blob_id.is_some()));

        let stages = lib
            .store
            .read(|conn| runs::group_stages(conn, summary.run, group))
            .unwrap();
        assert_eq!(stages.len(), 5);
        assert!(stages.iter().all(|s| s.status == StageStatus::Ok));
        assert!(stages.iter().all(|s| !s.cache_key.is_empty()));

        for stage_ordinal in 0..5 {
            let signals = lib
                .store
                .read(|conn| runs::stage_signals(conn, summary.run, group, stage_ordinal))
                .unwrap();
            assert_eq!(
                signals.len(),
                2,
                "stage {stage_ordinal} should record both signals"
            );
            assert!(
                signals
                    .iter()
                    .all(|s| s.blob_id.is_some() && s.sample_count == 16),
                "stage {stage_ordinal} should persist its samples"
            );
        }
    }
}

#[test]
fn each_stage_records_what_it_actually_computed() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let summary = run(&lib, &registry, &conditioning(), &RunOptions::default());
    let group = lib.groups[0];

    // A ramp 0, 0.5, … detrends to a symmetric ramp about zero, and the gain
    // stage doubles it.
    let detrended = samples(&lib, summary.run, group, 1, 0);
    let gained = samples(&lib, summary.run, group, 2, 0);
    assert_eq!(detrended[0], -3.75);
    for (before, after) in detrended.iter().zip(&gained) {
        assert!((after - before * 2.0).abs() < 1e-12);
    }

    // The passthrough shares the source blob rather than copying it (§5.3).
    let source = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, group, SOURCE_STAGE))
        .unwrap();
    let passed = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, group, 0))
        .unwrap();
    assert_eq!(passed[0].blob_id, source[0].blob_id);
    assert_eq!(passed[0].disposition, Disposition::Passthrough);
    assert_eq!(
        lib.store
            .read(|conn| runs::stage_signals(conn, summary.run, group, 2))
            .unwrap()[0]
            .disposition,
        Disposition::Replaced
    );
}

#[test]
fn artifacts_metrics_and_write_backs_all_land() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let summary = run(&lib, &registry, &conditioning(), &RunOptions::default());
    let group = lib.groups[0];

    let stats_artifacts = lib
        .store
        .read(|conn| runs::stage_artifacts(conn, summary.run, Some(group), 3))
        .unwrap();
    assert_eq!(stats_artifacts.len(), 1);
    assert_eq!(stats_artifacts[0].kind, "statistics.v1");
    let payload = lib
        .store
        .read(|conn| runs::artifact_payload(conn, &stats_artifacts[0]))
        .unwrap();
    let statistics: Statistics = serde_json::from_str(&payload).unwrap();
    assert_eq!(statistics.names, ["rf", "ref"]);

    let detections_artifacts = lib
        .store
        .read(|conn| runs::stage_artifacts(conn, summary.run, Some(group), 4))
        .unwrap();
    let payload = lib
        .store
        .read(|conn| runs::artifact_payload(conn, &detections_artifacts[0]))
        .unwrap();
    let detections: Detections = serde_json::from_str(&payload).unwrap();
    assert!(!detections.is_empty(), "the ramp crosses the threshold");
    assert!(detections_artifacts[0].summary.is_some());

    let stages = lib
        .store
        .read(|conn| runs::group_stages(conn, summary.run, group))
        .unwrap();
    assert!(
        stages[3].metrics.contains_key("rf.rms"),
        "{:?}",
        stages[3].metrics
    );
    assert_eq!(
        stages[4].metrics.get("detections"),
        Some(&(detections.len() as f64))
    );

    // `write_back` puts the measurement onto the recorded signal, so it
    // travels with the result rather than only with the run.
    let measured = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, group, 3))
        .unwrap();
    assert!(measured[0].attributes.get_f64("rms").is_some());
}

#[test]
fn re_running_an_unchanged_pipeline_hits_the_cache_for_every_stage() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();
    let id = save(&lib.store, &pipeline);

    let first = run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default(),
        &RunControl::new(),
    );
    let second = run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default(),
        &RunControl::new(),
    );

    assert_eq!(second.stages_run, 0);
    assert_eq!(second.stages_cached, 3 * 5);
    assert_eq!(second.status, RunStatus::Ok);

    // A cache hit is indistinguishable from a stage that ran, apart from the
    // status: the same rows are there to read (§9.5).
    let group = lib.groups[0];
    assert_eq!(
        samples(&lib, second.run, group, 2, 0),
        samples(&lib, first.run, group, 2, 0)
    );
    let stages = lib
        .store
        .read(|conn| runs::group_stages(conn, second.run, group))
        .unwrap();
    assert!(stages.iter().all(|s| s.status == StageStatus::Cached));
    let artifacts = lib
        .store
        .read(|conn| runs::stage_artifacts(conn, second.run, Some(group), 4))
        .unwrap();
    assert_eq!(artifacts.len(), 1, "artifacts are copied forward too");
    assert!(
        stages[3].metrics.contains_key("rf.rms"),
        "a cached stage carries the metrics it was recorded with"
    );
}

#[test]
fn editing_a_stage_re_runs_it_and_everything_after_it() {
    // The point of content-addressed caching: stage 4's parameters change,
    // stages 1–3 are reused (§9.5).
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();
    let id = save(&lib.store, &pipeline);
    run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default(),
        &RunControl::new(),
    );

    let mut edited = conditioning();
    edited.stages[2] = PipelineStage::new("dsp.condition.gain").with_param("gain", 3.0);
    let summary = run_saved(
        &lib,
        &registry,
        &edited,
        id,
        &RunOptions::default(),
        &RunControl::new(),
    );

    // Stages 0 and 1 are unchanged and hit; 2, 3 and 4 re-run.
    assert_eq!(summary.stages_cached, 3 * 2);
    assert_eq!(summary.stages_run, 3 * 3);

    let stages = lib
        .store
        .read(|conn| runs::group_stages(conn, summary.run, lib.groups[0]))
        .unwrap();
    let statuses: Vec<_> = stages.iter().map(|s| s.status).collect();
    assert_eq!(
        statuses,
        [
            StageStatus::Cached,
            StageStatus::Cached,
            StageStatus::Ok,
            StageStatus::Ok,
            StageStatus::Ok
        ]
    );

    let gained = samples(&lib, summary.run, lib.groups[0], 2, 0);
    assert_eq!(gained[0], -3.75 * 3.0);
}

#[test]
fn running_without_the_cache_re_runs_everything() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();
    let id = save(&lib.store, &pipeline);
    run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default(),
        &RunControl::new(),
    );
    let again = run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default().without_cache(),
        &RunControl::new(),
    );
    assert_eq!(again.stages_cached, 0);
    assert_eq!(again.stages_run, 3 * 5);
}

#[test]
fn one_bad_group_fails_on_its_own_without_sinking_the_run() {
    // Group 2 is sampled at 100 Hz, so a 200 Hz corner is above its Nyquist
    // frequency and the filter refuses it (§9.1).
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = Pipeline::new("filtered")
        .with_stage(PipelineStage::new("dsp.condition.detrend"))
        .with_stage(
            PipelineStage::new("dsp.filter.biquad")
                .with_param("cutoff_hz", 200.0)
                .with_param("sections", 2),
        )
        .with_stage(PipelineStage::new("dsp.measure.statistics"));

    let summary = run(&lib, &registry, &pipeline, &RunOptions::default());
    assert_eq!(summary.status, RunStatus::Failed);
    assert_eq!(summary.groups_ok, 2);
    assert_eq!(summary.groups_failed, 1);

    let failed = lib
        .store
        .read(|conn| runs::group_stages(conn, summary.run, lib.groups[2]))
        .unwrap();
    assert_eq!(failed[0].status, StageStatus::Ok, "the detrend still ran");
    assert_eq!(failed[1].status, StageStatus::Failed);
    assert!(failed[1].message.as_ref().unwrap().contains("Nyquist"));
    assert_eq!(failed.len(), 2, "the stage after it never ran");

    // Everything the failed group did get through is still on record.
    let recorded = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, lib.groups[2], 0))
        .unwrap();
    assert_eq!(recorded.len(), 2);

    let healthy = lib
        .store
        .read(|conn| runs::group_stages(conn, summary.run, lib.groups[0]))
        .unwrap();
    assert_eq!(healthy.len(), 3);
    assert!(healthy.iter().all(|s| s.status == StageStatus::Ok));
}

#[test]
fn a_cancelled_run_is_recorded_as_cancelled() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();
    let id = save(&lib.store, &pipeline);

    let cancel = Arc::new(AtomicBool::new(true));
    let summary = run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default(),
        &RunControl::new().with_cancel(cancel.clone()),
    );

    assert_eq!(summary.status, RunStatus::Cancelled);
    assert_eq!(summary.stages_run, 0);
    let header = lib
        .store
        .read(|conn| runs::get_run(conn, summary.run))
        .unwrap();
    assert_eq!(header.status, RunStatus::Cancelled);
    assert!(cancel.load(Ordering::Relaxed));
}

#[test]
fn progress_is_reported_for_every_group() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();
    let id = save(&lib.store, &pipeline);

    let seen: Arc<Mutex<Vec<RunProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let control = RunControl::new().with_progress(Arc::new(move |progress| {
        sink.lock().unwrap().push(progress);
    }));

    let summary = run_saved(
        &lib,
        &registry,
        &pipeline,
        id,
        &RunOptions::default(),
        &control,
    );
    let seen = seen.lock().unwrap();
    assert_eq!(summary.status, RunStatus::Ok);
    // Five stages plus one completion report, per group.
    assert_eq!(seen.len(), 3 * 6);
    assert!(seen.iter().all(|p| p.groups_total == 3));
    assert_eq!(
        seen.iter().filter(|p| p.groups_done == 3).count(),
        1,
        "the last group reports the run complete once"
    );
}

#[test]
fn a_run_can_be_narrowed_to_one_group_at_a_time() {
    // `in_flight_cap` bounds concurrency; the results must not depend on it.
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = conditioning();

    let serial = run(
        &lib,
        &registry,
        &pipeline,
        &RunOptions::default().in_flight(1).without_cache(),
    );
    let parallel = run(
        &lib,
        &registry,
        &pipeline,
        &RunOptions::default().in_flight(4).without_cache(),
    );

    assert_eq!(serial.status, RunStatus::Ok);
    assert_eq!(parallel.status, RunStatus::Ok);
    for &group in &lib.groups {
        assert_eq!(
            samples(&lib, serial.run, group, 2, 0),
            samples(&lib, parallel.run, group, 2, 0)
        );
    }
}

#[test]
fn deleting_a_run_leaves_the_library_as_it_was() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let before = lib.store.read(library::summary).unwrap();

    let summary = run(&lib, &registry, &conditioning(), &RunOptions::default());
    let during = lib.store.read(library::summary).unwrap();
    assert!(during.blobs > before.blobs, "a run writes new columns");

    lib.store
        .write(move |conn| runs::delete_run(conn, summary.run))
        .unwrap();
    let after = lib.store.read(library::summary).unwrap();
    assert_eq!(
        after.blobs, before.blobs,
        "every blob the run added is released"
    );
    assert_eq!(after.signals, before.signals);
}
