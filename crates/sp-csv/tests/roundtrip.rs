//! Goal G1: import → database → export produces an equivalent file
//! (`docs/DESIGN.md` §2.1, §7.5).
//!
//! "Equivalent" is defined here, and deliberately: the preamble is verbatim,
//! the rows carry the same fields in the same order, a numeric cell is equal
//! as a *number* rather than as text (`1000` and `1000.0` are the same value,
//! and `NA` and an empty cell are both "missing"), and the delimiter run is
//! normalised because every field is trimmed on the way in (§7.3).
//!
//! On top of that, export is a fixed point: exporting, re-importing and
//! exporting again reproduces the same bytes.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use sp_csv::parse::{self, split_fields};
use sp_csv::sniff::format_cell;
use sp_csv::{export, ingest, sniff, sniff_file, ImportControl, ImportProfile, ImportRequest};
use sp_store::Store;

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

/// Imports `text` and writes it straight back out.
fn round_trip(text: &str, profile: &ImportProfile) -> String {
    let (_dir, store) = open();
    round_trip_in(&store, text, profile)
}

fn round_trip_in(store: &Store, text: &str, profile: &ImportProfile) -> String {
    let preview = sniff(text.as_bytes(), profile).unwrap();
    let request = ImportRequest::new(preview.proposed(profile)).named("round trip");
    let owned = text.to_owned();
    let dataset_id = store
        .write(move |conn| {
            Ok(
                ingest::import_reader(conn, owned.as_bytes(), &request, &ImportControl::new())
                    .unwrap()
                    .dataset_id,
            )
        })
        .unwrap();

    store
        .read(move |conn| {
            let mut out = Vec::new();
            export::export_dataset(conn, dataset_id, &mut out).unwrap();
            Ok(String::from_utf8(out).unwrap())
        })
        .unwrap()
}

/// Splits a file into its preamble lines and its rows of fields.
fn rows(text: &str, preamble_lines: usize, delimiter: char) -> (Vec<String>, Vec<Vec<String>>) {
    let lines: Vec<&str> = text.lines().collect();
    let preamble = lines
        .iter()
        .take(preamble_lines)
        .map(|line| (*line).to_owned())
        .collect();
    let rows = lines
        .iter()
        .skip(preamble_lines)
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .map(|line| {
            split_fields(line, delimiter)
                .into_iter()
                .map(|field| field.text)
                .collect()
        })
        .collect();
    (preamble, rows)
}

/// Two cells mean the same thing: the same text, the same number, or both
/// missing.
fn equivalent(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    if parse::is_missing(left) && parse::is_missing(right) {
        return true;
    }
    match (parse::parse_number(left), parse::parse_number(right)) {
        (Some(a), Some(b)) => a == b || (a.is_nan() && b.is_nan()),
        _ => false,
    }
}

fn assert_equivalent(original: &str, exported: &str, preamble_lines: usize, delimiter: char) {
    let (before_preamble, before) = rows(original, preamble_lines, delimiter);
    let (after_preamble, after) = rows(exported, preamble_lines, delimiter);

    assert_eq!(
        before_preamble, after_preamble,
        "the preamble is reproduced verbatim"
    );
    assert_eq!(before.len(), after.len(), "same number of rows");
    for (index, (before, after)) in before.iter().zip(&after).enumerate() {
        assert_eq!(before.len(), after.len(), "row {index} has the same arity");
        for (column, (left, right)) in before.iter().zip(after).enumerate() {
            assert!(
                equivalent(left, right),
                "row {index}, column {column}: '{left}' became '{right}'"
            );
        }
    }
}

#[test]
fn the_sample_file_round_trips() {
    let text = std::fs::read_to_string(fixture("sample.csv")).unwrap();
    let exported = round_trip(&text, &ImportProfile::default());
    assert_equivalent(&text, &exported, 1, ',');
}

#[test]
fn a_file_with_a_text_column_and_missing_cells_round_trips() {
    let text = std::fs::read_to_string(fixture("pulses.csv")).unwrap();
    let exported = round_trip(&text, &ImportProfile::default());
    assert_equivalent(&text, &exported, 1, ',');
    // The text column comes back spelt as it was written, not as a code.
    assert!(exported.contains("359.75,X"), "{exported}");
}

#[test]
fn the_delimiter_and_preamble_of_a_different_shape_round_trip() {
    let text = std::fs::read_to_string(fixture("semicolons.csv")).unwrap();
    let profile = ImportProfile {
        preamble_lines: 2,
        ..ImportProfile::default()
    };
    let exported = round_trip(&text, &profile);
    assert_equivalent(&text, &exported, 2, ';');
    assert!(exported.starts_with("# instrument export\n# revision 4\n"));
}

#[test]
fn line_endings_and_a_byte_order_mark_do_not_survive_but_the_data_does() {
    for name in ["bom_crlf.csv", "lone_cr.csv"] {
        let bytes = std::fs::read(fixture(name)).unwrap();
        let text = String::from_utf8_lossy(&bytes).replace('\u{feff}', "");
        let exported = round_trip(&text, &ImportProfile::default());
        // The reader normalises every ending to LF; the fields are unchanged.
        let expected = text.replace("\r\n", "\n").replace('\r', "\n");
        assert_equivalent(&expected, &exported, 1, ',');
        assert!(!exported.contains('\r'), "{name}");
    }
}

#[test]
fn export_is_a_fixed_point() {
    for (name, preamble) in [("sample.csv", 1), ("pulses.csv", 1), ("semicolons.csv", 2)] {
        let text = std::fs::read_to_string(fixture(name)).unwrap();
        let profile = ImportProfile {
            preamble_lines: preamble,
            ..ImportProfile::default()
        };
        let once = round_trip(&text, &profile);
        let twice = round_trip(&once, &profile);
        assert_eq!(once, twice, "{name}: exporting twice is not stable");
    }
}

