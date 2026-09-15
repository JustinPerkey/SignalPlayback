//! The M11 exit criterion: each stage family has a stage that runs end to end
//! (`docs/DESIGN.md` §16.1).
//!
//! The conformance harness in `tests/conformance.rs` says each of them honours
//! the contract. This says they do something: two pipelines go through the
//! whole stack — registry, scheduler, run tables, blob store — over data whose
//! answer is known in advance, and the artifacts come back out of the database
//! saying what went in.

use sp_core::run::RunStatus;
use sp_core::{DType, DatasetId, GroupId, PipelineId, SampleBuffer, SourceKind, Timebase};
use sp_dsp::artifacts::{Bits, Metrics, Peaks, Symbols};
use sp_proc::pipeline::{Pipeline, PipelineStage};
use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions, RunSummary};
use sp_proc::StageRegistry;
use sp_store::library::{NewDataset, NewGroup, NewSignal};
use sp_store::runs;
use sp_store::trains::NewTrain;
use sp_store::{library, trains, Store};

/// The sample rate every group in this file is captured at.
const RATE_HZ: f64 = 1000.0;

struct Library {
    _dir: tempfile::TempDir,
    store: Store,
    dataset: DatasetId,
    groups: Vec<GroupId>,
}

/// A library of one group holding one signal.
fn library(name: &str, signal: &str, values: &[f64]) -> Library {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    let samples = SampleBuffer::from_f64(DType::F64, values);
    let signal = signal.to_owned();
    let name = name.to_owned();
    let (dataset, groups) = store
        .write(move |conn| {
            let dataset =
                library::insert_dataset(conn, &NewDataset::new(name, SourceKind::Generated))?;
            let train = trains::insert_train(conn, &NewTrain::new(dataset, 0).named("capture"))?;
            let group = library::insert_group(conn, &NewGroup::new(train, 0, 0).named("dwell"))?;
            library::insert_signal(
                conn,
                &NewSignal::new(group, 0, signal, Timebase::regular(RATE_HZ, 0.0), samples),
            )?;
            Ok((dataset, vec![group]))
        })
        .unwrap();
    Library {
        _dir: dir,
        store,
        dataset,
        groups,
    }
}

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

fn run(lib: &Library, registry: &StageRegistry, pipeline: &Pipeline) -> RunSummary {
    let id = save(&lib.store, pipeline);
    run_pipeline(
        &lib.store,
        registry,
        id,
        pipeline,
        &lib.groups,
        &RunOptions::default().over_dataset(lib.dataset),
        &RunControl::new(),
    )
    .expect("the run itself should not fail")
}

/// The artifact stage `ordinal` published for the run's only group, decoded
/// out of the database rather than out of the stage that made it.
fn artifact<A: serde::de::DeserializeOwned>(
    lib: &Library,
    summary: &RunSummary,
    ordinal: i32,
) -> A {
    let rows = lib
        .store
        .read(|conn| runs::stage_artifacts(conn, summary.run, Some(lib.groups[0]), ordinal))
        .unwrap();
    assert_eq!(rows.len(), 1, "stage {ordinal} should publish one artifact");
    let payload = lib
        .store
        .read(|conn| runs::artifact_payload(conn, &rows[0]))
        .unwrap();
    serde_json::from_str(&payload).unwrap()
}

/// Four 20 ms pulses at 100 ms intervals, in 0.4 s of noise-free baseline.
///
/// The first pulse starts 10 ms in rather than at the first sample: a peak
/// finder cannot claim a maximum whose rising edge is off the end of the
/// capture, and a capture that opens mid-pulse is a different test.
fn pulse_train() -> Vec<f64> {
    let mut values = vec![0.0; 400];
    for pulse in 0..4 {
        let start = 10 + pulse * 100;
        for sample in values.iter_mut().skip(start).take(20) {
            *sample = 1.0;
        }
    }
    values
}

/// An NRZ waveform at ±1, ten samples per symbol — a 100 Hz symbol rate.
fn nrz(bits: &[u8]) -> Vec<f64> {
    bits.iter()
        .flat_map(|&bit| [if bit == 1 { 1.0 } else { -1.0 }; 10])
        .collect()
}

