//! The M7 exit criterion: a pipeline runs headless and fails on a baseline
//! deviation (`docs/DESIGN.md` §16, G8).
//!
//! These tests go through the whole regression stack — assertions evaluated
//! per group, a run promoted to a baseline, a changed algorithm compared back
//! against it — with the built-in stages, so they are the check that §9.7 and
//! §10.4 hold end to end rather than only in unit tests.

use sp_core::run::{AssertStatus, RunStatus};
use sp_core::Tolerances;
use sp_core::{DType, DatasetId, GroupId, PipelineId, RunId, SampleBuffer, SourceKind, Timebase};
use sp_proc::compare::{self, DiffOptions};
use sp_proc::pipeline::{Pipeline, PipelineStage};
use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions, RunSummary};
use sp_proc::StageRegistry;
use sp_store::library::{NewDataset, NewGroup, NewSignal};
use sp_store::regress;
use sp_store::runs::{self, SOURCE_STAGE};
use sp_store::trains::NewTrain;
use sp_store::{library, trains, Store};

/// A library of three groups, each one ramp of a different slope.
struct Library {
    _dir: tempfile::TempDir,
    store: Store,
    dataset: DatasetId,
    groups: Vec<GroupId>,
}

fn ramp(offset: f64, slope: f64, len: usize) -> SampleBuffer {
    let values: Vec<f64> = (0..len).map(|i| offset + i as f64 * slope).collect();
    SampleBuffer::from_f64(DType::F64, &values)
}

