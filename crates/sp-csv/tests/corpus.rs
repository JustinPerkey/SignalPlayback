//! Golden-file tests against the fixture corpus (`docs/DESIGN.md` §14).
//!
//! M2's exit criterion is that the corpus imports: every well-formed shape
//! lands in the library with the groups, pulses and column types it should,
//! and every malformed one produces diagnostics rather than a panic or a
//! half-written dataset.

use std::path::{Path, PathBuf};

use sp_core::{DType, PropKind, PropScope, PropertyDef, SampleRange};
use sp_csv::profile::CountMode;
use sp_csv::{ingest, sniff_file, ColumnRule, ImportControl, ImportProfile, ImportRequest};
use sp_store::{library, props, pulses, Store};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    (dir, store)
}

/// Sniffs the fixture, applies the sniff's own proposals, and imports it.
fn import(store: &Store, name: &str, profile: ImportProfile) -> sp_csv::ImportReport {
    let path = fixture(name);
    let preview = sniff_file(&path, &profile).unwrap();
    let request = ImportRequest::new(preview.proposed(&profile));
    store
        .write(move |conn| {
            Ok(ingest::import_file(conn, &path, &request, &ImportControl::new()).unwrap())
        })
        .unwrap()
}

#[test]
fn the_sample_file_imports_as_two_groups_of_two_pulses() {
    let (_dir, store) = open();
    let report = import(&store, "sample.csv", ImportProfile::default());
    assert_eq!(report.groups, 2);
    assert_eq!(report.pulses, 4);
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);

    let (groups, fields, toa) = store
        .read(move |conn| {
            let groups = library::list_groups(conn, report.dataset_id)?;
            let fields = pulses::list_fields(conn, groups[0].id)?;
            let toa = pulses::read_toa(conn, groups[0].id, SampleRange::first(2))?;
            Ok((groups, fields, toa))
        })
        .unwrap();

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].name.as_deref(), Some("1"));
    assert_eq!(groups[0].declared_count, 2);
    assert_eq!(groups[0].actual_count, 2);
    assert!(groups[0].is_pulse_group());
    // Group-header columns other than the count become group properties.
    assert_eq!(groups[0].attributes.get_f64("total_time"), Some(1000.0));
    assert_eq!(groups[0].attributes.get_str("info"), Some("info"));

    assert_eq!(
        fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
        ["pulse_width", "power", "angle"]
    );
    assert_eq!(fields[0].stats.unwrap().min(), Some(100.0));

    // Microseconds on the way in, seconds on the library timeline.
    assert!((toa[0] - 10e-6).abs() < 1e-18);
    assert!((toa[1] - 20e-6).abs() < 1e-18);
}

#[test]
fn a_text_column_and_missing_cells_survive_the_import() {
    let (_dir, store) = open();
    let report = import(&store, "pulses.csv", ImportProfile::default());
    assert_eq!(report.groups, 3);
    assert_eq!(report.pulses, 5);
    // 'band' is text, so nothing about it is a diagnostic.
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);

    let (groups, fields, power) = store
        .read(move |conn| {
            let groups = library::list_groups(conn, report.dataset_id)?;
            let fields = pulses::list_fields(conn, groups[0].id)?;
            let power = pulses::read_field(conn, groups[0].id, 1, SampleRange::first(3))?;
            Ok((groups, fields, power))
        })
        .unwrap();

    assert_eq!(groups.len(), 3);
    // A group that declares no rows is still a group.
    assert_eq!(groups[2].actual_count, 0);

    let band = fields.iter().find(|f| f.key == "band").unwrap();
    assert_eq!(band.dtype, DType::I32, "a text column is stored as codes");
    assert_eq!(
        ingest::text_dictionary(&groups[0].attributes, "band"),
        Some(vec!["L".to_owned(), "S".to_owned()])
    );

    // The empty 'power' cell is a gap, and it is excluded from the statistics.
    assert!(power.value(2).unwrap().is_nan());
    let power_field = fields.iter().find(|f| f.key == "power").unwrap();
    assert_eq!(power_field.stats.unwrap().non_finite(), 1);
}

