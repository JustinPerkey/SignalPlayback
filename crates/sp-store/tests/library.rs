//! M1 exit criteria (`docs/DESIGN.md` §16): signals insert and list, property
//! queries work, a column round-trips through chunked BLOB storage byte for
//! byte, and `Verify Library` passes.

use serde_json::json;
use sp_core::{
    Attributes, DType, Domain, FieldRange, PropKind, PropScope, PropertyDef, Provenance,
    SampleBuffer, SampleRange, Samples, Scaling, SourceKind, TimeUnit, Timebase,
};
use sp_store::blob::{self, BlobKind, BlobWriter, HEADER_LEN};
use sp_store::{
    library, props, pulses, pyramid, trains, verify, NewDataset, NewGroup, NewPulseField,
    NewPulseGroup, NewSignal, NewTrain, PropertyQuery, PulsePredicate, Store,
};

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    (dir, store)
}

fn sine(count: usize) -> Vec<f64> {
    (0..count)
        .map(|i| (i as f64 * 0.01).sin() * 100.0)
        .collect()
}

#[test]
fn signals_insert_and_list() {
    let (_dir, store) = open();

    let (dataset_id, group_id, ids) = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let dataset_id = library::insert_dataset(
                &tx,
                &NewDataset::new("capture 1", SourceKind::CsvImport).with_source_uri("c:/x.csv"),
            )?;
            let train_id =
                trains::insert_train(&tx, &NewTrain::new(dataset_id, 0).named("capture 1"))?;
            let group_id =
                library::insert_group(&tx, &NewGroup::new(train_id, 0, 2).named("ANT_A"))?;
            let mut ids = Vec::new();
            for (ordinal, name) in ["ch0", "ch1"].into_iter().enumerate() {
                let gain = ordinal as f64 + 1.0;
                let values: Vec<f64> = sine(1_000).into_iter().map(|v| v * gain / 2.0).collect();
                let samples = SampleBuffer::from_f64(DType::F32, &values);
                let mut attrs = Attributes::new();
                attrs.insert("prf_hz", 1_000.0 * (ordinal as f64 + 1.0));
                attrs.insert("coding", "nrz");
                let signal = NewSignal::new(
                    group_id,
                    ordinal as u32,
                    name,
                    Timebase::regular(48_000.0, 0.5),
                    samples,
                )
                .with_units("V")
                .with_domain(Domain::Analog)
                .with_provenance(Provenance::Imported)
                .with_attributes(attrs);
                ids.push(library::insert_signal(&tx, &signal)?);
            }
            tx.commit()?;
            Ok((dataset_id, group_id, ids))
        })
        .unwrap();

    store
        .read(|conn| {
            let datasets = library::list_datasets(conn)?;
            assert_eq!(datasets.len(), 1);
            assert_eq!(datasets[0].id, dataset_id);
            assert_eq!(datasets[0].name, "capture 1");
            assert_eq!(datasets[0].source_kind, SourceKind::CsvImport);
            assert_eq!(datasets[0].source_uri.as_deref(), Some("c:/x.csv"));

            // Groups hang off a train, not off the dataset (§6.6).
            let trains = trains::list_trains(conn, dataset_id)?;
            assert_eq!(trains.len(), 1);
            assert_eq!(trains[0].display_name(), "capture 1");

            let groups = library::list_groups(conn, trains[0].id)?;
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].id, group_id);
            assert_eq!(groups[0].train_id, trains[0].id);
            assert_eq!(groups[0].name.as_deref(), Some("ANT_A"));
            assert!(!groups[0].is_pulse_group());

            let signals = library::list_signals(conn, group_id)?;
            assert_eq!(signals.len(), 2);
            let s = &signals[1];
            assert_eq!(s.id, ids[1]);
            assert_eq!(s.name, "ch1");
            assert_eq!(s.units.as_deref(), Some("V"));
            assert_eq!(s.dtype, DType::F32);
            assert_eq!(s.sample_count, 1_000);
            assert_eq!(s.timebase, Timebase::regular(48_000.0, 0.5));
            assert_eq!(s.attributes.get_f64("prf_hz"), Some(2_000.0));
            let stats = s.stats.unwrap();
            assert_eq!(stats.count(), 1_000);
            assert!(stats.max().unwrap() <= 100.0);
            assert!(stats.min().unwrap() >= -100.0);

            let summary = library::summary(conn)?;
            assert_eq!(summary.datasets, 1);
            assert_eq!(summary.groups, 1);
            assert_eq!(summary.signals, 2);
            assert_eq!(summary.blobs, 2);

            // Full-text search reaches names and attribute text.
            assert_eq!(library::search_signals(conn, "ch1")?, vec![ids[1]]);
            assert_eq!(library::search_signals(conn, "nrz")?.len(), 2);
            assert!(library::search_signals(conn, "zzz")?.is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn samples_round_trip_byte_for_byte_across_chunks() {
    let (_dir, store) = open();
    // A chunk size that is not a multiple of any sample width, so chunk
    // boundaries split individual values and the header.
    let chunk = 1_001;

    let cases: Vec<(SampleBuffer, Timebase)> = vec![
        (
            SampleBuffer::from_f64(DType::F32, &sine(5_000)),
            Timebase::regular(1e6, 0.0),
        ),
        (
            SampleBuffer::from_f64(DType::F64, &sine(3_333)),
            Timebase::regular(10.0, -2.5),
        ),
        (
            SampleBuffer::new(Samples::I16(
                (0..4_000).map(|i| (i % 65_536) as i16).collect(),
            ))
            .with_scaling(Scaling::new(0.001, -1.0)),
            Timebase::regular(100.0, 0.0),
        ),
        (
            SampleBuffer::new(Samples::I32((0..2_000).map(|i| i * 1_000_003).collect())),
            Timebase::irregular(3.0),
        ),
        (
            SampleBuffer::new(Samples::U8((0..3_001).map(|i| (i % 251) as u8).collect())),
            Timebase::regular(8_000.0, 0.0),
        ),
        (
            SampleBuffer::new(Samples::C64(
                (0..1_500)
                    .map(|i| sp_core::C64::new(i as f32, -(i as f32)))
                    .collect(),
            )),
            Timebase::regular(1.0, 0.0),
        ),
        // Degenerate: an empty column is a header and nothing else.
        (
            SampleBuffer::new(Samples::F32(Vec::new())),
            Timebase::regular(1.0, 0.0),
        ),
    ];

    for (buffer, timebase) in cases {
        let expected = buffer.clone();
        let blob_id = store
            .write(move |conn| {
                let tx = conn.transaction()?;
                let id = blob::write_column(&tx, &buffer, timebase, chunk)?;
                tx.commit()?;
                Ok(id)
            })
            .unwrap();

        store
            .read(|conn| {
                let info = blob::info(conn, blob_id)?;
                assert_eq!(info.kind, BlobKind::Samples);
                assert_eq!(info.chunk_size, chunk);
                assert_eq!(
                    info.byte_len as usize,
                    HEADER_LEN + expected.len() * expected.dtype().size_bytes()
                );
                assert_eq!(info.chunk_count(), info.byte_len.div_ceil(chunk as u64));

                let header = blob::read_header(conn, blob_id)?;
                assert_eq!(header.dtype, expected.dtype());
                assert_eq!(header.count as usize, expected.len());
                assert_eq!(header.timebase, timebase);
                assert_eq!(header.scaling, expected.scaling());

                // Whole column, exactly.
                let back = blob::read_column(conn, blob_id, SampleRange::first(u64::MAX))?;
                assert_eq!(
                    blob::encode_samples(back.samples(), 0..back.len()),
                    blob::encode_samples(expected.samples(), 0..expected.len()),
                    "{:?} column differs after round trip",
                    expected.dtype()
                );
                assert_eq!(back.scaling(), expected.scaling());

                // A span straddling a chunk boundary.
                if expected.len() > 400 {
                    let span = blob::read_column(conn, blob_id, SampleRange::new(200, 400))?;
                    assert_eq!(span.len(), 200);
                    assert_eq!(
                        blob::encode_samples(span.samples(), 0..200),
                        blob::encode_samples(expected.samples(), 200..400)
                    );
                }

                // Out-of-range spans clamp rather than fail.
                let tail = blob::read_column(
                    conn,
                    blob_id,
                    SampleRange::new(expected.len() as u64 + 10, u64::MAX),
                )?;
                assert!(tail.is_empty());
                Ok(())
            })
            .unwrap();
    }

    let report = store.read(verify::verify).unwrap();
    // Blobs written directly have no referencing row yet, which verify
    // flags; nothing else may be wrong.
    assert!(report.checksum_mismatches.is_empty(), "{report:?}");
    assert!(report.length_mismatches.is_empty(), "{report:?}");
    assert_eq!(report.blobs_checked, 7);
}

#[test]
fn identical_columns_share_one_blob() {
    let (_dir, store) = open();
    let (a, b, c) = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let buffer = SampleBuffer::from_f64(DType::F32, &sine(10_000));
            let tb = Timebase::regular(1.0, 0.0);
            let a = blob::write_column(&tx, &buffer, tb, 4_096)?;
            let b = blob::write_column(&tx, &buffer, tb, 4_096)?;
            // Same samples, different timebase: different header, different bytes.
            let c = blob::write_column(&tx, &buffer, Timebase::regular(2.0, 0.0), 4_096)?;
            tx.commit()?;
            Ok((a, b, c))
        })
        .unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);

    store
        .read(|conn| {
            assert_eq!(blob::info(conn, a)?.refcount, 2);
            assert_eq!(blob::info(conn, c)?.refcount, 1);
            // The discarded duplicate left no pending row behind.
            assert_eq!(blob::list(conn)?.len(), 2);
            assert!(verify::verify(conn)?.pending.is_empty());
            Ok(())
        })
        .unwrap();

    store
        .write(move |conn| {
            blob::release(conn, a)?;
            assert_eq!(blob::info(conn, a)?.refcount, 1);
            blob::release(conn, a)?;
            assert!(blob::info(conn, a).is_err());
            let chunks: i64 =
                conn.query_row("SELECT COUNT(*) FROM sample_chunk", [], |r| r.get(0))?;
            let c_chunks = blob::info(conn, c)?.chunk_count() as i64;
            assert_eq!(chunks, c_chunks, "released blob's chunks are gone");
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_streamed_write_matches_a_single_write() {
    let (_dir, store) = open();
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let expected = payload.clone();
    let (whole, streamed) = store
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut w = BlobWriter::begin(&tx, BlobKind::Artifact, 3_000)?;
            w.write(&payload)?;
            let whole = w.finish()?;

            let mut w = BlobWriter::begin(&tx, BlobKind::Artifact, 3_000)?;
            for piece in payload.chunks(17) {
                w.write(piece)?;
            }
            let streamed = w.finish()?;
            tx.commit()?;
            Ok((whole, streamed))
        })
        .unwrap();
    assert_eq!(whole, streamed, "same bytes, same address");
    store
        .read(|conn| {
            let info = blob::info(conn, whole)?;
            assert_eq!(info.refcount, 2);
            assert_eq!(blob::read_bytes(conn, whole, 0, usize::MAX)?, expected);
            assert_eq!(
                blob::read_bytes(conn, whole, 2_999, 3)?,
                expected[2_999..3_002]
            );
            assert_eq!(blob::rehash(conn, &info)?, info.checksum);
            Ok(())
        })
        .unwrap();
}

