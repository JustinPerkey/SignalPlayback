//! The long way through the program, in one place: a spec is generated into
//! the library, run through a pipeline, read back out of the run tables,
//! played back over a pyramid, and generated a second time to prove the whole
//! chain is content-addressed — plus a captured file that goes in, out, and
//! back in again with the same rows on both sides (`docs/DESIGN.md` §2, §16).
//!
//! Every crate already has a suite over its own share of that path. What none
//! of them can check is the join: that the number the run records is the
//! number the spec put in, is the number the inspector recomputes, and is the
//! number a second run reproduces without touching a sample. `sp-app` is the
//! only crate that sees `sp-gen`, `sp-csv`, `sp-proc`, `sp-dsp`, `sp-engine`
//! and `sp-store` at once, so the joins are tested from here.
//!
//! The fixture is chosen so the arithmetic is exact rather than approximate:
//! 256 samples at 1024 Hz hold exactly sixteen cycles of a 64 Hz tone, which
//! is a whole number of periods (so RMS is `amp/√2`) and lands on FFT bin 16
//! of a 256-point transform (so a rectangular window leaks into nothing).

use std::f64::consts::SQRT_2;
use std::path::{Path, PathBuf};

use sp_core::run::{Disposition, RunStatus, StageStatus};
use sp_core::time::SampleRange;
use sp_core::{
    DType, DatasetId, Domain, GroupId, PipelineId, PropScope, Provenance, RunId, Signal, SignalId,
    SourceKind, TimeRange, Timebase,
};
use sp_csv::{export_dataset_to_file, import_file, ImportControl, ImportProfile, ImportRequest};
use sp_dsp::artifacts::{Spectrum, Statistics};
use sp_engine::reduce::{TraceDescriptor, TraceGeometry, TraceStyle};
use sp_engine::source::{self, ColumnSource};
use sp_engine::viewport::Amplitude;
use sp_engine::{reduce, Transport, Viewport};
use sp_gen::generate::{GenReport, GenRequest, SEED_KEY};
use sp_gen::spec::Node;
use sp_gen::sweep::{ParamSweep, SweepValues};
use sp_gen::{tree, GenControl, GenSpec};
use sp_proc::pipeline::{Pipeline, PipelineStage};
use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions, RunSummary};
use sp_proc::StageRegistry;
use sp_store::{library, props, pulses, runs, stats, trains, verify, PropertyQuery, Store};

const RATE_HZ: f64 = 1_024.0;
const DURATION_S: f64 = 0.25;
const SAMPLES: u64 = 256;
const TONE_HZ: f64 = 64.0;
/// One rung per amplitude. This is the sweep laid out as *one group*, which
/// is what makes a curve across the signals of a group; the group-per-rung
/// ladder of §8.4 is held by `sp-gen/tests/ladder.rs`. Either shape feeds a
/// run, and this file is about the loop rather than about the layout.
const AMPLITUDES: [f64; 3] = [0.25, 0.5, 1.0];
const GAIN: f64 = 2.0;
const SEED: u64 = 7;

/// A library holding one generated sweep, ready to run.
struct Ladder {
    _dir: tempfile::TempDir,
    store: Store,
    dataset: DatasetId,
    group: GroupId,
    signals: Vec<Signal>,
}

