//! Run-to-run diffing, against runs written by hand (`docs/DESIGN.md` §10.4).
//!
//! Fabricating the run rows rather than running a pipeline is what makes the
//! awkward cases reachable: a single sample changed in the middle of a column,
//! a NaN where a number used to be, an artifact that stopped being published.
//! The end-to-end path is covered by `sp-dsp`'s regression tests.

use sp_core::run::{Disposition, RunStatus, StageStatus};
use sp_core::{DType, GroupId, PipelineId, RunId, SampleBuffer, SourceKind, Timebase, Tolerances};
use sp_proc::compare::{diff_runs, DiffOptions};
use sp_store::library::{NewDataset, NewGroup};
use sp_store::runs::{self, NewArtifact, NewRun, NewRunSignal, RunGroupRow, RunStageRow};
use sp_store::trains::NewTrain;
use sp_store::{library, trains, Store};

const STAGE: i32 = 0;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    pipeline: PipelineId,
    group: GroupId,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    let (pipeline, group) = store
        .write(|conn| {
            let dataset =
                library::insert_dataset(conn, &NewDataset::new("d", SourceKind::Generated))?;
            let train = trains::insert_train(conn, &NewTrain::new(dataset, 0))?;
            let group = library::insert_group(conn, &NewGroup::new(train, 0, 0))?;
            let pipeline = runs::insert_pipeline(conn, &runs::NewPipeline::new("p"))?;
            Ok((pipeline, group))
        })
        .unwrap();
    Fixture {
        _dir: dir,
        store,
        pipeline,
        group,
    }
}

/// What one fabricated run recorded for its single group and stage.
struct Recorded {
    samples: Vec<f64>,
    metrics: Vec<(&'static str, f64)>,
    artifact: Option<String>,
    status: RunStatus,
    hash: &'static str,
}

impl Default for Recorded {
    fn default() -> Self {
        Self {
            samples: (0..8).map(f64::from).collect(),
            metrics: vec![("snr_db", 14.0)],
            artifact: Some(r#"{"peak":[0.9,0.4],"signal":["rf","rf"]}"#.to_owned()),
            status: RunStatus::Ok,
            hash: "hash",
        }
    }
}

/// Writes a run the way the scheduler would, without running anything.
fn record(fixture: &Fixture, what: Recorded) -> RunId {
    let pipeline = fixture.pipeline;
    let group = fixture.group;
    fixture
        .store
        .write(move |conn| {
            let run = runs::begin_run(conn, &NewRun::new(pipeline, what.hash))?;
            runs::record_group(
                conn,
                run,
                &RunGroupRow {
                    group_id: group,
                    status: what.status,
                    wall_ms: Some(3),
                    message: None,
                },
            )?;

            let mut stage = RunStageRow::new(group, STAGE, StageStatus::Ok);
            stage.wall_ms = Some(3);
            for (name, value) in &what.metrics {
                stage.metrics.insert((*name).to_owned(), *value);
            }
            runs::record_stage(conn, run, &stage)?;

            runs::record_signal(
                conn,
                run,
                &NewRunSignal::new(group, STAGE, 0, "rf", Timebase::regular(1000.0, 0.0))
                    .with_disposition(Disposition::Replaced)
                    .with_samples(SampleBuffer::from_f64(DType::F64, &what.samples)),
            )?;

            if let Some(payload) = &what.artifact {
                runs::insert_artifact(
                    conn,
                    run,
                    &NewArtifact::new(STAGE, "detections", "detections.v1", 1, payload.clone())
                        .for_group(group),
                )?;
            }
            runs::finish_run(conn, run, what.status)?;
            Ok(run)
        })
        .unwrap()
}

fn compare(fixture: &Fixture, a: RunId, b: RunId, tolerances: Tolerances) -> sp_proc::RunDiff {
    diff_runs(&fixture.store, a, b, &DiffOptions::within(tolerances)).unwrap()
}

#[test]
fn two_identical_runs_diff_to_nothing() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let b = record(&fixture, Recorded::default());

    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    assert!(diff.is_clean(), "{:?}", diff.deviations());
    assert!(diff.same_pipeline);
    assert_eq!(diff.groups.len(), 1);

    let signal = &diff.groups[0].signals[0];
    assert_eq!(signal.max_abs_error, 0.0);
    assert_eq!(signal.rms_error, 0.0);
    assert_eq!(signal.first_divergence, None);
    assert!(signal.describe().contains("identical"));
    assert!(diff.groups[0].artifacts[0].describe().contains("identical"));
}

#[test]
fn one_changed_sample_is_found_and_located() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let mut changed = Recorded::default();
    changed.samples[5] += 0.25;
    let b = record(&fixture, changed);

    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    assert!(!diff.is_clean());
    let signal = &diff.groups[0].signals[0];
    assert_eq!(signal.first_divergence, Some(5));
    assert!((signal.max_abs_error - 0.25).abs() < 1e-12);
    // One sample of eight moved by 0.25, so the RMS error is 0.25/√8.
    assert!((signal.rms_error - 0.25 / 8.0f64.sqrt()).abs() < 1e-12);
    assert!(signal.describe().contains("first differs at sample 5"));

    // A tolerance wide enough to cover it makes the same diff clean.
    let tolerated = compare(
        &fixture,
        a,
        b,
        Tolerances {
            sample_abs: 0.5,
            ..Tolerances::EXACT
        },
    );
    assert!(tolerated.is_clean(), "{:?}", tolerated.deviations());
    assert_eq!(tolerated.groups[0].signals[0].max_abs_error, 0.25);
}