#[test]
fn property_definitions_and_queries() {
    let (_dir, store) = open();

    let ids = store
        .write(|conn| {
            let tx = conn.transaction()?;
            props::insert_property_def(
                &tx,
                &PropertyDef::new(
                    "symbol_rate_hz",
                    PropScope::Signal,
                    PropKind::FreqHz {
                        min: None,
                        max: None,
                    },
                )
                .with_label("Symbol rate")
                .with_unit("Hz")
                .in_section("Timing"),
            )?;
            props::insert_property_def(
                &tx,
                &PropertyDef::new(
                    "coding",
                    PropScope::Signal,
                    PropKind::Enum {
                        variants: vec!["nrz".into(), "manchester".into()],
                    },
                )
                .with_default(json!("nrz")),
            )?;

            let dataset_id =
                library::insert_dataset(&tx, &NewDataset::new("gen", SourceKind::Generated))?;
            let train_id = trains::insert_train(&tx, &NewTrain::new(dataset_id, 0))?;
            let group_id = library::insert_group(&tx, &NewGroup::new(train_id, 0, 4))?;
            let mut ids = Vec::new();
            for i in 0..4u32 {
                let mut attrs = Attributes::new();
                attrs.insert("symbol_rate_hz", 250_000.0 * f64::from(i + 1));
                attrs.insert("coding", if i % 2 == 0 { "nrz" } else { "manchester" });
                attrs.insert("inverted", i == 3);
                let signal = NewSignal::new(
                    group_id,
                    i,
                    format!("sig{i}"),
                    Timebase::regular(1e6, 0.0),
                    SampleBuffer::from_f64(DType::F32, &sine(64)),
                )
                .with_domain(Domain::BasebandIq)
                .with_attributes(attrs);
                ids.push(library::insert_signal(&tx, &signal)?);
            }
            tx.commit()?;
            Ok(ids)
        })
        .unwrap();

    store
        .read(|conn| {
            let defs = props::list_property_defs(conn, Some(PropScope::Signal))?;
            assert_eq!(defs.len(), 2);
            let sr = props::get_property_def(conn, PropScope::Signal, "symbol_rate_hz")?.unwrap();
            assert_eq!(sr.label, "Symbol rate");
            assert_eq!(sr.unit.as_deref(), Some("Hz"));
            assert_eq!(sr.section.as_deref(), Some("Timing"));
            assert!(matches!(sr.kind, PropKind::FreqHz { .. }));
            let coding = props::get_property_def(conn, PropScope::Signal, "coding")?.unwrap();
            assert_eq!(coding.default, Some(json!("nrz")));
            assert!(props::get_property_def(conn, PropScope::Group, "coding")?.is_none());

            // "every signal with symbol_rate_hz above 600 k"
            let fast = props::find_signals(
                conn,
                "symbol_rate_hz",
                &PropertyQuery::Between {
                    min: Some(600_000.0),
                    max: None,
                },
            )?;
            assert_eq!(fast, vec![ids[2], ids[3]]);

            let manchester =
                props::find_signals(conn, "coding", &PropertyQuery::Equals("manchester".into()))?;
            assert_eq!(manchester, vec![ids[1], ids[3]]);

            let inverted = props::find_signals(
                conn,
                "inverted",
                &PropertyQuery::Between {
                    min: Some(1.0),
                    max: Some(1.0),
                },
            )?;
            assert_eq!(inverted, vec![ids[3]]);

            assert_eq!(
                props::find_signals(conn, "coding", &PropertyQuery::Exists)?.len(),
                4
            );
            let counts = props::attribute_key_counts(conn)?;
            assert_eq!(
                counts,
                vec![
                    ("coding".to_owned(), 4),
                    ("inverted".to_owned(), 4),
                    ("symbol_rate_hz".to_owned(), 4)
                ]
            );
            Ok(())
        })
        .unwrap();

    // Editing attributes refreshes the index; deleting a definition leaves
    // the values in place as unrecognised.
    let target = ids[0];
    store
        .write(move |conn| {
            let mut attrs = library::get_signal(conn, target)?.attributes;
            attrs.insert("symbol_rate_hz", 9_000_000.0);
            attrs.remove("coding");
            library::update_signal_attributes(conn, target, &attrs)?;
            assert!(props::delete_property_def(
                conn,
                PropScope::Signal,
                "coding"
            )?);
            Ok(())
        })
        .unwrap();
    store
        .read(|conn| {
            let fast = props::find_signals(
                conn,
                "symbol_rate_hz",
                &PropertyQuery::Between {
                    min: Some(5_000_000.0),
                    max: None,
                },
            )?;
            assert_eq!(fast, vec![target]);
            assert_eq!(
                props::find_signals(conn, "coding", &PropertyQuery::Exists)?.len(),
                3
            );
            let signal = library::get_signal(conn, ids[1])?;
            let defs = props::list_property_defs(conn, Some(PropScope::Signal))?;
            assert_eq!(
                signal.attributes.unrecognised(&defs),
                ["coding", "inverted"]
            );
            Ok(())
        })
        .unwrap();

    // Duplicate (scope, key) is refused; bad keys are refused.
    let err = store
        .write(|conn| {
            props::insert_property_def(
                conn,
                &PropertyDef::new("symbol_rate_hz", PropScope::Signal, PropKind::DurationS),
            )?;
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(err, sp_store::StoreError::Sqlite(_)));
    let err = store
        .write(|conn| {
            props::insert_property_def(
                conn,
                &PropertyDef::new("Bad Key", PropScope::Signal, PropKind::Bool),
            )?;
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(err, sp_store::StoreError::Invalid(_)));
}

#[test]
fn tags_attach_and_filter() {
    let (_dir, store) = open();
    let (a, b) = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let d = library::insert_dataset(&tx, &NewDataset::new("t", SourceKind::Generated))?;
            let t = trains::insert_train(&tx, &NewTrain::new(d, 0))?;
            let g = library::insert_group(&tx, &NewGroup::new(t, 0, 2))?;
            let mk = |i: u32| {
                NewSignal::new(
                    g,
                    i,
                    format!("s{i}"),
                    Timebase::regular(1.0, 0.0),
                    SampleBuffer::from_f64(DType::F32, &[f64::from(i)]),
                )
            };
            let a = library::insert_signal(&tx, &mk(0))?;
            let b = library::insert_signal(&tx, &mk(1))?;
            library::tag_signal(&tx, a, "golden")?;
            library::tag_signal(&tx, a, "golden")?; // idempotent
            library::tag_signal(&tx, a, "noisy")?;
            library::tag_signal(&tx, b, "noisy")?;
            tx.commit()?;
            Ok((a, b))
        })
        .unwrap();
    store
        .read(|conn| {
            assert_eq!(library::list_tags(conn)?, ["golden", "noisy"]);
            assert_eq!(library::signal_tags(conn, a)?, ["golden", "noisy"]);
            assert_eq!(library::signals_with_tag(conn, "noisy")?, vec![a, b]);
            assert_eq!(library::signals_with_tag(conn, "golden")?, vec![a]);
            Ok(())
        })
        .unwrap();
    // A filter over several tags narrows: both, not either.
    store
        .read(|conn| {
            assert_eq!(
                library::tag_counts(conn)?,
                vec![("golden".to_owned(), 1), ("noisy".to_owned(), 2)]
            );
            assert_eq!(
                library::signals_with_all_tags(conn, &["noisy".into()])?,
                vec![a, b]
            );
            assert_eq!(
                library::signals_with_all_tags(conn, &["noisy".into(), "golden".into()])?,
                vec![a]
            );
            assert!(library::signals_with_all_tags(conn, &[])?.is_empty());
            Ok(())
        })
        .unwrap();

    store
        .write(move |conn| library::untag_signal(conn, a, "golden"))
        .unwrap();
    store
        .read(|conn| {
            assert!(library::signals_with_tag(conn, "golden")?.is_empty());
            // The tag itself outlives its last use, so it can be reapplied.
            assert_eq!(library::tag_counts(conn)?[0], ("golden".to_owned(), 0));
            Ok(())
        })
        .unwrap();

    // Deleting the tag takes it out of the library for good.
    assert!(store
        .write(|conn| library::delete_tag(conn, "golden"))
        .unwrap());
    store
        .read(|conn| {
            assert_eq!(library::list_tags(conn)?, ["noisy"]);
            Ok(())
        })
        .unwrap();
}