fn library() -> Library {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    let (dataset, groups) = store
        .write(|conn| {
            let dataset =
                library::insert_dataset(conn, &NewDataset::new("regress", SourceKind::Generated))?;
            let train = trains::insert_train(conn, &NewTrain::new(dataset, 0).named("capture"))?;
            let mut groups = Vec::new();
            for ordinal in 0..3u32 {
                let group = library::insert_group(
                    conn,
                    &NewGroup::new(train, ordinal, 0).named(format!("dwell {ordinal}")),
                )?;
                library::insert_signal(
                    conn,
                    &NewSignal::new(
                        group,
                        0,
                        "rf",
                        Timebase::regular(1000.0, 0.0),
                        ramp(f64::from(ordinal), 0.5 + f64::from(ordinal), 16),
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

/// Gain ×2 then statistics, so every group records `rf.rms` and publishes a
/// `statistics` artifact.
fn measured(gain: f64) -> Pipeline {
    Pipeline::new("measured")
        .with_stage(PipelineStage::new("dsp.condition.gain").with_param("gain", gain))
        .with_stage(PipelineStage::new("dsp.measure.statistics"))
}

fn save(lib: &Library, pipeline: &Pipeline) -> PipelineId {
    let name = pipeline.name.clone();
    let stages = pipeline.to_rows();
    let assertions = pipeline.to_assertion_rows();
    lib.store
        .write(move |conn| {
            let id = runs::insert_pipeline(conn, &runs::NewPipeline::new(name))?;
            runs::set_pipeline_stages(conn, id, &stages)?;
            regress::set_pipeline_assertions(conn, id, &assertions)?;
            Ok(id)
        })
        .unwrap()
}

fn run(lib: &Library, registry: &StageRegistry, pipeline: &Pipeline) -> RunSummary {
    run_with(lib, registry, pipeline, &RunOptions::default())
}

fn run_with(
    lib: &Library,
    registry: &StageRegistry,
    pipeline: &Pipeline,
    options: &RunOptions,
) -> RunSummary {
    let id = save(lib, pipeline);
    let options = RunOptions {
        dataset_id: Some(lib.dataset),
        // Each run must recompute: a cache hit copies the earlier run's rows,
        // which is exactly what a regression test must not be fooled by.
        use_cache: false,
        ..options.clone()
    };
    run_pipeline(
        &lib.store,
        registry,
        id,
        pipeline,
        &lib.groups,
        &options,
        &RunControl::new(),
    )
    .expect("the run itself should not fail")
}

fn promote(lib: &Library, name: &str, run: RunId, tolerances: Tolerances) {
    let name = name.to_owned();
    lib.store
        .write(move |conn| regress::promote(conn, &name, run, &tolerances).map(|_| ()))
        .unwrap();
}

#[test]
fn a_run_with_passing_assertions_succeeds_and_records_every_outcome() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    // Group 0 is the ramp 0, 0.5, … ×2, so its RMS is a known number; the
    // other groups are steeper, so a shared bound has to be generous.
    let pipeline = measured(2.0)
        .asserting("metrics.rf.rms > 0")
        .asserting("signals[rf].samples == 16")
        .asserting("statistics.count == 1")
        .asserting("stage[0].status == ok");

    let summary = run(&lib, &registry, &pipeline);
    assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());
    assert_eq!(summary.assertions_failed, 0);
    assert_eq!(summary.assertions_passed, 3 * 4, "four over three groups");
    assert!(summary.describe().contains("12 assertion(s) passed"));

    let recorded = lib
        .store
        .read(move |conn| regress::run_assertions(conn, summary.run))
        .unwrap();
    assert_eq!(recorded.len(), 12);
    assert!(recorded.iter().all(|row| row.status == AssertStatus::Pass));
    assert_eq!(recorded[0].expression, "metrics.rf.rms > 0");
}

#[test]
fn a_failing_assertion_fails_its_group_and_the_run() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    // Group 0's doubled ramp has an RMS near 8.5; the steeper groups are far
    // above it, so this bound fails on two of the three.
    let pipeline = measured(2.0).asserting("metrics.rf.rms < 12");

    let summary = run(&lib, &registry, &pipeline);
    assert_eq!(summary.status, RunStatus::Failed);
    assert_eq!(summary.groups_ok, 1);
    assert_eq!(summary.groups_failed, 2);
    assert_eq!(summary.assertions_failed, 2);

    let failed: Vec<_> = lib
        .store
        .read(move |conn| regress::run_assertions(conn, summary.run))
        .unwrap()
        .into_iter()
        .filter(|row| row.status == AssertStatus::Fail)
        .collect();
    assert_eq!(failed.len(), 2);
    assert!(failed[0].message.as_ref().unwrap().contains("is not <"));
    assert!(failed[0].actual.unwrap() > 12.0);
    assert_eq!(failed[0].expected, Some(12.0));

    // The group row carries the first failure, so the results list says why
    // without opening the group.
    let groups = lib
        .store
        .read(move |conn| runs::run_groups(conn, summary.run))
        .unwrap();
    let failing = groups
        .iter()
        .find(|row| row.status == RunStatus::Failed)
        .unwrap();
    assert!(failing.message.as_ref().unwrap().contains("metrics.rf.rms"));

    // Every stage still ran: an assertion judges a group, it does not stop it.
    let stages = lib
        .store
        .read(move |conn| runs::run_stages(conn, summary.run))
        .unwrap();
    assert_eq!(stages.len(), 3 * 2);
    assert!(stages.iter().all(|row| row.status.produced_output()));
}

#[test]
fn a_pipeline_whose_assertion_will_not_parse_refuses_to_run() {
    // A test suite that quietly drops a test is worse than one that will not
    // start, so this is caught by validation rather than at evaluation.
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = measured(2.0).asserting("metrics.rf.rms <<>> 4");
    let id = save(&lib, &pipeline);

    let refused = run_pipeline(
        &lib.store,
        &registry,
        id,
        &pipeline,
        &lib.groups,
        &RunOptions::default(),
        &RunControl::new(),
    );
    let message = refused.unwrap_err().to_string();
    assert!(message.contains("assertion 0"), "{message}");
    assert!(message.contains("expected a number"), "{message}");
}

#[test]
fn re_running_the_same_pipeline_matches_its_baseline_exactly() {
    // G8: the same pipeline over the same inputs produces the same results,
    // which is what makes an exact baseline a usable gate.
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let golden = run(&lib, &registry, &measured(2.0));
    promote(&lib, "golden", golden.run, Tolerances::EXACT);

    let again = run(&lib, &registry, &measured(2.0));
    let report = compare::check_baseline(&lib.store, again.run, "golden").unwrap();
    assert!(report.passed(), "{:?}", report.lines());
    assert!(report.diff.same_pipeline);
    assert_eq!(report.diff.groups.len(), 3);
    assert!(report.diff.missing_groups.is_empty());
    assert!(report.lines()[0].contains("matched over 3 group(s)"));

    // Every signal compared sample for sample, and none diverged.
    let signals = &report.diff.groups[0].signals;
    assert!(signals.iter().all(|d| d.first_divergence.is_none()));
    assert!(signals.iter().all(|d| d.max_abs_error == 0.0));
    assert!(signals.iter().any(|d| d.stage_ordinal == SOURCE_STAGE));
}

#[test]
fn a_changed_algorithm_deviates_from_the_baseline_and_says_where() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let golden = run(&lib, &registry, &measured(2.0));
    promote(&lib, "golden", golden.run, Tolerances::EXACT);

    // The gain changes, so every sample after stage 0 differs — and the first
    // one that does is sample 0 of the first group whose ramp is not zero.
    let changed = run(&lib, &registry, &measured(2.5));
    let report = compare::check_baseline(&lib.store, changed.run, "golden").unwrap();
    assert!(!report.passed());
    assert!(!report.diff.same_pipeline, "the parameters differ");

    let deviations = report.diff.deviations();
    assert_eq!(deviations.len(), 3, "every group deviates");
    let lines = report.lines().join("\n");
    assert!(lines.contains("first differs at sample"), "{lines}");
    assert!(lines.contains("metric 'rf.rms'"), "{lines}");
    assert!(lines.contains("artifact 'statistics'"), "{lines}");

    // The source signals are untouched by the change; the deviation starts at
    // the stage that changed.
    let group = &report.diff.groups[1];
    let source: Vec<_> = group
        .signals
        .iter()
        .filter(|d| d.stage_ordinal == SOURCE_STAGE)
        .collect();
    assert!(source.iter().all(|d| d.within_tolerance));
    let gained = group.signals.iter().find(|d| d.stage_ordinal == 0).unwrap();
    assert!(!gained.within_tolerance);
    assert_eq!(gained.first_divergence, Some(0));
    assert!(gained.rms_error > 0.0);
}

#[test]
fn a_tolerance_is_what_decides_whether_a_difference_is_a_regression() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let golden = run(&lib, &registry, &measured(2.0));

    // A gain a hair off produces differences of about 0.1% of the signal.
    let nudged = run(&lib, &registry, &measured(2.001));

    let exact = compare::diff_runs(
        &lib.store,
        golden.run,
        nudged.run,
        &DiffOptions::within(Tolerances::EXACT),
    )
    .unwrap();
    assert!(!exact.is_clean(), "an exact baseline catches any drift");

    let loose = Tolerances {
        sample_rel: 0.01,
        metric_rel: 0.01,
        artifact_abs: 0.5,
        ..Tolerances::EXACT
    };
    let tolerated = compare::diff_runs(
        &lib.store,
        golden.run,
        nudged.run,
        &DiffOptions::within(loose),
    )
    .unwrap();
    assert!(
        tolerated.is_clean(),
        "1% drift under a 1% tolerance is not a regression: {:?}",
        tolerated.deviations()
    );
    assert!(tolerated.describe().contains("matches run"));
}

#[test]
fn an_assertion_can_compare_against_the_baseline_it_was_given() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let golden = run(&lib, &registry, &measured(2.0));

    let pipeline = measured(2.0).asserting("metrics.rf.rms within 1% of baseline");

    // Without a baseline the assertion has nothing to say — and says so,
    // rather than passing.
    let alone = run(&lib, &registry, &pipeline);
    assert_eq!(alone.status, RunStatus::Ok);
    assert_eq!(alone.assertions_passed, 0);
    let outcomes = lib
        .store
        .read(move |conn| regress::run_assertions(conn, alone.run))
        .unwrap();
    assert!(outcomes
        .iter()
        .all(|row| row.status == AssertStatus::NotApplicable));

    // Given one, the same run passes against itself.
    let against = run_with(
        &lib,
        &registry,
        &pipeline,
        &RunOptions::default().against_baseline(golden.run),
    );
    assert_eq!(against.status, RunStatus::Ok, "{}", against.describe());
    assert_eq!(against.assertions_passed, 3);

    // A changed algorithm breaks the same assertion.
    let changed = measured(4.0).asserting("metrics.rf.rms within 1% of baseline");
    let broken = run_with(
        &lib,
        &registry,
        &changed,
        &RunOptions::default().against_baseline(golden.run),
    );
    assert_eq!(broken.status, RunStatus::Failed);
    assert_eq!(broken.assertions_failed, 3);
}