#[test]
fn detection_and_measurement_run_end_to_end_over_a_pulse_train() {
    // Threshold finds the pulses, peak find agrees about where they are, and
    // pulse metrics turns the spans into the numbers the capture is named by.
    let lib = library("pulses", "rf", &pulse_train());
    let registry = sp_dsp::registry().unwrap();
    let pipeline = Pipeline::new("pulse analysis")
        .with_stage(PipelineStage::new("dsp.detect.threshold").with_param("level", 0.5))
        .with_stage(
            PipelineStage::new("dsp.detect.peaks")
                .with_param("min_prominence", 0.5)
                .with_param("min_distance_s", 0.05),
        )
        .with_stage(PipelineStage::new("dsp.measure.pulse").with_param("write_back", true));

    let summary = run(&lib, &registry, &pipeline);
    assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());
    assert_eq!(summary.stages_run, 3);

    let peaks: Peaks = artifact(&lib, &summary, 1);
    assert_eq!(peaks.len(), 4, "one per pulse, not one per flat top sample");

    let metrics: Metrics = artifact(&lib, &summary, 2);
    assert_eq!(metrics.get("pulses"), Some(4.0));
    assert!((metrics.get("prf").unwrap() - 10.0).abs() < 1e-9);
    assert!((metrics.get("width_mean").unwrap() - 0.02).abs() < 1e-9);
    assert!((metrics.get("duty").unwrap() - 0.2).abs() < 1e-9);

    // Every measurement is also a recorded metric, which is what the chart
    // across groups reads and what an assertion is written against (§10.5).
    let stages = lib
        .store
        .read(|conn| runs::group_stages(conn, summary.run, lib.groups[0]))
        .unwrap();
    assert_eq!(stages.len(), 3);
    assert!((stages[2].metrics["prf"] - 10.0).abs() < 1e-9);
    assert_eq!(stages[0].metrics.get("detections"), Some(&4.0));
    assert_eq!(stages[1].metrics.get("peaks"), Some(&4.0));
}

#[test]
fn symbol_decode_runs_end_to_end_and_the_bits_are_the_bits_that_went_in() {
    let sent = [1u8, 0, 1, 1, 0, 0, 1, 0];
    let lib = library("nrz", "rf", &nrz(&sent));
    let registry = sp_dsp::registry().unwrap();
    let pipeline = Pipeline::new("demodulate")
        .with_stage(PipelineStage::new("dsp.digital.slice"))
        .with_stage(
            PipelineStage::new("dsp.digital.symbols")
                .with_param("signal", "logic")
                .with_param("symbol_rate_hz", 100.0)
                .with_param("level", 0.5)
                .with_param("spacing", 1.0),
        )
        .with_stage(PipelineStage::new("dsp.digital.bits"));

    let summary = run(&lib, &registry, &pipeline);
    assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());

    // The slicer's logic signal is persisted beside the waveform it came from.
    let sliced = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, lib.groups[0], 0))
        .unwrap();
    assert_eq!(sliced.len(), 2);
    assert_eq!(sliced[1].name, "logic");
    assert_eq!(sliced[1].domain, sp_core::Domain::DigitalLogic);

    let symbols: Symbols = artifact(&lib, &summary, 1);
    let expected: Vec<i64> = sent.iter().map(|&bit| i64::from(bit)).collect();
    assert_eq!(symbols.symbol, expected);
    assert_eq!(symbols.signal, "logic");

    // The port carried the decoder's artifact to the packer without either of
    // them naming the other (§9.3).
    let bits: Bits = artifact(&lib, &summary, 2);
    assert_eq!(bits.digits(), "10110010");
    assert_eq!(bits.count, 8);
}

#[test]
fn a_family_pipeline_is_valid_before_it_is_run() {
    // The packer's required artifact port is satisfied by the decoder in front
    // of it, and by nothing else: editing the decoder out is an error the
    // editor reports rather than a run that fails at the first group.
    let registry = sp_dsp::registry().unwrap();
    let decode = PipelineStage::new("dsp.digital.symbols").with_param("symbol_rate_hz", 100.0);

    let whole = Pipeline::new("demodulate")
        .with_stage(PipelineStage::new("dsp.digital.slice"))
        .with_stage(decode)
        .with_stage(PipelineStage::new("dsp.digital.bits"));
    assert!(whole.validate(&registry).is_ok());

    let broken = Pipeline::new("demodulate")
        .with_stage(PipelineStage::new("dsp.digital.slice"))
        .with_stage(PipelineStage::new("dsp.digital.bits"));
    let issues = broken.issues(&registry);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(format!("{issues:?}").contains("symbols"));
}