#[test]
fn a_dataset_that_was_never_imported_cannot_be_written_back() {
    use sp_core::SourceKind;
    use sp_store::{library, NewDataset};

    let (_dir, store) = open();
    let dataset_id = store
        .write(|conn| {
            library::insert_dataset(conn, &NewDataset::new("generated", SourceKind::Generated))
        })
        .unwrap();

    let outcome: Result<(), String> = store
        .read(move |conn| {
            let mut out = Vec::new();
            Ok(export::export_dataset(conn, dataset_id, &mut out)
                .map_err(|error| error.to_string()))
        })
        .unwrap();
    assert!(outcome
        .unwrap_err()
        .contains("not imported from a CSV file"));
}

#[test]
fn a_file_on_disk_round_trips_through_a_file_on_disk() {
    let (dir, store) = open();
    let source = fixture("sample.csv");
    let request = ImportRequest::new(ImportProfile::default());
    let dataset_id = store
        .write({
            let source = source.clone();
            move |conn| {
                Ok(
                    ingest::import_file(conn, &source, &request, &ImportControl::new())
                        .unwrap()
                        .dataset_id,
                )
            }
        })
        .unwrap();

    let out = dir.path().join("exported.csv");
    store
        .read({
            let out = out.clone();
            move |conn| {
                export::export_dataset_to_file(conn, dataset_id, &out).unwrap();
                Ok(())
            }
        })
        .unwrap();

    let original = std::fs::read_to_string(&source).unwrap();
    let exported = std::fs::read_to_string(&out).unwrap();
    assert_equivalent(&original, &exported, 1, ',');

    // And the exported file sniffs as the same shape it came from.
    let preview = sniff_file(&out, &ImportProfile::default()).unwrap();
    assert_eq!(preview.layout.count_label(), "count");
    assert_eq!(preview.layout.time_label(), "time");
}

// ---------------------------------------------------------------------------
// The property test (G1)
// ---------------------------------------------------------------------------

/// A generated file, written in the exact form export produces, so a passing
/// round trip is byte equality rather than a judgement call.
#[derive(Debug, Clone)]
struct Generated {
    text: String,
    preamble_lines: usize,
}

/// Cells the generator draws from: whole numbers, decimals that survive `f64`
/// exactly as written, negatives, and gaps.
fn cell() -> impl Strategy<Value = String> {
    prop_oneof![
        (-1000i64..1000).prop_map(|v| format_cell(v as f64)),
        (-100_000i64..100_000).prop_map(|v| format_cell(v as f64 / 100.0)),
        Just(String::new()),
    ]
}

fn label(prefix: &'static str) -> impl Strategy<Value = String> {
    (0usize..4).prop_map(move |i| format!("{prefix}{i}"))
}

prop_compose! {
    fn generated_file()(
        preamble in prop::collection::vec("[a-zA-Z ][a-zA-Z0-9 ]{0,20}", 1..3),
        extra_group_labels in prop::collection::vec(label("meta"), 0..3),
        value_labels in prop::collection::vec(label("value"), 1..4),
        groups in prop::collection::vec(
            (0u32..1000, prop::collection::vec(prop::collection::vec(cell(), 1..4), 0..6)),
            1..4,
        ),
    ) -> Generated {
        // Labels must be distinct, or two columns would share a key.
        let mut group_header = vec!["gid".to_owned(), "count".to_owned()];
        for (i, _) in extra_group_labels.iter().enumerate() {
            group_header.push(format!("meta{i}"));
        }
        let width = value_labels.len();
        let mut pulse_header = vec!["time".to_owned()];
        for i in 0..width {
            pulse_header.push(format!("value{i}"));
        }

        let mut lines: Vec<String> = preamble.clone();
        lines.push(group_header.join(","));
        lines.push(pulse_header.join(","));

        for (index, (gid, rows)) in groups.iter().enumerate() {
            let mut row = vec![gid.to_string(), rows.len().to_string()];
            for i in 0..group_header.len() - 2 {
                row.push(format!("m{index}_{i}"));
            }
            lines.push(row.join(","));

            for (n, cells) in rows.iter().enumerate() {
                // Times of arrival are whole microseconds, strictly increasing.
                let mut row = vec![((index * 1000) + (n + 1) * 10).to_string()];
                for i in 0..width {
                    row.push(cells.get(i).cloned().unwrap_or_default());
                }
                lines.push(row.join(","));
            }
        }

        Generated {
            text: lines.join("\n") + "\n",
            preamble_lines: preamble.len(),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        // Persisting seeds would write into the source tree on every failing
        // run; the minimal input is printed either way.
        failure_persistence: None,
        ..ProptestConfig::with_cases(64)
    })]

    /// G1: a file written in canonical form comes back byte for byte.
    #[test]
    fn any_well_formed_file_round_trips(file in generated_file()) {
        let profile = ImportProfile {
            preamble_lines: file.preamble_lines,
            ..ImportProfile::default()
        };
        let exported = round_trip(&file.text, &profile);
        prop_assert_eq!(&exported, &file.text);
    }

    /// And it stays that way on the second pass, whatever the file shape.
    #[test]
    fn export_of_any_file_is_a_fixed_point(file in generated_file()) {
        let profile = ImportProfile {
            preamble_lines: file.preamble_lines,
            ..ImportProfile::default()
        };
        let once = round_trip(&file.text, &profile);
        let twice = round_trip(&once, &profile);
        prop_assert_eq!(once, twice);
    }
}