impl Ladder {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let report = generate_ladder(&store, "ladder");
        let group = report.group_id;
        let signals = store
            .read(move |conn| library::list_signals(conn, group))
            .unwrap();
        Self {
            _dir: dir,
            store,
            dataset: report.dataset_id,
            group,
            signals,
        }
    }

    fn names(&self) -> Vec<String> {
        self.signals.iter().map(|s| s.name.clone()).collect()
    }

    /// Runs `pipeline` over the ladder's one group, as the Pipeline screen
    /// does, and hands back the summary.
    fn run(
        &self,
        registry: &StageRegistry,
        pipeline: &Pipeline,
        options: &RunOptions,
    ) -> RunSummary {
        let id = save(&self.store, pipeline);
        self.run_saved(registry, pipeline, id, &[self.group], options)
    }

    fn run_saved(
        &self,
        registry: &StageRegistry,
        pipeline: &Pipeline,
        id: PipelineId,
        groups: &[GroupId],
        options: &RunOptions,
    ) -> RunSummary {
        run_pipeline(
            &self.store,
            registry,
            id,
            pipeline,
            groups,
            &options.clone().over_dataset(self.dataset),
            &RunControl::new(),
        )
        .expect("the run itself should not fail")
    }

    fn stage_metrics(&self, run: RunId, stage: i32) -> std::collections::BTreeMap<String, f64> {
        let group = self.group;
        self.store
            .read(move |conn| runs::group_stages(conn, run, group))
            .unwrap()
            .into_iter()
            .find(|row| row.stage_ordinal == stage)
            .unwrap_or_else(|| panic!("stage {stage} should have been recorded"))
            .metrics
    }

    /// The artifact one stage published for the ladder's group, decoded.
    fn artifact<T: serde::de::DeserializeOwned>(&self, run: RunId, stage: i32) -> T {
        self.artifact_for(run, self.group, stage)
    }

    /// The same, for a group other than the ladder's own.
    fn artifact_for<T: serde::de::DeserializeOwned>(
        &self,
        run: RunId,
        group: GroupId,
        stage: i32,
    ) -> T {
        let rows = self
            .store
            .read(move |conn| runs::stage_artifacts(conn, run, Some(group), stage))
            .unwrap();
        assert_eq!(rows.len(), 1, "stage {stage} should publish one artifact");
        let payload = self
            .store
            .read(|conn| runs::artifact_payload(conn, &rows[0]))
            .unwrap();
        serde_json::from_str(&payload).expect("the payload should decode as its declared kind")
    }
}

/// A tone of a whole number of cycles, in `f64` so the arithmetic below is the
/// spec's rather than `f32`'s.
fn tone() -> GenSpec {
    GenSpec::new(
        RATE_HZ,
        DURATION_S,
        Node::Sine {
            freq_hz: TONE_HZ,
            amp: 1.0,
            phase_rad: 0.0,
            offset: 0.0,
        },
    )
    .with_dtype(DType::F64)
    .with_seed(SEED)
}

/// Generates the amplitude sweep into `store`, the way the Generate screen
/// does at its default layout: one dataset, one train, one group, one signal
/// per rung.
fn generate_ladder(store: &Store, name: &str) -> GenReport {
    let request = GenRequest::new(tone())
        .named(name)
        .with_signal_name("tone")
        .with_sweep(ParamSweep {
            pointer: tree::ROOT.to_owned(),
            field: "amp".to_owned(),
            property_key: "amp".to_owned(),
            values: SweepValues::List {
                values: AMPLITUDES.to_vec(),
            },
        });
    store
        .write(move |conn| Ok(sp_gen::generate(conn, &request, &GenControl::new())))
        .unwrap()
        .expect("the ladder should generate")
}

/// Saves a pipeline with its assertions, as the pipeline editor would.
fn save(store: &Store, pipeline: &Pipeline) -> PipelineId {
    let name = pipeline.name.clone();
    let stages = pipeline.to_rows();
    let assertions = pipeline.to_assertion_rows();
    store
        .write(move |conn| {
            let id = runs::insert_pipeline(conn, &runs::NewPipeline::new(name))?;
            runs::set_pipeline_stages(conn, id, &stages)?;
            sp_store::regress::set_pipeline_assertions(conn, id, &assertions)?;
            Ok(id)
        })
        .unwrap()
}

/// Passthrough → gain → statistics, then one FFT per rung.
///
/// The FFT stage transforms one signal, so a ladder needs one instance per
/// rung — which is also what the parameter is for (§9.8).
fn measured(names: &[String]) -> Pipeline {
    let mut pipeline = Pipeline::new("ladder")
        .with_stage(PipelineStage::new("dsp.util.passthrough").labelled("Raw"))
        .with_stage(PipelineStage::new("dsp.condition.gain").with_param("gain", GAIN))
        .with_stage(PipelineStage::new("dsp.measure.statistics").with_param("write_back", true));
    for name in names {
        pipeline = pipeline.with_stage(
            PipelineStage::new("dsp.transform.fft")
                .with_param("signal", name.as_str())
                // Sixteen whole cycles in a 256-point transform sit on a bin
                // centre, so the rectangular window is the exact one here.
                .with_param("window", "rectangular"),
        );
    }
    pipeline
}