#[test]
fn pulse_groups_store_columns_and_search_across_groups() {
    let (_dir, store) = open();

    // Mirrors sample/sample.csv: two groups of two pulses, TOA in µs.
    let (train_id, g1, g2) = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let dataset_id = library::insert_dataset(
                &tx,
                &NewDataset::new("sample.csv", SourceKind::CsvImport),
            )?;
            let unit = TimeUnit::Microseconds;
            // One file is one train; its two blocks are segments of it.
            let train_id = trains::insert_train(
                &tx,
                &NewTrain::new(dataset_id, 0)
                    .named("sample.csv")
                    .with_toa_unit(unit),
            )?;
            let mut attrs = Attributes::new();
            attrs.insert("total_time", 1000.0);
            attrs.insert("info", "info");
            let g1 = pulses::insert_pulse_group(
                &tx,
                &NewPulseGroup::new(
                    train_id,
                    0,
                    vec![unit.to_seconds(10.0), unit.to_seconds(20.0)],
                )
                .named("1")
                .with_field(NewPulseField::new("pulse width", vec![100.0, 100.0]))
                .with_field(NewPulseField::new("power", vec![100.0, 100.0]))
                .with_field(NewPulseField::new("angle", vec![100.0, 100.0]))
                .with_attributes(attrs.clone()),
            )?;
            // Second group varies so the search has something to discriminate.
            let g2 = pulses::insert_pulse_group(
                &tx,
                &NewPulseGroup::new(
                    train_id,
                    1,
                    vec![unit.to_seconds(30.0), unit.to_seconds(40.0)],
                )
                .named("2")
                .with_field(NewPulseField::new("pulse width", vec![1.5, 100.0]))
                .with_field(NewPulseField::new("power", vec![100.0, f64::NAN]))
                .with_field(NewPulseField::new("angle", vec![35.0, 100.0]))
                .with_attributes(attrs),
            )?;
            tx.commit()?;
            Ok((train_id, g1, g2))
        })
        .unwrap();

    store
        .read(|conn| {
            let groups = library::list_groups(conn, train_id)?;
            assert_eq!(groups.len(), 2);
            // Both blocks belong to the one capture, and the train knows how
            // much it holds without reading a column.
            assert_eq!(trains::train_extent(conn, train_id)?, (2, 4));
            assert!(trains::get_train(conn, train_id)?.is_pulse_train());
            assert!(groups[0].is_pulse_group());
            assert_eq!(groups[0].toa_unit, Some(TimeUnit::Microseconds));
            assert_eq!(groups[0].actual_count, 2);
            assert_eq!(groups[0].attributes.get_str("info"), Some("info"));

            let fields = pulses::list_fields(conn, g2)?;
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].name, "pulse width");
            assert_eq!(fields[0].key, "pulse_width");
            assert_eq!(fields[0].dtype, DType::F32);
            let power = fields[1].stats.unwrap();
            assert_eq!(power.count(), 1);
            assert_eq!(power.non_finite(), 1);
            assert_eq!(power.max(), Some(100.0));

            let toa = pulses::read_toa(conn, g2, SampleRange::first(10))?;
            assert_eq!(toa.len(), 2);
            assert!((toa[1] - 40e-6).abs() < 1e-15);
            let angle = pulses::read_field(conn, g2, 2, SampleRange::first(10))?;
            assert_eq!(angle.to_f64(), [35.0, 100.0]);

            // The vertical slice of pulse (g2, 0).
            let (toa, record) = pulses::read_record(conn, sp_core::PulseRef::new(g2, 0))?;
            assert!((toa - 30e-6).abs() < 1e-15);
            assert_eq!(record[0].1, 1.5);
            assert_eq!(record[2].1, 35.0);

            // G10: pulse_width < 2 AND angle BETWEEN 30 AND 40. Group 1's
            // zone map (pulse_width 100..100) eliminates it in SQL.
            let hits = pulses::search(
                conn,
                &[
                    PulsePredicate::new("pulse_width", FieldRange::at_most(2.0)),
                    PulsePredicate::new("angle", FieldRange::between(30.0, 40.0)),
                ],
            )?;
            assert_eq!(hits, vec![sp_core::PulseRef::new(g2, 0)]);

            // A NaN cell never matches, and an all-matching predicate returns
            // every pulse in every group.
            let powered = pulses::search(
                conn,
                &[PulsePredicate::new("power", FieldRange::at_least(50.0))],
            )?;
            assert_eq!(powered.len(), 3);
            assert!(!powered.contains(&sp_core::PulseRef::new(g2, 1)));
            assert!(powered.contains(&sp_core::PulseRef::new(g1, 1)));

            assert!(pulses::search(conn, &[]).is_err());
            assert!(pulses::search(
                conn,
                &[PulsePredicate::new("no_such_field", FieldRange::ANY)]
            )?
            .is_empty());

            let summary = library::summary(conn)?;
            assert_eq!(summary.pulse_fields, 6);
            // Group 1's three identical columns deduplicate to one blob.
            assert_eq!(summary.blobs, 2 + 1 + 3);
            Ok(())
        })
        .unwrap();
}

