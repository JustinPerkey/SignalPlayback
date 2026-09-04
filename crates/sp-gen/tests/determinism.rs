//! Goal G3: generated signals are reproducible.
//!
//! > The parameter spec plus a stored seed regenerates bit-identical samples.
//!
//! And the property that makes parallel rendering safe (`docs/DESIGN.md` §8.2):
//! a range rendered in chunks matches the same range rendered whole, so the
//! renderer may split a window across cores however it likes.

use proptest::prelude::*;
use sp_core::SampleRange;
use sp_gen::spec::{ConcatPart, EnvelopeSpec, ModKind, Node, NoiseKind, Sweep};
use sp_gen::{GenControl, GenSpec, Sources};

/// A fixed grid, so every generated spec is comparable and every frequency
/// below can be kept under Nyquist without the strategy having to know the
/// rate.
const RATE_HZ: f64 = 8_000.0;
const NYQUIST_HZ: f64 = RATE_HZ / 2.0;
const DURATION_S: f64 = 0.05;

fn samples(spec: &GenSpec, range: SampleRange) -> Vec<u64> {
    sp_gen::render_values(spec, range, &Sources::new(), &GenControl::new())
        .expect("the strategy only builds valid specs")
        .into_iter()
        // Compare bit patterns: "bit-identical" is the claim, and it is the
        // only comparison a NaN cannot slip through.
        .map(f64::to_bits)
        .collect()
}

fn whole(spec: &GenSpec) -> Vec<u64> {
    samples(spec, SampleRange::first(spec.sample_count()))
}

/// Frequencies well under Nyquist. A `Resample` lowers its child's Nyquist,
/// and the strategy nests them, so the headroom is what keeps most generated
/// trees valid; `spec()` filters out the few that still are not.
fn freq() -> impl Strategy<Value = f64> {
    1.0..NYQUIST_HZ / 3.0
}

fn amp() -> impl Strategy<Value = f64> {
    0.1..4.0f64
}

fn primitive() -> impl Strategy<Value = Node> {
    prop_oneof![
        (freq(), amp(), -3.0..3.0f64, -1.0..1.0f64).prop_map(
            |(freq_hz, amp, phase_rad, offset)| {
                Node::Sine {
                    freq_hz,
                    amp,
                    phase_rad,
                    offset,
                }
            }
        ),
        (freq(), amp(), 0.05..0.95f64, -3.0..3.0f64).prop_map(|(freq_hz, amp, duty, phase_rad)| {
            Node::Square {
                freq_hz,
                amp,
                duty,
                phase_rad,
            }
        }),
        (freq(), amp(), -3.0..3.0f64).prop_map(|(freq_hz, amp, phase_rad)| Node::Triangle {
            freq_hz,
            amp,
            phase_rad,
        }),
        (freq(), amp(), any::<bool>()).prop_map(|(freq_hz, amp, rising)| Node::Sawtooth {
            freq_hz,
            amp,
            rising,
        }),
        (0.002..0.02f64, 0.0001..0.001f64, amp()).prop_map(|(period_s, width_s, amp)| {
            Node::Pulse {
                period_s,
                width_s,
                amp,
                rise_s: 0.0,
                fall_s: 0.0,
            }
        }),
        (freq(), freq(), amp(), 0usize..3).prop_map(|(f0_hz, f1_hz, amp, sweep)| Node::Chirp {
            f0_hz,
            f1_hz,
            sweep: Sweep::ALL[sweep],
            amp,
        }),
        (-2.0..2.0f64).prop_map(|level| Node::Dc { level }),
        (-2.0..2.0f64, -2.0..2.0f64).prop_map(|(start, end)| Node::Ramp { start, end }),
        (0.0..DURATION_S, -1.0..1.0f64, -1.0..1.0f64).prop_map(|(at_s, before, after)| {
            Node::Step {
                at_s,
                before,
                after,
            }
        }),
        (0.0..DURATION_S, amp()).prop_map(|(at_s, amp)| Node::Impulse { at_s, amp }),
        (0usize..4, amp()).prop_map(|(kind, amp)| Node::Noise {
            kind: NoiseKind::ALL[kind],
            amp,
        }),
        (2u8..24, amp()).prop_map(|(order, amp)| Node::Prbs {
            order,
            taps: None,
            amp,
        }),
    ]
}