/// The ordinal of the FFT stage that transforms rung `rung`.
fn fft_stage(rung: usize) -> i32 {
    3 + rung as i32
}

fn close(left: f64, right: f64, tolerance: f64) -> bool {
    (left - right).abs() <= tolerance
}

// ---------------------------------------------------------------------------
// Generation into the library
// ---------------------------------------------------------------------------

#[test]
fn a_generated_ladder_lands_in_the_library_as_a_group_a_run_can_take() {
    let lib = Ladder::new();

    // One action, one dataset, one train, one group — the shape an import
    // produces, so the two are interchangeable as pipeline input (§6.6).
    let (dataset, trains, groups) = lib
        .store
        .read(|conn| {
            let dataset = library::get_dataset(conn, lib.dataset)?;
            let trains = trains::list_trains(conn, lib.dataset)?;
            let groups = library::list_groups(conn, trains[0].id)?;
            Ok((dataset, trains, groups))
        })
        .unwrap();
    assert_eq!(dataset.name, "ladder");
    assert_eq!(dataset.source_kind, SourceKind::Generated);
    assert_eq!(trains.len(), 1);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].declared_count, AMPLITUDES.len() as u32);

    assert_eq!(lib.signals.len(), AMPLITUDES.len());
    for (signal, amp) in lib.signals.iter().zip(AMPLITUDES) {
        assert_eq!(signal.provenance, Provenance::Generated);
        assert_eq!(signal.domain, Domain::Analog);
        assert_eq!(signal.sample_count, SAMPLES);
        assert_eq!(signal.timebase, Timebase::regular(RATE_HZ, 0.0));
        assert_eq!(signal.attributes.get_f64("amp"), Some(amp));
        assert_eq!(signal.attributes.get_i64(SEED_KEY), Some(SEED as i64));
        // The store summarised the column as it was written, so the library
        // table has numbers to sort on without re-reading a sample.
        let stats = signal.stats.expect("a stored signal carries its summary");
        assert!(close(stats.max().unwrap(), amp, 1e-9), "{stats:?}");
        assert!(close(stats.min().unwrap(), -amp, 1e-9), "{stats:?}");
    }

    // The swept value is a declared property, so the library filter can ask
    // for a rung by value rather than by name (§8.3, §15.2).
    let defs = lib
        .store
        .read(|conn| props::list_property_defs(conn, Some(PropScope::Signal)))
        .unwrap();
    assert!(defs.iter().any(|def| def.key == "amp"));

    let loud = lib
        .store
        .read(|conn| {
            props::find_signals(
                conn,
                "amp",
                &PropertyQuery::Between {
                    min: Some(0.5),
                    max: None,
                },
            )
        })
        .unwrap();
    let expected: Vec<SignalId> = lib
        .signals
        .iter()
        .filter(|s| s.attributes.get_f64("amp").unwrap() >= 0.5)
        .map(|s| s.id)
        .collect();
    assert_eq!(loud, expected, "the property index answers by value");
}