#[test]
fn a_different_delimiter_and_a_longer_preamble_are_profile_settings() {
    let (_dir, store) = open();
    let profile = ImportProfile {
        preamble_lines: 2,
        ..ImportProfile::default()
    };
    let report = import(&store, "semicolons.csv", profile);
    assert_eq!(report.layout.delimiter, ';');
    assert_eq!(report.layout.count_label(), "nrec");
    assert_eq!(report.layout.time_label(), "toa");
    assert_eq!(report.layout.preamble.len(), 2);
    assert_eq!(report.groups, 2);
    assert_eq!(report.pulses, 3);
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
}

#[test]
fn a_bom_crlf_file_and_a_lone_cr_file_both_read() {
    for (name, warnings) in [("bom_crlf.csv", 0), ("lone_cr.csv", 1)] {
        let (_dir, store) = open();
        let report = import(&store, name, ImportProfile::default());
        assert_eq!(report.groups, 1, "{name}");
        assert_eq!(report.pulses, 2, "{name}");
        assert_eq!(report.diagnostics.total(), warnings, "{name}");
        assert_eq!(report.layout.preamble, ["Skip Row"], "{name}");
    }
}

#[test]
fn comments_and_blank_lines_do_not_reach_the_library() {
    let (_dir, store) = open();
    let report = import(&store, "comments.csv", ImportProfile::default());
    assert_eq!(report.groups, 1);
    assert_eq!(report.pulses, 2);
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
}

#[test]
fn a_short_block_imports_with_the_counts_that_disagree_both_recorded() {
    let (_dir, store) = open();
    let report = import(&store, "short_count.csv", ImportProfile::default());
    assert_eq!(report.pulses, 2);
    assert_eq!(report.diagnostics.total(), 2);
    assert!(report
        .diagnostics
        .items()
        .iter()
        .any(|d| d.message.contains("declared 4")));

    let groups = store
        .read(move |conn| library::list_groups(conn, report.dataset_id))
        .unwrap();
    assert_eq!(groups[0].declared_count, 4);
    assert_eq!(groups[0].actual_count, 2);
    assert!(!groups[0].count_matches());
}

#[test]
fn strict_mode_refuses_the_file_tolerant_mode_imports() {
    let (_dir, store) = open();
    let path = fixture("short_count.csv");
    let request = ImportRequest::new(ImportProfile {
        mode: CountMode::Strict,
        ..ImportProfile::default()
    });

    let outcome: Result<(), String> = store
        .write(move |conn| {
            Ok(
                ingest::import_file(conn, &path, &request, &ImportControl::new())
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
            )
        })
        .unwrap();
    assert!(outcome.is_err());

    // Nothing was left behind: the transaction rolled back.
    let summary = store.read(library::summary).unwrap();
    assert_eq!(summary.datasets, 0);
    assert_eq!(summary.groups, 0);
    assert_eq!(summary.blobs, 0);
}

#[test]
fn unreadable_cells_are_diagnostics_with_a_position_to_jump_to() {
    let (_dir, store) = open();
    let report = import(&store, "bad_cells.csv", ImportProfile::default());
    assert_eq!(report.pulses, 3);
    assert_eq!(report.diagnostics.total(), 1, "{:?}", report.diagnostics);

    // 'oops' in the time column is caught; 'twenty' sits in a column the
    // sniff pass therefore reads as text.
    let diagnostic = &report.diagnostics.items()[0];
    assert_eq!(diagnostic.line, 6);
    assert_eq!(diagnostic.group_index, Some(0));
    assert!(diagnostic.message.contains("oops"));
    assert!(diagnostic.to_string().contains("line 6"));
}

#[test]
fn a_file_with_no_count_column_is_refused_before_anything_is_written() {
    let (_dir, store) = open();
    let path = fixture("no_count.csv");
    assert!(sniff_file(&path, &ImportProfile::default()).is_err());

    let request = ImportRequest::new(ImportProfile::default());
    let outcome: Result<(), String> = store
        .write(move |conn| {
            Ok(
                ingest::import_file(conn, &path, &request, &ImportControl::new())
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
            )
        })
        .unwrap();
    assert!(outcome.unwrap_err().contains("count column"));
    assert_eq!(store.read(library::summary).unwrap().datasets, 0);
}