/// A tree up to four levels deep, so the strategy exercises the combinators
/// that change their children's grid.
fn node() -> impl Strategy<Value = Node> {
    primitive().prop_recursive(4, 24, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..3).prop_map(|terms| Node::Sum { terms }),
            prop::collection::vec(inner.clone(), 1..3).prop_map(|terms| Node::Product { terms }),
            prop::collection::vec((inner.clone(), 0.005..0.03f64), 1..3).prop_map(|parts| {
                Node::Concat {
                    parts: parts
                        .into_iter()
                        .map(|(node, duration_s)| ConcatPart { node, duration_s })
                        .collect(),
                }
            }),
            (inner.clone(), -3.0..3.0f64).prop_map(|(input, factor)| Node::Gain {
                input: Box::new(input),
                factor,
            }),
            (inner.clone(), 0.0..0.02f64).prop_map(|(input, by_s)| Node::Delay {
                input: Box::new(input),
                by_s,
            }),
            (inner.clone(), -2.0..0.0f64, 0.0..2.0f64).prop_map(|(input, lo, hi)| Node::Clip {
                input: Box::new(input),
                lo,
                hi,
            }),
            (inner.clone(), 0.0..1.0f64).prop_map(|(input, alpha)| Node::Envelope {
                input: Box::new(input),
                env: EnvelopeSpec::Tukey { alpha },
            }),
            (inner.clone(), RATE_HZ * 0.75..RATE_HZ).prop_map(|(input, to_rate_hz)| {
                Node::Resample {
                    input: Box::new(input),
                    to_rate_hz,
                }
            }),
            // FM and PM need an oscillator carrier and, for FM, an integrable
            // modulator; validation rejects anything else, so the strategy
            // only builds the shapes that are legal.
            (freq(), amp(), inner, 0usize..3, 1.0..500.0f64).prop_map(
                |(freq_hz, amp, modulator, kind, depth)| {
                    let integrable = modulator.is_integrable();
                    Node::Modulate {
                        carrier: Box::new(Node::Sine {
                            freq_hz,
                            amp,
                            phase_rad: 0.0,
                            offset: 0.0,
                        }),
                        modulator: Box::new(modulator),
                        kind: match kind {
                            0 => ModKind::Am {
                                depth: depth / 500.0,
                            },
                            1 if integrable => ModKind::Fm { dev_hz: depth },
                            _ => ModKind::Pm {
                                dev_rad: depth / 100.0,
                            },
                        },
                    }
                }
            ),
        ]
    })
}

fn spec() -> impl Strategy<Value = GenSpec> {
    (node(), any::<u64>())
        .prop_map(|(root, seed)| GenSpec::new(RATE_HZ, DURATION_S, root).with_seed(seed))
        // Deeply nested resamples can still drive a child's Nyquist below one
        // of the strategy's frequencies. Those specs are legitimately invalid
        // and the renderer refuses them, so they are no use here.
        .prop_filter("the spec must be renderable", |spec| {
            !sp_gen::validate(spec).blocks()
        })
}

/// Cut points inside a 400-sample render.
fn cuts() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(1u64..400, 0..5).prop_map(|mut cuts| {
        cuts.sort_unstable();
        cuts.dedup();
        cuts
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        // Persisting seeds would write into the source tree on every failing
        // run; the minimal input is printed either way.
        failure_persistence: None,
        ..ProptestConfig::with_cases(64)
    })]

    /// G3: the same spec and seed render the same bits, every time.
    #[test]
    fn a_spec_renders_the_same_bits_twice(spec in spec()) {
        prop_assert_eq!(whole(&spec), whole(&spec));
    }

    /// G3: the spec is the whole truth, so a round trip through JSON — which
    /// is how `signal.gen_spec` and a preset file store it — changes nothing.
    #[test]
    fn a_spec_survives_json_and_renders_the_same_bits(spec in spec()) {
        let json = spec.to_json().expect("serialises");
        let restored = GenSpec::from_json(&json).expect("deserialises");
        prop_assert_eq!(&restored, &spec);
        prop_assert_eq!(whole(&restored), whole(&spec));
    }

    /// §8.2: a range rendered in chunks matches the range rendered whole, so
    /// rayon may split a window however it likes.
    #[test]
    fn chunks_join_up_into_the_whole_render(spec in spec(), cuts in cuts()) {
        let count = spec.sample_count();
        let expected = whole(&spec);

        let mut joined = Vec::with_capacity(expected.len());
        let mut start = 0;
        for cut in cuts.into_iter().chain(std::iter::once(count)) {
            let cut = cut.min(count);
            if cut <= start {
                continue;
            }
            joined.extend(samples(&spec, SampleRange::new(start, cut)));
            start = cut;
        }
        prop_assert_eq!(joined, expected);
    }

    /// §8.2: a noise node derives its stream from the seed, so changing the
    /// seed changes the samples and nothing else does.
    #[test]
    fn the_seed_is_what_moves_a_noise_stream(seed in any::<u64>(), kind in 0usize..4) {
        let noisy = |seed| {
            GenSpec::new(
                RATE_HZ,
                DURATION_S,
                Node::Noise { kind: NoiseKind::ALL[kind], amp: 1.0 },
            )
            .with_seed(seed)
        };
        prop_assert_eq!(whole(&noisy(seed)), whole(&noisy(seed)));
        prop_assert_ne!(whole(&noisy(seed)), whole(&noisy(seed.wrapping_add(1))));
    }
}