#[test]
fn verify_passes_on_a_healthy_library_and_catches_damage() {
    let (_dir, store) = open();
    let (dataset_id, signal_id) = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let d = library::insert_dataset(&tx, &NewDataset::new("v", SourceKind::Generated))?;
            let t = trains::insert_train(&tx, &NewTrain::new(d, 0))?;
            let g = library::insert_group(&tx, &NewGroup::new(t, 0, 1))?;
            let s = library::insert_signal_chunked(
                &tx,
                &NewSignal::new(
                    g,
                    0,
                    "s",
                    Timebase::regular(1.0, 0.0),
                    SampleBuffer::from_f64(DType::F32, &sine(5_000)),
                ),
                1_024,
            )?;
            let p = pulses::insert_pulse_group(
                &tx,
                &NewPulseGroup::new(t, 1, vec![0.0, 1.0, 2.0])
                    .with_field(NewPulseField::new("w", vec![1.0, 2.0, 3.0])),
            )?;
            assert_ne!(p, g);
            tx.commit()?;
            Ok((d, s))
        })
        .unwrap();

    let report = store.read(verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.blobs_checked, 3);

    // Flip a byte in the middle of the signal's column.
    store
        .write(move |conn| {
            let blob_id = library::signal_blob(conn, signal_id)?;
            let rowid: i64 = conn.query_row(
                "SELECT id FROM sample_chunk WHERE blob_id = ?1 AND ordinal = 2",
                [blob_id.get()],
                |r| r.get(0),
            )?;
            let mut blob =
                conn.blob_open(rusqlite::MAIN_DB, c"sample_chunk", c"data", rowid, false)?;
            let mut byte = [0u8; 1];
            blob.read_at_exact(&mut byte, 100)?;
            byte[0] ^= 0xFF;
            blob.write_at(&byte, 100)?;
            drop(blob);
            // And forge a refcount on it.
            conn.execute(
                "UPDATE sample_blob SET refcount = 5 WHERE id = ?1",
                [blob_id.get()],
            )?;
            Ok(())
        })
        .unwrap();

    let report = store.read(verify::verify).unwrap();
    assert!(!report.is_clean());
    assert_eq!(report.checksum_mismatches.len(), 1);
    assert_eq!(report.refcount_mismatches.len(), 1);
    assert_eq!(report.refcount_mismatches[0].1, 5);
    assert_eq!(report.refcount_mismatches[0].2, 1);
    assert_eq!(report.problem_count(), 2);

    // Repair fixes the reference count, never the bytes.
    let fixed = store
        .write(move |conn| verify::repair_references(conn, &report))
        .unwrap();
    assert_eq!(fixed, 1);
    let report = store.read(verify::verify).unwrap();
    assert_eq!(report.problem_count(), 1);
    assert_eq!(report.checksum_mismatches.len(), 1);

    // Deleting the dataset releases everything; verify is clean and empty.
    store
        .write(move |conn| {
            let tx = conn.transaction()?;
            library::delete_dataset(&tx, dataset_id)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    let report = store.read(verify::verify).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.blobs_checked, 0);
    assert_eq!(store.read(library::summary).unwrap(), Default::default());
}