#[test]
fn the_samples_the_library_hands_back_are_the_samples_the_spec_renders() {
    // G3, through the database rather than only in memory: the samples are a
    // cache of the spec, so the spec stored beside them has to re-render them
    // bit for bit — otherwise deleting a blob is not the safe act §8 says it
    // is.
    let lib = Ladder::new();

    for signal in &lib.signals {
        let id = signal.id;
        let (json, stored) = lib
            .store
            .read(move |conn| {
                Ok((
                    library::gen_spec(conn, id)?,
                    library::read_samples(conn, id, SampleRange::first(SAMPLES))?,
                ))
            })
            .unwrap();

        let json = json.expect("a generated signal carries the spec that made it");
        let spec = GenSpec::from_json(&json).expect("the stored spec should parse");
        assert_eq!(spec.seed, SEED);
        assert_eq!(spec.sample_count(), SAMPLES);

        let rendered = sp_gen::render(&spec).unwrap();
        assert_eq!(rendered.dtype(), stored.dtype());
        let rendered: Vec<f64> = rendered.values().collect();
        let stored: Vec<f64> = stored.values().collect();
        assert_eq!(rendered.len(), stored.len());
        for (index, (a, b)) in rendered.iter().zip(&stored).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "'{}' differs at sample {index}",
                signal.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Generation → run → results
// ---------------------------------------------------------------------------

#[test]
fn a_run_over_the_ladder_measures_what_the_spec_put_into_it() {
    let lib = Ladder::new();
    let registry = sp_dsp::registry().unwrap();
    let names = lib.names();
    // The rung the sweep named after its value, quoted because the name has a
    // space in it (§9.7).
    let pipeline = measured(&names).asserting(r#"signals["tone 1"].samples == 256"#);
    let summary = lib.run(&registry, &pipeline, &RunOptions::default());

    assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());
    assert_eq!(summary.groups_ok, 1);
    assert_eq!(summary.assertions_failed, 0);
    assert_eq!(summary.stages_run, 3 + AMPLITUDES.len());

    // 1. The measurement stage's artifact names the rungs in the order the
    //    sweep produced them.
    let statistics: Statistics = lib.artifact(summary.run, 2);
    assert_eq!(statistics.names, names);

    // 2. And its numbers are the spec's, put through the gain: a whole number
    //    of cycles has RMS amp/√2 and peaks at ±amp.
    for (rung, amp) in AMPLITUDES.iter().enumerate() {
        let expected_peak = GAIN * amp;
        let expected_rms = expected_peak / SQRT_2;
        assert!(
            close(statistics.rms[rung], expected_rms, 1e-9),
            "rung {rung}: rms {} should be {expected_rms}",
            statistics.rms[rung]
        );
        assert!(close(statistics.max[rung], expected_peak, 1e-9));
        assert!(close(statistics.min[rung], -expected_peak, 1e-9));
        assert!(close(statistics.mean[rung], 0.0, 1e-9));
    }

    // 3. The metrics chart reads the same numbers, one series per rung, and
    //    they climb with the swept value — the curve §8.4 says falls out of
    //    one run.
    let metrics = lib.stage_metrics(summary.run, 2);
    let charted: Vec<f64> = names
        .iter()
        .map(|name| metrics[&format!("{name}.rms")])
        .collect();
    assert_eq!(charted, statistics.rms);
    assert!(
        charted.windows(2).all(|pair| pair[1] > pair[0]),
        "{charted:?} should rise with the amplitude"
    );

    // 4. The spectrum of every rung peaks at the frequency the spec asked
    //    for, at the amplitude the gain left it at.
    for (rung, amp) in AMPLITUDES.iter().enumerate() {
        let stage = fft_stage(rung);
        let spectrum: Spectrum = lib.artifact(summary.run, stage);
        assert_eq!(spectrum.signal, names[rung]);
        let (hz, db) = spectrum.peak().expect("a tone has a peak");
        assert!(close(hz, TONE_HZ, 1e-9), "rung {rung} peaked at {hz} Hz");
        assert!(
            close(db, 20.0 * (GAIN * amp).log10(), 1e-6),
            "rung {rung} peaked at {db} dB"
        );

        let metrics = lib.stage_metrics(summary.run, stage);
        assert_eq!(metrics.get("peak_hz"), Some(&hz));
        assert_eq!(metrics.get("fft_size"), Some(&(SAMPLES as f64)));
    }

    // 5. The write-back put the measurement onto the recorded signal, so the
    //    number travels with the result and not only with the run.
    let recorded = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, lib.group, 2))
        .unwrap();
    for (row, amp) in recorded.iter().zip(AMPLITUDES) {
        let rms = row.attributes.get_f64("rms").expect("write_back");
        assert!(close(rms, GAIN * amp / SQRT_2, 1e-9));
    }

    // 6. The source is recorded like any other stage, and the passthrough
    //    shares the library's own blob rather than copying it (§5.3, §9.6).
    let source = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, lib.group, runs::SOURCE_STAGE))
        .unwrap();
    let passed = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, lib.group, 0))
        .unwrap();
    let library_blobs: Vec<_> = lib
        .signals
        .iter()
        .map(|signal| {
            let id = signal.id;
            lib.store
                .read(move |conn| library::signal_blob(conn, id))
                .unwrap()
        })
        .collect();
    assert_eq!(
        source
            .iter()
            .map(|row| row.blob_id.unwrap())
            .collect::<Vec<_>>(),
        library_blobs
    );
    assert!(passed
        .iter()
        .all(|row| row.disposition == Disposition::Passthrough));
    assert_eq!(
        passed
            .iter()
            .map(|row| row.blob_id.unwrap())
            .collect::<Vec<_>>(),
        library_blobs
    );

    // 7. Nothing the run wrote left the library inconsistent.
    let report = lib.store.read(verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn the_numbers_a_run_records_are_the_numbers_the_inspector_recomputes() {
    // Two independent paths to the same figure: the summary the store folded
    // as the column was written and carried into the run, against a fresh
    // streaming pass over the stored samples (§15.3). If they can disagree,
    // the Inspector and the Results screen can show different numbers for one
    // signal, which is the one thing this product may not do.
    let lib = Ladder::new();
    let registry = sp_dsp::registry().unwrap();
    let pipeline =
        Pipeline::new("measure only").with_stage(PipelineStage::new("dsp.measure.statistics"));
    let summary = lib.run(&registry, &pipeline, &RunOptions::default());
    let recorded: Statistics = lib.artifact(summary.run, 0);

    for (rung, signal) in lib.signals.iter().enumerate() {
        let id = signal.id;
        let profile = lib
            .store
            .read(move |conn| stats::profile_signal(conn, id, 32))
            .unwrap();
        let fresh = profile.stats();

        assert_eq!(fresh.count(), SAMPLES);
        assert!(close(fresh.min().unwrap(), recorded.min[rung], 1e-12));
        assert!(close(fresh.max().unwrap(), recorded.max[rung], 1e-12));
        assert!(close(fresh.mean().unwrap(), recorded.mean[rung], 1e-12));
        assert!(close(fresh.rms().unwrap(), recorded.rms[rung], 1e-12));

        // The distribution behind the numbers covers every sample, so the
        // histogram the Inspector draws is of the whole column.
        let counted: u64 = profile.histogram().counts().iter().sum();
        assert_eq!(counted, SAMPLES);
    }
}

// ---------------------------------------------------------------------------
// Run → playback
// ---------------------------------------------------------------------------

#[test]
fn a_stage_output_plays_back_over_a_pyramid_of_its_own() {
    // What the Results screen does with a stage's output: build a pyramid over
    // the column the run recorded, reduce it to one cell per pixel, and put a
    // playhead on it (§10.2, §11).
    let lib = Ladder::new();
    let registry = sp_dsp::registry().unwrap();
    let summary = lib.run(
        &registry,
        &Pipeline::new("gain")
            .with_stage(PipelineStage::new("dsp.condition.gain").with_param("gain", GAIN)),
        &RunOptions::default(),
    );

    // The loudest rung, at the output of the gain stage.
    let rung = AMPLITUDES.len() - 1;
    let peak = GAIN * AMPLITUDES[rung];
    let row = lib
        .store
        .read(|conn| runs::stage_signals(conn, summary.run, lib.group, 0))
        .unwrap()[rung]
        .clone();
    assert_eq!(row.disposition, Disposition::Replaced);
    let blob = row.blob_id.expect("the stage recorded its samples");

    source::ensure(&lib.store, blob).unwrap();
    let descriptor = TraceDescriptor {
        timebase: row.timebase,
        count: row.sample_count,
        domain: row.domain,
    };
    let whole = TimeRange::new(0.0, DURATION_S);
    let full_view = Viewport::new(whole, Amplitude::new(-3.0, 3.0), 320.0);
    let snapshot = lib
        .store
        .read(|conn| {
            let column = ColumnSource::open(conn, blob).unwrap();
            Ok(reduce::trace(&column, descriptor, &full_view, TraceStyle::default()).unwrap())
        })
        .unwrap();

    assert!(!snapshot.geometry.is_empty());
    let extent = snapshot.geometry.extent();
    assert!(
        close(f64::from(extent.max), peak, 1e-6) && close(f64::from(extent.min), -peak, 1e-6),
        "the trace should span the gained tone, not the raw one: {extent:?}"
    );

    // The transport walks the same timeline the signal is on, and what is
    // under the playhead reduces to something drawable.
    let mut transport = Transport::new(whole);
    transport.play();
    transport.advance(DURATION_S / 2.0);
    let playhead = transport.playhead_s();
    assert!(playhead > 0.0 && playhead < DURATION_S, "{playhead}");

    let zoomed = Viewport::new(
        TimeRange::new(playhead - 0.01, playhead + 0.01),
        Amplitude::new(-3.0, 3.0),
        320.0,
    );
    let zoomed = lib
        .store
        .read(|conn| {
            let column = ColumnSource::open(conn, blob).unwrap();
            Ok(reduce::trace(&column, descriptor, &zoomed, TraceStyle::default()).unwrap())
        })
        .unwrap();
    assert!(!zoomed.geometry.is_empty());
    assert!(
        matches!(zoomed.geometry, TraceGeometry::Points(_)),
        "twenty milliseconds at 1024 Hz is twenty samples across 320 pixels"
    );

    // A pyramid is a cache over a run's output as much as over a library
    // column: dropping it leaves the library clean and the trace still drawn.
    let report = lib.store.read(verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");
    lib.store
        .write(|conn| sp_store::pyramid::clear_all(conn))
        .unwrap();
    let report = lib.store.read(verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");
}

// ---------------------------------------------------------------------------
// Content addressing, end to end
// ---------------------------------------------------------------------------

#[test]
fn the_same_spec_generated_twice_costs_one_column_and_no_second_run() {
    // Three separately designed mechanisms have to agree for this to hold:
    // generation is deterministic (G3), the blob store deduplicates by
    // content (§5.3), and a cache key addresses content rather than a group
    // (§9.5). Each is unit-tested on its own; this is the claim a user
    // actually feels — regenerate a ladder, re-run the pipeline, and the
    // library neither grows nor recomputes.
    let lib = Ladder::new();
    let registry = sp_dsp::registry().unwrap();
    let pipeline = measured(&lib.names());
    let id = save(&lib.store, &pipeline);
    let first = lib.run_saved(
        &registry,
        &pipeline,
        id,
        &[lib.group],
        &RunOptions::default(),
    );
    assert_eq!(first.stages_run, 3 + AMPLITUDES.len());

    let before = lib.store.read(library::summary).unwrap();
    let again = generate_ladder(&lib.store, "ladder again");
    let after = lib.store.read(library::summary).unwrap();

    assert_eq!(after.datasets, before.datasets + 1);
    assert_eq!(after.signals, before.signals + AMPLITUDES.len() as u64);
    assert_eq!(
        after.blobs, before.blobs,
        "the same samples are the same column"
    );
    assert_eq!(after.blob_bytes, before.blob_bytes);

    // The second dataset's group is a different group of the same content, so
    // every stage is a cache hit — including the artifacts and metrics, which
    // are copied forward with the samples.
    let second = lib.run_saved(
        &registry,
        &pipeline,
        id,
        &[again.group_id],
        &RunOptions::default(),
    );
    assert_eq!(second.status, RunStatus::Ok, "{}", second.describe());
    assert_eq!(second.stages_run, 0, "{}", second.describe());
    assert_eq!(second.stages_cached, 3 + AMPLITUDES.len());

    let stages = lib
        .store
        .read(move |conn| runs::group_stages(conn, second.run, again.group_id))
        .unwrap();
    assert!(stages.iter().all(|row| row.status == StageStatus::Cached));

    let cached: Statistics = lib.artifact_for(second.run, again.group_id, 2);
    let computed: Statistics = lib.artifact(first.run, 2);
    assert_eq!(cached.rms, computed.rms);
    assert_eq!(cached.names, computed.names);

    // And a run that is told not to trust the cache computes the same numbers
    // from scratch, which is what makes the cache safe to believe.
    let recomputed = lib.run_saved(
        &registry,
        &pipeline,
        id,
        &[again.group_id],
        &RunOptions::default().without_cache(),
    );
    assert_eq!(recomputed.stages_cached, 0);
    let recomputed: Statistics = lib.artifact_for(recomputed.run, again.group_id, 2);
    assert_eq!(recomputed.rms, computed.rms);
}

// ---------------------------------------------------------------------------
// A captured file, out and back in
// ---------------------------------------------------------------------------

fn sample_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample/sample.csv")
}

/// Imports a file into a fresh library and hands back both.
fn import_into(dir: &Path, name: &str, file: &Path) -> (Store, DatasetId) {
    let store = Store::open(dir.join(format!("{name}.db"))).unwrap();
    let request = ImportRequest::new(ImportProfile::default()).named(name);
    let file = file.to_path_buf();
    let report = store
        .write(move |conn| {
            Ok(import_file(conn, &file, &request, &ImportControl::new())
                .map_err(|error| error.to_string()))
        })
        .unwrap()
        .expect("the sample file should import");
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    (store, report.dataset_id)
}

/// Writes a dataset back out to `path`, as the Export command does.
fn export(store: &Store, dataset: DatasetId, path: &Path) {
    let out = path.to_path_buf();
    store
        .read(move |conn| {
            Ok(export_dataset_to_file(conn, dataset, &out).map_err(|error| error.to_string()))
        })
        .unwrap()
        .expect("an imported dataset exports");
}

/// Every pulse group of a dataset, read back as plain numbers: name, group
/// properties, field keys and units, times of arrival and field values.
type Captured = Vec<(
    Option<String>,
    Vec<(String, String)>,
    Vec<(String, Option<String>)>,
    Vec<f64>,
    Vec<Vec<f64>>,
)>;

fn capture(store: &Store, dataset: DatasetId) -> Captured {
    store
        .read(move |conn| {
            let mut out = Captured::new();
            for group in library::list_groups_in_dataset(conn, dataset)? {
                let count = group.actual_count as u64;
                let attributes = group
                    .attributes
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_string()))
                    .collect();
                let fields = pulses::list_fields(conn, group.id)?;
                let mut values = Vec::new();
                for field in &fields {
                    values.push(
                        pulses::read_field(
                            conn,
                            group.id,
                            field.ordinal,
                            SampleRange::first(count),
                        )?
                        .values()
                        .collect::<Vec<f64>>(),
                    );
                }
                out.push((
                    group.name.clone(),
                    attributes,
                    fields
                        .iter()
                        .map(|field| (field.key.clone(), field.unit.clone()))
                        .collect(),
                    pulses::read_toa(conn, group.id, SampleRange::first(count))?,
                    values,
                ));
            }
            Ok(out)
        })
        .unwrap()
}