#[test]
fn a_mapped_column_is_read_as_the_property_it_is_bound_to() {
    let (_dir, store) = open();
    store
        .write(|conn| {
            props::insert_property_def(
                conn,
                &PropertyDef::new(
                    "channel",
                    PropScope::Group,
                    PropKind::Int {
                        min: None,
                        max: None,
                    },
                )
                .with_label("Channel"),
            )
        })
        .unwrap();

    let mut profile = ImportProfile::default();
    profile.set_group_rule(ColumnRule::new("groupID").bound_to("channel"));
    let report = import(&store, "sample.csv", profile);

    let groups = store
        .read(move |conn| library::list_groups(conn, report.dataset_id))
        .unwrap();
    assert_eq!(groups[0].attributes.get_i64("channel"), Some(1));
    assert_eq!(groups[1].attributes.get_i64("channel"), Some(2));
    // The label is gone from the attributes: the binding renamed it.
    assert!(groups[0].attributes.get("groupid").is_none());
}

#[test]
fn a_saved_profile_can_be_reloaded_and_linked_to_what_it_imported() {
    let (_dir, store) = open();
    let mut profile = ImportProfile::named("Radar A");
    profile.preamble_lines = 2;
    profile.set_pulse_rule(ColumnRule::new("width").with_dtype(DType::F32));
    let json = profile.to_json().unwrap();

    let profile_id = store
        .write(move |conn| sp_store::profiles::save(conn, "Radar A", &json))
        .unwrap();

    let path = fixture("semicolons.csv");
    let saved = store
        .read(move |conn| sp_store::profiles::get(conn, profile_id))
        .unwrap();
    let reloaded = ImportProfile::from_json(&saved.rules_json).unwrap();
    assert_eq!(reloaded, profile);

    let request = ImportRequest::new(reloaded).with_profile_id(profile_id);
    let report = store
        .write(move |conn| {
            Ok(ingest::import_file(conn, &path, &request, &ImportControl::new()).unwrap())
        })
        .unwrap();

    let linked = store
        .read(move |conn| sp_store::profiles::dataset_profile(conn, report.dataset_id))
        .unwrap();
    assert_eq!(
        linked.map(|profile| profile.name),
        Some("Radar A".to_owned())
    );

    // Deleting the profile clears the link rather than refusing.
    store
        .write(move |conn| sp_store::profiles::delete(conn, profile_id))
        .unwrap();
    let linked = store
        .read(move |conn| sp_store::profiles::dataset_profile(conn, report.dataset_id))
        .unwrap();
    assert!(linked.is_none());
}

#[test]
fn cancelling_an_import_leaves_the_library_untouched() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let (_dir, store) = open();
    let flag = Arc::new(AtomicBool::new(true));
    let control = ImportControl::new().with_cancel(flag.clone());
    let path = fixture("sample.csv");
    let request = ImportRequest::new(ImportProfile::default());

    let outcome: Result<(), String> = store
        .write(move |conn| {
            Ok(ingest::import_file(conn, &path, &request, &control)
                .map(|_| ())
                .map_err(|error| error.to_string()))
        })
        .unwrap();
    assert_eq!(outcome.unwrap_err(), "the import was cancelled");

    let summary = store.read(library::summary).unwrap();
    assert_eq!(summary.datasets, 0);
    assert_eq!(summary.blobs, 0);
}

#[test]
fn the_whole_corpus_either_imports_or_explains_itself() {
    for name in [
        "sample.csv",
        "pulses.csv",
        "semicolons.csv",
        "short_count.csv",
        "bad_cells.csv",
        "no_count.csv",
        "comments.csv",
        "bom_crlf.csv",
        "lone_cr.csv",
    ] {
        let (_dir, store) = open();
        let path = fixture(name);
        let profile = ImportProfile {
            preamble_lines: if name == "semicolons.csv" { 2 } else { 1 },
            ..ImportProfile::default()
        };
        let profile = match sniff_file(&path, &profile) {
            Ok(preview) => preview.proposed(&profile),
            Err(error) => {
                // The only fixture that cannot be framed says why.
                assert_eq!(name, "no_count.csv", "{name}: {error}");
                continue;
            }
        };
        let request = ImportRequest::new(profile);
        let report = store
            .write(move |conn| {
                Ok(ingest::import_file(conn, &path, &request, &ImportControl::new()).unwrap())
            })
            .unwrap();
        assert!(report.groups > 0, "{name}");
        assert!(
            store.read(sp_store::verify::verify).unwrap().is_clean(),
            "{name}: the library is not clean after import"
        );
    }
}
