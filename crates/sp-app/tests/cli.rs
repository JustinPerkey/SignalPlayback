//! The M7 exit criterion, exercised the way CI would: the real binary, a real
//! library, and an exit code (`docs/DESIGN.md` §16, G8).
//!
//! These tests spawn `signalplayback` as a process rather than calling into
//! it, because the thing being checked is precisely what a CI job sees — the
//! exit status and the text on stdout.

use std::path::Path;
use std::process::{Command, Output};

use sp_core::{DType, PipelineId, SampleBuffer, SourceKind, Timebase};
use sp_proc::pipeline::{Pipeline, PipelineStage};
use sp_store::library::{NewDataset, NewGroup, NewSignal};
use sp_store::runs::NewPipeline;
use sp_store::trains::NewTrain;
use sp_store::{library, regress, runs, trains, Store};

/// A library with one dataset of two groups and one saved pipeline: gain then
/// statistics, with an assertion that holds.
fn build_library(path: &Path, gain: f64) -> PipelineId {
    let store = Store::open(path).unwrap();
    let pipeline = Pipeline::new("detector")
        .with_stage(PipelineStage::new("dsp.condition.gain").with_param("gain", gain))
        .with_stage(PipelineStage::new("dsp.measure.statistics"))
        .asserting("signals[rf].samples == 16");
    let stages = pipeline.to_rows();
    let assertions = pipeline.to_assertion_rows();

    store
        .write(move |conn| {
            let dataset =
                library::insert_dataset(conn, &NewDataset::new("ladder", SourceKind::Generated))?;
            let train = trains::insert_train(conn, &NewTrain::new(dataset, 0))?;
            for ordinal in 0..2u32 {
                let group = library::insert_group(conn, &NewGroup::new(train, ordinal, 0))?;
                let values: Vec<f64> = (0..16)
                    .map(|i| f64::from(ordinal) + f64::from(i) * 0.5)
                    .collect();
                library::insert_signal(
                    conn,
                    &NewSignal::new(
                        group,
                        0,
                        "rf",
                        Timebase::regular(1000.0, 0.0),
                        SampleBuffer::from_f64(DType::F64, &values),
                    ),
                )?;
            }
            let id = runs::insert_pipeline(conn, &NewPipeline::new("detector"))?;
            runs::set_pipeline_stages(conn, id, &stages)?;
            regress::set_pipeline_assertions(conn, id, &assertions)?;
            Ok(id)
        })
        .unwrap()
}

/// Rewrites the saved pipeline's gain, as editing it in the window would.
fn change_gain(path: &Path, id: PipelineId, gain: f64) {
    let store = Store::open(path).unwrap();
    let rows = Pipeline::new("detector")
        .with_stage(PipelineStage::new("dsp.condition.gain").with_param("gain", gain))
        .with_stage(PipelineStage::new("dsp.measure.statistics"))
        .to_rows();
    store
        .write(move |conn| runs::set_pipeline_stages(conn, id, &rows))
        .unwrap();
}

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_signalplayback"))
        .args(args)
        .output()
        .expect("the binary should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_pipeline_runs_headlessly_and_fails_on_a_baseline_deviation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let library = path.to_string_lossy().into_owned();
    let id = build_library(&path, 2.0);

    // 1. A first run, promoted to the baseline every later run answers to.
    let first = cli(&[
        "run",
        "--library",
        &library,
        "--pipeline",
        "detector",
        "--dataset",
        "ladder",
        "--promote",
        "golden",
    ]);
    assert_eq!(first.status.code(), Some(0), "{}", stdout(&first));
    assert!(
        stdout(&first).contains("2 group(s) ok"),
        "{}",
        stdout(&first)
    );
    assert!(stdout(&first).contains("promoted run"));

    // 2. The same pipeline again matches it — G8, from the outside.
    let again = cli(&[
        "run",
        "--library",
        &library,
        "--pipeline",
        "detector",
        "--dataset",
        "ladder",
        "--assert-baseline",
        "golden",
        "--no-cache",
    ]);
    assert_eq!(again.status.code(), Some(0), "{}", stdout(&again));
    assert!(stdout(&again).contains("matched over 2 group(s)"));

    // 3. Changing the algorithm fails the run, and says where.
    change_gain(&path, id, 3.0);
    let changed = cli(&[
        "run",
        "--library",
        &library,
        "--pipeline",
        "detector",
        "--dataset",
        "ladder",
        "--assert-baseline",
        "golden",
        "--no-cache",
    ]);
    let text = stdout(&changed);
    assert_eq!(changed.status.code(), Some(2), "{text}");
    assert!(text.contains("deviates from run"), "{text}");
    assert!(text.contains("first differs at sample"), "{text}");

    // 4. The baseline is listed by name, still pointing at the first run.
    let listed = cli(&["baselines", "--library", &library]);
    assert_eq!(listed.status.code(), Some(0));
    assert!(stdout(&listed).contains("golden"));
    assert!(stdout(&listed).contains("exact"));
}

#[test]
fn a_failing_assertion_fails_the_run_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let library = path.to_string_lossy().into_owned();
    let id = build_library(&path, 2.0);

    // An assertion that cannot hold: the signals are 16 samples long.
    let store = Store::open(&path).unwrap();
    store
        .write(move |conn| {
            regress::set_pipeline_assertions(
                conn,
                id,
                &[regress::AssertionRow::new(0, "signals[rf].samples == 99")],
            )
        })
        .unwrap();
    drop(store);

    let output = cli(&[
        "run",
        "--library",
        &library,
        "--pipeline",
        "detector",
        "--dataset",
        "ladder",
    ]);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(text.contains("2 failed"), "{text}");
    assert!(text.contains("signals[rf].samples == 99"), "{text}");
}

#[test]
fn a_request_that_is_simply_wrong_exits_differently_from_a_run_that_failed() {
    // Exit 1 says the harness is wrong; exit 2 says the algorithm regressed.
    // A CI job that cannot tell them apart cannot be trusted either way.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let library = path.to_string_lossy().into_owned();
    build_library(&path, 2.0);

    let missing = cli(&["run", "--library", &library, "--pipeline", "nonesuch"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no pipeline called 'nonesuch'"));

    let unknown_baseline = cli(&[
        "run",
        "--library",
        &library,
        "--pipeline",
        "detector",
        "--assert-baseline",
        "nope",
    ]);
    assert_eq!(unknown_baseline.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unknown_baseline.stderr).contains("no baseline"));

    let no_library = cli(&["run", "--library", "/nowhere/library.db", "--pipeline", "p"]);
    assert_eq!(no_library.status.code(), Some(1));
}

#[test]
fn help_lists_the_commands_and_the_exit_codes() {
    let output = cli(&["help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("--assert-baseline"));
    assert!(text.contains("Exit codes"));
}