/// §8.2: "adding a node elsewhere in the tree does not perturb an existing
/// node's noise", because a stream is keyed by the node's path.
#[test]
fn adding_a_sibling_leaves_an_existing_noise_stream_alone() {
    let noise = || Node::Noise {
        kind: NoiseKind::Pink,
        amp: 1.0,
    };
    let before = GenSpec::new(
        RATE_HZ,
        DURATION_S,
        Node::Sum {
            terms: vec![noise()],
        },
    )
    .with_seed(11);
    let after = GenSpec::new(
        RATE_HZ,
        DURATION_S,
        Node::Sum {
            terms: vec![noise(), Node::Dc { level: 1.0 }],
        },
    )
    .with_seed(11);

    let before = sp_gen::render_values(
        &before,
        SampleRange::first(before.sample_count()),
        &Sources::new(),
        &GenControl::new(),
    )
    .unwrap();
    let after = sp_gen::render_values(
        &after,
        SampleRange::first(after.sample_count()),
        &Sources::new(),
        &GenControl::new(),
    )
    .unwrap();

    // The DC term is a known constant, so subtracting it recovers the noise.
    // The comparison is approximate on purpose: adding and removing 1.0 moves
    // the last bit of a sum, which is arithmetic, not a different stream. A
    // perturbed stream would differ by order one.
    for (index, (a, b)) in before.iter().zip(&after).enumerate() {
        assert!(
            (a - (b - 1.0)).abs() < 1e-12,
            "sample {index}: {a} vs {}",
            b - 1.0
        );
    }
}

/// A noise node's stream follows its position, so the same node in two places
/// draws different numbers — which is what keeps two noise terms in one sum
/// from being perfectly correlated.
#[test]
fn two_noise_nodes_in_one_tree_draw_different_streams() {
    let noise = || Node::Noise {
        kind: NoiseKind::Gaussian,
        amp: 1.0,
    };
    let spec = GenSpec::new(
        RATE_HZ,
        DURATION_S,
        Node::Sum {
            terms: vec![
                noise(),
                Node::Gain {
                    input: Box::new(noise()),
                    factor: -1.0,
                },
            ],
        },
    )
    .with_seed(3);
    // Identical streams would cancel to silence.
    let values = sp_gen::render_values(
        &spec,
        SampleRange::first(spec.sample_count()),
        &Sources::new(),
        &GenControl::new(),
    )
    .unwrap();
    assert!(
        values.iter().any(|v| v.abs() > 0.1),
        "the two streams cancelled"
    );
}

/// The renderer splits work across cores; the result must not depend on how
/// many it used.
#[test]
fn a_long_render_is_the_same_whichever_thread_produced_each_chunk() {
    // Longer than one chunk (65 536 samples), so rayon really does fan out.
    let spec = GenSpec::new(
        48_000.0,
        5.0,
        Node::Sum {
            terms: vec![
                Node::Chirp {
                    f0_hz: 20.0,
                    f1_hz: 20_000.0,
                    sweep: Sweep::Log,
                    amp: 1.0,
                },
                Node::Noise {
                    kind: NoiseKind::Pink,
                    amp: 0.3,
                },
            ],
        },
    )
    .with_seed(7);

    let parallel = whole(&spec);
    assert_eq!(parallel.len(), 240_000);

    // The same span, one 4 999-sample window at a time.
    let mut serial = Vec::with_capacity(parallel.len());
    let mut start = 0;
    while start < spec.sample_count() {
        let end = (start + 4_999).min(spec.sample_count());
        serial.extend(samples(&spec, SampleRange::new(start, end)));
        start = end;
    }
    assert_eq!(serial, parallel);
}
