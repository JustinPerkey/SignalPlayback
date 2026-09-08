//! The M9 exit criterion: a sample DLL runs as a stage over one group at a
//! time, and the run records which build produced the result
//! (`docs/DESIGN.md` §16.1).
//!
//! The same library is exercised twice over. Linked into this binary it gives
//! fast, precise coverage of the marshalling; loaded from the `cdylib` that
//! `cargo build` produced it gives the thing the milestone actually claims —
//! a file on disk, opened at run time, driven through the scheduler and the
//! run tables like any other stage.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use sp_core::run::RunStatus;
use sp_core::time::SampleRange;
use sp_core::{DType, GroupId, PipelineId, SampleBuffer, SourceKind, Timebase};
use sp_ext::{AllowList, ExtLibrary, SymbolSource};
use sp_proc::pipeline::{Pipeline, PipelineStage};
use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions};
use sp_proc::{ParamSet, Stage, StageCtx, StageRegistry};
use sp_store::library::{NewDataset, NewGroup, NewSignal};
use sp_store::trains::NewTrain;
use sp_store::{library, runs, trains, Store};

/// The sample library as it is linked into this test binary.
#[derive(Debug)]
struct LinkedIn;

// SAFETY: the addresses are of `extern "C"` functions in this binary, which
// lives at least as long as anything that could call them.
unsafe impl SymbolSource for LinkedIn {
    fn symbol(&self, name: &str) -> Option<*const c_void> {
        sp_ext_sample::symbol(name)
    }
}

fn linked_in() -> Arc<ExtLibrary> {
    ExtLibrary::from_source(
        Box::new(LinkedIn),
        PathBuf::from("sp-ext-sample (linked in)"),
        String::new(),
    )
    .expect("the sample library should conform to the ABI it was written from")
}