#[test]
fn a_group_the_baseline_covers_and_the_run_does_not_is_a_failure() {
    // A case that stopped being tested must never read as a pass.
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let golden = run(&lib, &registry, &measured(2.0));
    promote(&lib, "golden", golden.run, Tolerances::EXACT);

    let pipeline = measured(2.0);
    let id = save(&lib, &pipeline);
    let partial = run_pipeline(
        &lib.store,
        &registry,
        id,
        &pipeline,
        &lib.groups[..2],
        &RunOptions::default(),
        &RunControl::new(),
    )
    .unwrap();

    let report = compare::check_baseline(&lib.store, partial.run, "golden").unwrap();
    assert!(!report.passed());
    assert_eq!(report.diff.missing_groups, vec![lib.groups[2]]);
    assert!(report
        .lines()
        .join("\n")
        .contains("did not cover this group"));
}

#[test]
fn a_baseline_cannot_be_checked_against_itself() {
    let lib = library();
    let registry = sp_dsp::registry().unwrap();
    let golden = run(&lib, &registry, &measured(2.0));
    promote(&lib, "golden", golden.run, Tolerances::EXACT);

    let error = compare::check_baseline(&lib.store, golden.run, "golden").unwrap_err();
    assert!(error.to_string().contains("is the baseline"));

    let unknown = compare::check_baseline(&lib.store, golden.run, "nope").unwrap_err();
    assert!(unknown.to_string().contains("no baseline"));
}
