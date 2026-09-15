//! M12's exit criterion: a ladder produces one group per SNR rung,
//! deterministically (G3).
//!
//! > **Impairment ladders** — one clean source rendered at a sweep of SNRs or
//! > clock offsets, producing a group per rung, so an algorithm's degradation
//! > curve falls out of one run. (`docs/DESIGN.md` §8.4)
//!
//! The group is the unit a pipeline processes and asserts over, so the rung
//! has to be the group rather than a signal inside one. What that costs is one
//! layout flag on the request: the same sweep, written a different shape.

use sp_core::{PropScope, SampleRange};
use sp_gen::spec::Node;
use sp_gen::sweep::{ParamSweep, SweepLayout, SweepValues};
use sp_gen::{GenControl, GenRequest, GenSpec};
use sp_store::{library, props, Store};

/// A tone with an AWGN rung over it, at 8 kHz for a fifth of a second.
fn ladder_spec() -> GenSpec {
    GenSpec::new(
        8_000.0,
        0.2,
        Node::Awgn {
            input: Box::new(Node::Sine {
                freq_hz: 200.0,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            }),
            snr_db: 0.0,
        },
    )
    .with_seed(4_242)
}

/// The SNR rungs: 0 dB to 24 dB in 6 dB steps.
fn snr_sweep() -> ParamSweep {
    ParamSweep {
        pointer: sp_gen::ROOT.to_owned(),
        field: "snr_db".to_owned(),
        property_key: "snr_db".to_owned(),
        values: SweepValues::Range {
            start: 0.0,
            stop: 24.0,
            step: 6.0,
        },
    }
}

fn ladder_request() -> GenRequest {
    GenRequest::new(ladder_spec())
        .named("Impairment ladder")
        .with_signal_name("Tone")
        .with_sweep(snr_sweep())
        .with_layout(SweepLayout::GroupPerRung)
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("library.db")).unwrap();
    (dir, store)
}

/// Generates `request` into a fresh library and returns, per group, its name,
/// its `snr_db` attribute and the samples of the one signal it holds.
fn run(request: GenRequest) -> Vec<(String, Option<f64>, Vec<f64>)> {
    let (_dir, store) = store();
    let report = store
        .write(move |conn| Ok(sp_gen::generate(conn, &request, &GenControl::new())))
        .unwrap()
        .unwrap();

    let train_id = report.train_id;
    store
        .read(move |conn| {
            let mut out = Vec::new();
            for group in library::list_groups(conn, train_id)? {
                let signals = library::list_signals(conn, group.id)?;
                assert_eq!(signals.len(), 1, "a rung holds one signal");
                let count = signals[0].sample_count;
                let samples =
                    library::read_samples(conn, signals[0].id, SampleRange::first(count))?;
                out.push((
                    group.name.clone().unwrap_or_default(),
                    group.attributes.get_f64("snr_db"),
                    samples.to_f64(),
                ));
            }
            Ok(out)
        })
        .unwrap()
}

#[test]
fn a_ladder_produces_one_group_per_snr_rung() {
    let rungs = run(ladder_request());

    assert_eq!(rungs.len(), 5, "0, 6, 12, 18 and 24 dB");
    let values: Vec<_> = rungs.iter().map(|(_, snr, _)| *snr).collect();
    assert_eq!(
        values,
        [Some(0.0), Some(6.0), Some(12.0), Some(18.0), Some(24.0)],
        "every group carries its own rung"
    );
    let names: Vec<_> = rungs.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "snr_db 0",
            "snr_db 6",
            "snr_db 12",
            "snr_db 18",
            "snr_db 24"
        ],
        "a group is labelled by its rung, which is what the chart across groups reads"
    );
}

#[test]
fn a_ladder_is_the_same_ladder_the_second_time() {
    // G3, at the shape the milestone delivers: the spec plus the stored seed
    // regenerates bit-identical samples, rung by rung.
    let first = run(ladder_request());
    let second = run(ladder_request());

    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
        let (a_bits, b_bits): (Vec<u64>, Vec<u64>) = (
            a.2.iter().copied().map(f64::to_bits).collect(),
            b.2.iter().copied().map(f64::to_bits).collect(),
        );
        assert_eq!(a_bits, b_bits, "{} is not bit-identical", a.0);
    }
}

#[test]
fn every_rung_lands_at_the_noise_it_names() {
    let rungs = run(ladder_request());
    // The clean source, rendered on its own, is what each rung is a ratio of.
    let clean = sp_gen::render_values(
        &GenSpec::new(
            8_000.0,
            0.2,
            Node::Sine {
                freq_hz: 200.0,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            },
        ),
        SampleRange::first(1_600),
        &sp_gen::Sources::new(),
        &GenControl::new(),
    )
    .unwrap();
    let power = |values: &[f64]| values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64;

    for (name, snr_db, samples) in &rungs {
        let noise: Vec<f64> = samples
            .iter()
            .zip(&clean)
            .map(|(noisy, clean)| noisy - clean)
            .collect();
        let achieved = 10.0 * (power(&clean) / power(&noise)).log10();
        let asked = snr_db.expect("a rung carries its value");
        assert!(
            (achieved - asked).abs() < 1.0,
            "{name}: asked for {asked} dB, measured {achieved} dB"
        );
    }
}

#[test]
fn the_rung_is_a_declared_group_property_rather_than_a_loose_attribute() {
    let (_dir, store) = store();
    let request = ladder_request();
    store
        .write(move |conn| Ok(sp_gen::generate(conn, &request, &GenControl::new())))
        .unwrap()
        .unwrap();

    let defs = store
        .read(|conn| props::list_property_defs(conn, Some(PropScope::Group)))
        .unwrap();
    assert!(
        defs.iter().any(|def| def.key == "snr_db"),
        "the ladder declares its rung at group scope: {defs:?}"
    );
}

#[test]
fn the_same_sweep_without_the_layout_is_still_one_group() {
    // The ladder is a change to how a sweep writes, not to what it renders:
    // the default layout is the one M3 shipped, untouched.
    let request = GenRequest::new(ladder_spec())
        .named("One group")
        .with_signal_name("Tone")
        .with_sweep(snr_sweep());

    let (_dir, store) = store();
    let report = store
        .write(move |conn| Ok(sp_gen::generate(conn, &request, &GenControl::new())))
        .unwrap()
        .unwrap();
    assert_eq!(report.groups, 1);
    assert_eq!(report.signals, 5);

    let group_id = report.group_id;
    let signals = store
        .read(move |conn| library::list_signals(conn, group_id))
        .unwrap();
    assert_eq!(signals.len(), 5);
}