#[test]
fn a_nan_where_a_number_used_to_be_is_a_deviation() {
    // Arithmetic would make the difference NaN, which compares false against
    // every bound — so a signal that started producing NaN would otherwise
    // pass a regression check silently.
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let mut broken = Recorded::default();
    broken.samples[2] = f64::NAN;
    let b = record(&fixture, broken);

    let diff = compare(
        &fixture,
        a,
        b,
        Tolerances {
            sample_abs: 1e9,
            ..Tolerances::EXACT
        },
    );
    assert!(!diff.is_clean(), "no tolerance covers a NaN");
    assert_eq!(diff.groups[0].signals[0].first_divergence, Some(2));

    // Two runs that both produce NaN in the same place have not changed.
    let c = record(&fixture, {
        let mut same = Recorded::default();
        same.samples[2] = f64::NAN;
        same
    });
    assert!(compare(&fixture, b, c, Tolerances::EXACT).is_clean());
}

#[test]
fn a_signal_that_changed_length_is_a_deviation_however_close_its_samples() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let b = record(
        &fixture,
        Recorded {
            samples: (0..6).map(f64::from).collect(),
            ..Recorded::default()
        },
    );

    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    assert!(!diff.is_clean());
    let signal = &diff.groups[0].signals[0];
    assert_eq!(signal.samples, (8, 6));
    assert_eq!(signal.first_divergence, Some(6));
    assert!(signal.describe().contains("6 samples, was 8"));
}

#[test]
fn metrics_compare_value_by_value_and_report_one_that_vanished() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let b = record(
        &fixture,
        Recorded {
            metrics: vec![("snr_db", 14.2), ("new_metric", 1.0)],
            ..Recorded::default()
        },
    );

    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    let metrics = &diff.groups[0].metrics;
    let snr = metrics.iter().find(|m| m.name == "snr_db").unwrap();
    assert!((snr.abs_error - 0.2).abs() < 1e-12);
    assert!(!snr.within_tolerance);
    assert!(snr.describe().contains("was 14"));

    // A metric only one side has cannot be within tolerance of anything.
    let added = metrics.iter().find(|m| m.name == "new_metric").unwrap();
    assert_eq!(added.values, (None, Some(1.0)));
    assert!(!added.within_tolerance);
    assert!(added.describe().contains("new"));

    // 2% covers the change to snr_db, but not the metric that appeared.
    let loose = compare(
        &fixture,
        a,
        b,
        Tolerances {
            metric_rel: 0.02,
            ..Tolerances::EXACT
        },
    );
    let snr = loose.groups[0]
        .metrics
        .iter()
        .find(|m| m.name == "snr_db")
        .unwrap();
    assert!(snr.within_tolerance);
    assert!(!loose.is_clean());
}

#[test]
fn an_artifact_diffs_field_by_field_and_notices_one_that_stopped_being_published() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let b = record(
        &fixture,
        Recorded {
            artifact: Some(r#"{"peak":[0.9,0.6],"signal":["rf","rf"]}"#.to_owned()),
            ..Recorded::default()
        },
    );

    // The payload is read through a schema inferred from itself, so this
    // works without the crate that wrote it.
    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    let artifact = &diff.groups[0].artifacts[0];
    assert!(!artifact.within_tolerance);
    let peak = artifact.fields.iter().find(|f| f.field == "peak").unwrap();
    assert_eq!(peak.mismatches, 1);
    assert_eq!(peak.first_divergence, Some(1));
    assert!(artifact.describe().contains("peak"));

    // A tolerance on artifact fields covers it.
    assert!(compare(
        &fixture,
        a,
        b,
        Tolerances {
            artifact_abs: 0.5,
            ..Tolerances::EXACT
        }
    )
    .is_clean());

    // An artifact that is no longer published is a difference in itself.
    let gone = record(
        &fixture,
        Recorded {
            artifact: None,
            ..Recorded::default()
        },
    );
    let diff = compare(&fixture, a, gone, Tolerances::EXACT);
    assert!(!diff.is_clean());
    assert!(diff.groups[0].artifacts[0]
        .describe()
        .contains("did not publish"));
}

#[test]
fn a_group_that_started_failing_is_a_regression_however_identical_its_numbers() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let b = record(
        &fixture,
        Recorded {
            status: RunStatus::Failed,
            ..Recorded::default()
        },
    );

    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    assert!(!diff.is_clean());
    let reasons = &diff.deviations()[0].1;
    assert!(
        reasons[0].contains("is failed, was succeeded"),
        "{reasons:?}"
    );
}

#[test]
fn a_different_pipeline_is_reported_but_does_not_fail_the_comparison_by_itself() {
    // Comparing an old and a new pipeline is exactly how a deliberate change
    // is reviewed, so the hash is information rather than a verdict.
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let b = record(
        &fixture,
        Recorded {
            hash: "a different algorithm",
            ..Recorded::default()
        },
    );

    let diff = compare(&fixture, a, b, Tolerances::EXACT);
    assert!(!diff.same_pipeline);
    assert!(diff.is_clean(), "{:?}", diff.deviations());
}

#[test]
fn comparing_only_the_last_stage_skips_the_ones_before_it() {
    let fixture = fixture();
    let a = record(&fixture, Recorded::default());
    let mut changed = Recorded::default();
    changed.samples[0] += 1.0;
    let b = record(&fixture, changed);

    let everywhere = diff_runs(&fixture.store, a, b, &DiffOptions::default()).unwrap();
    let only_last = diff_runs(
        &fixture.store,
        a,
        b,
        &DiffOptions::default().final_stage_only(),
    )
    .unwrap();

    // Both find the change; the narrower one looks at one stage to do it.
    assert!(!everywhere.is_clean());
    assert!(!only_last.is_clean());
    assert!(only_last.groups[0]
        .signals
        .iter()
        .all(|signal| signal.stage_ordinal == STAGE));
}