/// The built `cdylib`, wherever cargo put it.
///
/// `cargo test --workspace` builds it, since it is a workspace member; running
/// this test alone may not have, and a missing file then means "not built",
/// not "broken" — so the DLL-backed tests say so and stop rather than failing
/// for a reason that is not about the code.
fn built_library() -> Option<PathBuf> {
    let name = format!(
        "{}sp_ext_sample{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    // .../target/<profile>/deps/<test binary>
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    for _ in 0..2 {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir.pop();
    }
    eprintln!("skipped: {name} has not been built; run `cargo test --workspace`");
    None
}

/// A one-signal group frame is enough for the marshalling tests; the run
/// tests below go through the store.
fn frame(values: &[f64]) -> sp_proc::GroupFrame {
    use sp_core::{Attributes, GroupMeta, RunId, TrainId};
    sp_proc::GroupFrame::new(
        GroupMeta {
            id: GroupId::new(1),
            train_id: TrainId::new(1),
            ordinal: 0,
            name: Some("dwell 0".to_owned()),
            toa_unit: None,
            attributes: Attributes::new(),
        },
        vec![sp_proc::SignalRef::in_memory(
            "rf",
            sp_core::Domain::Analog,
            Timebase::regular(1000.0, 0.0),
            SampleBuffer::from_f64(DType::F64, values),
            Attributes::new(),
        )],
        RunId::new(1),
    )
}

fn stage_of(library: &Arc<ExtLibrary>, params: ParamSet) -> Box<dyn Stage> {
    let mut registry = StageRegistry::new();
    sp_ext::register(&mut registry, Arc::clone(library)).unwrap();
    let mut stage = registry
        .create("ext.sample.gain")
        .expect("the loaded library's kind should be registered");
    stage.configure(&params).expect("valid parameters");
    stage
}

fn ctx() -> StageCtx {
    use sp_core::RunId;
    StageCtx::new(sp_proc::RunCtx::new(RunId::new(1), "hash", 0))
}

#[test]
fn a_loaded_library_declares_itself_as_a_stage_the_palette_can_list() {
    let library = linked_in();
    let descriptor = library.descriptor();
    assert_eq!(descriptor.kind, "ext.sample.gain");
    assert_eq!(descriptor.version, 1);
    assert_eq!(descriptor.label, "Sample gain (external)");
    assert_eq!(descriptor.params.len(), 2);
    assert_eq!(
        library.concurrency(),
        sp_ext::Concurrency::ThreadSafe,
        "the sample library declares itself thread-safe"
    );
}

#[test]
fn one_group_goes_out_and_comes_back_as_an_ordinary_stage_output() {
    let library = linked_in();
    let mut stage = stage_of(&library, ParamSet::new().with("gain", 3.0));
    let input = frame(&[1.0, -2.0, 4.0]);

    let output = stage.process(&ctx(), &input).unwrap();
    output
        .check_covers(&input)
        .expect("every input signal must be accounted for");

    let sp_proc::SignalOut::Replace { samples, .. } = &output.signals[0] else {
        panic!(
            "expected the input to be replaced, got {:?}",
            output.signals[0]
        );
    };
    assert_eq!(samples.values().collect::<Vec<_>>(), [3.0, -6.0, 12.0]);
    assert_eq!(output.metrics["gain"], 3.0);
    assert_eq!(output.metrics["peak_abs"], 12.0);
}

#[test]
fn a_library_may_add_signals_as_well_as_replace_them() {
    let library = linked_in();
    let mut stage = stage_of(
        &library,
        ParamSet::new().with("gain", 2.0).with("add_envelope", true),
    );
    let input = frame(&[1.0, -2.0]);

    let output = stage.process(&ctx(), &input).unwrap();
    output.check_covers(&input).unwrap();
    assert_eq!(output.signals.len(), 2, "one replacement and one addition");
    let sp_proc::SignalOut::Add { name, samples, .. } = &output.signals[1] else {
        panic!("expected an addition, got {:?}", output.signals[1]);
    };
    assert_eq!(name, "envelope 0");
    assert_eq!(samples.values().collect::<Vec<_>>(), [1.0, 2.0]);
}

#[test]
fn every_result_carries_the_build_that_produced_it() {
    // G8: a run says exactly which library made this output.
    let library = linked_in();
    let mut stage = stage_of(&library, ParamSet::new());
    let output = stage.process(&ctx(), &frame(&[1.0])).unwrap();
    let provenance = &output.diagnostics[0].message;
    assert!(provenance.contains("ext.sample.gain v1"), "{provenance}");
}

#[test]
fn one_instance_keeps_its_state_across_the_groups_it_sees() {
    // The handle is where per-instance state lives, and the host gives each
    // group's stage its own — so a counter inside the library counts the
    // groups this instance processed and no others.
    let library = linked_in();
    let mut stage = stage_of(&library, ParamSet::new());
    for expected in 1..=3 {
        let output = stage.process(&ctx(), &frame(&[1.0])).unwrap();
        assert_eq!(output.metrics["groups_seen"], f64::from(expected));
    }

    let mut fresh = stage_of(&library, ParamSet::new());
    let output = fresh.process(&ctx(), &frame(&[1.0])).unwrap();
    assert_eq!(output.metrics["groups_seen"], 1.0, "a new instance is new");
}

#[test]
fn a_parameter_the_schema_refuses_never_reaches_the_library() {
    let library = linked_in();
    let mut registry = StageRegistry::new();
    sp_ext::register(&mut registry, Arc::clone(&library)).unwrap();
    let mut stage = registry.create("ext.sample.gain").unwrap();

    let error = stage
        .configure(&ParamSet::new().with("gain", 5000.0))
        .unwrap_err();
    assert_eq!(error.parameter(), Some("gain"), "{error}");
}

#[test]
fn a_library_cannot_be_registered_twice_under_the_same_kind() {
    let mut registry = StageRegistry::new();
    sp_ext::register(&mut registry, linked_in()).unwrap();
    let error = sp_ext::register(&mut registry, linked_in()).unwrap_err();
    assert_eq!(
        error,
        sp_proc::RegistryError::Duplicate("ext.sample.gain".into())
    );
}

#[test]
fn a_library_on_disk_is_refused_until_it_is_allowed_and_then_loads() {
    let Some(path) = built_library() else { return };

    let refused = ExtLibrary::open(&path, &AllowList::new()).unwrap_err();
    assert!(
        matches!(refused, sp_ext::ExtError::NotAllowed(_)),
        "{refused}"
    );

    let library = ExtLibrary::open(&path, &AllowList::new().allowing(&path)).unwrap();
    assert_eq!(library.descriptor().kind, "ext.sample.gain");
    assert_eq!(
        library.hash().len(),
        64,
        "a loaded file is addressed by its BLAKE3 hash, which is what pins the build (G8)"
    );
    assert!(library.provenance().contains("blake3"));
}

#[test]
fn the_library_file_is_part_of_the_cache_key() {
    // Recompiling a DLL moves nothing the descriptor declares, so without the
    // file hash a stale cached output would be reused (§9.5).
    let Some(path) = built_library() else { return };
    let library = ExtLibrary::open(&path, &AllowList::unrestricted()).unwrap();
    let mut registry = StageRegistry::new();
    sp_ext::register(&mut registry, Arc::clone(&library)).unwrap();
    let stage = registry.create("ext.sample.gain").unwrap();

    assert_eq!(stage.cache_salt().as_deref(), Some(library.hash()));
    assert_eq!(
        linked_in().descriptor().kind,
        stage.descriptor().kind,
        "the same library, however it was loaded, is the same stage"
    );
}

/// A store holding one dataset of three groups with one signal each.
struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    groups: Vec<GroupId>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    let groups = store
        .write(|conn| {
            let dataset =
                library::insert_dataset(conn, &NewDataset::new("ext", SourceKind::Generated))?;
            let train = trains::insert_train(conn, &NewTrain::new(dataset, 0).named("capture"))?;
            let mut groups = Vec::new();
            for ordinal in 0..3u32 {
                let group = library::insert_group(
                    conn,
                    &NewGroup::new(train, ordinal, 0).named(format!("dwell {ordinal}")),
                )?;
                let values: Vec<f64> = (0..8)
                    .map(|i| f64::from(ordinal + 1) * f64::from(i))
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
                groups.push(group);
            }
            Ok(groups)
        })
        .unwrap();
    Fixture {
        _dir: dir,
        store,
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

#[test]
fn a_dll_runs_as_a_stage_over_one_group_at_a_time_and_every_output_is_persisted() {
    // This is the milestone in one test: a file on disk, allowed by the user,
    // loaded at run time, and driven by the ordinary scheduler.
    let Some(path) = built_library() else { return };
    let fixture = fixture();

    let library = ExtLibrary::open(&path, &AllowList::new().allowing(&path)).unwrap();
    let mut registry = sp_dsp::registry().unwrap();
    sp_ext::register(&mut registry, Arc::clone(&library)).unwrap();

    let pipeline = Pipeline::new("external")
        .with_stage(PipelineStage::new("dsp.util.passthrough").labelled("Raw"))
        .with_stage(PipelineStage::new("ext.sample.gain").with_param("gain", 4.0));
    let id = save(&fixture.store, &pipeline);

    let summary = run_pipeline(
        &fixture.store,
        &registry,
        id,
        &pipeline,
        &fixture.groups,
        &RunOptions::default(),
        &RunControl::new(),
    )
    .expect("the run itself should not fail");

    assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());
    assert_eq!(summary.groups_ok, 3);
    assert_eq!(summary.stages_run, 6, "two stages over three groups");

    for (at, &group) in fixture.groups.iter().enumerate() {
        let rows = fixture
            .store
            .read(|conn| runs::stage_signals(conn, summary.run, group, 1))
            .unwrap();
        assert_eq!(rows.len(), 1, "the external stage recorded its output");
        let samples: Vec<f64> = fixture
            .store
            .read(|conn| {
                runs::read_signal_samples(conn, &rows[0], SampleRange::first(rows[0].sample_count))
            })
            .unwrap()
            .values()
            .collect();
        let expected: Vec<f64> = (0..8)
            .map(|i| 4.0 * (at as f64 + 1.0) * f64::from(i))
            .collect();
        assert_eq!(samples, expected, "group {at} went through the library");

        let stages = fixture
            .store
            .read(|conn| runs::group_stages(conn, summary.run, group))
            .unwrap();
        let external = stages.iter().find(|s| s.stage_ordinal == 1).unwrap();
        assert_eq!(external.metrics["gain"], 4.0);
        assert_eq!(
            external.metrics["groups_seen"], 1.0,
            "each group gets its own instance, so each sees one group"
        );
        assert!(
            external.diagnostics[0].message.contains("sp_ext_sample"),
            "the recorded run names the library file (G8): {}",
            external.diagnostics[0].message
        );
        assert!(!external.cache_key.is_empty());
    }
}