#[test]
fn a_rolled_back_import_leaves_no_trace() {
    let (_dir, store) = open();
    let err = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let d = library::insert_dataset(&tx, &NewDataset::new("x", SourceKind::CsvImport))?;
            let t = trains::insert_train(&tx, &NewTrain::new(d, 0))?;
            let g = library::insert_group(&tx, &NewGroup::new(t, 0, 1))?;
            library::insert_signal(
                &tx,
                &NewSignal::new(
                    g,
                    0,
                    "s",
                    Timebase::regular(1.0, 0.0),
                    SampleBuffer::from_f64(DType::F32, &sine(100)),
                ),
            )?;
            // Simulate a mid-import failure: drop the transaction.
            Err::<(), _>(sp_store::StoreError::Invalid("count mismatch".into()))
        })
        .unwrap_err();
    assert!(matches!(err, sp_store::StoreError::Invalid(_)));
    let summary = store.read(library::summary).unwrap();
    assert_eq!(summary, Default::default());
    assert!(store.read(verify::verify).unwrap().is_clean());
}

/// The pyramid index owns one reference to the blob it points at, and gives it
/// back when the pyramid is dropped (§5.4). What the bytes *mean* is
/// `sp-engine`'s business; here they are just a blob.
#[test]
fn a_pyramid_holds_one_reference_to_its_blob() {
    let (_dir, store) = open();
    let (source, pyramid_blob) = store
        .write(|conn| {
            let tx = conn.transaction()?;
            let source = blob::write_column(
                &tx,
                &SampleBuffer::from_f64(DType::F32, &sine(1_000)),
                Timebase::regular(1_000.0, 0.0),
                blob::DEFAULT_CHUNK_SIZE,
            )?;
            let mut writer = BlobWriter::begin(&tx, BlobKind::Pyramid, blob::DEFAULT_CHUNK_SIZE)?;
            writer.write(b"not really a pyramid, but it is a blob")?;
            let pyramid_blob = writer.finish()?;
            tx.commit()?;
            Ok((source, pyramid_blob))
        })
        .unwrap();

    let checksum = store
        .read(move |conn| Ok(blob::info(conn, source)?.checksum))
        .unwrap();

    let row = store
        .write({
            let checksum = checksum.clone();
            move |conn| pyramid::link(conn, &checksum, pyramid_blob, 3, 6, 1_000)
        })
        .unwrap();
    assert_eq!(row.blob_id, pyramid_blob);
    assert_eq!(
        store
            .read(move |conn| Ok(blob::info(conn, pyramid_blob)?.refcount))
            .unwrap(),
        1,
        "the index takes over the writer's reference rather than adding one",
    );
    assert_eq!(store.read(pyramid::list).unwrap(), vec![row.clone()]);

    // Dropping the pyramid frees its blob and leaves the column alone.
    assert_eq!(
        store.write(move |conn| pyramid::clear_all(conn)).unwrap(),
        1
    );
    store
        .read(move |conn| {
            assert!(blob::info(conn, pyramid_blob).is_err());
            assert!(blob::info(conn, source).is_ok());
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_second_pyramid_for_the_same_column_is_refused_without_leaking() {
    let (_dir, store) = open();
    let checksum = "0".repeat(64);

    let make_blob = |bytes: &'static [u8]| {
        store
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut writer =
                    BlobWriter::begin(&tx, BlobKind::Pyramid, blob::DEFAULT_CHUNK_SIZE)?;
                writer.write(bytes)?;
                let id = writer.finish()?;
                tx.commit()?;
                Ok(id)
            })
            .unwrap()
    };

    let first = make_blob(b"first");
    let second = make_blob(b"second");
    let kept = store
        .write({
            let checksum = checksum.clone();
            move |conn| pyramid::link(conn, &checksum, first, 1, 6, 10)
        })
        .unwrap();
    let again = store
        .write({
            let checksum = checksum.clone();
            move |conn| pyramid::link(conn, &checksum, second, 1, 6, 10)
        })
        .unwrap();

    assert_eq!(again, kept, "the pyramid already filed wins");
    store
        .read(move |conn| {
            assert_eq!(blob::info(conn, first)?.refcount, 1);
            assert!(
                blob::info(conn, second).is_err(),
                "the losing build's blob is released, not leaked"
            );
            Ok(())
        })
        .unwrap();
}