#[test]
fn a_captured_file_re_imports_to_the_same_rows_it_exported_from() {
    // G1 is stated as a file-level promise and `sp-csv` holds the writer to it
    // text by text. This is the same loop judged where it matters to everything
    // downstream: the rows in the database. A run over a re-imported dataset
    // has to see exactly what a run over the original saw.
    let dir = tempfile::tempdir().unwrap();
    let (first, original) = import_into(dir.path(), "first", &sample_file());

    let exported = dir.path().join("exported.csv");
    export(&first, original, &exported);

    let (second, reimported) = import_into(dir.path(), "second", &exported);

    let before = capture(&first, original);
    let after = capture(&second, reimported);
    assert_eq!(before.len(), 2, "the sample file is two groups");
    assert_eq!(before, after, "the library should hold the same rows");

    // The whole library agrees, not only the rows that were compared.
    let left = first.read(library::summary).unwrap();
    let right = second.read(library::summary).unwrap();
    assert_eq!(
        (left.trains, left.groups, left.pulse_fields, left.blob_bytes),
        (
            right.trains,
            right.groups,
            right.pulse_fields,
            right.blob_bytes
        )
    );

    // Both libraries are exportable again, because the layout travelled with
    // the dataset rather than with the file it came from (§7.5).
    let twice = dir.path().join("twice.csv");
    export(&second, reimported, &twice);
    assert_eq!(
        std::fs::read_to_string(&exported).unwrap(),
        std::fs::read_to_string(&twice).unwrap(),
        "export is a fixed point through the database too"
    );

    for store in [&first, &second] {
        let report = store.read(verify::verify).unwrap();
        assert!(report.is_clean(), "{report:?}");
    }
}
