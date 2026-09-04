//! Spec → samples (`docs/DESIGN.md` §8.2).
//!
//! > Pure function `render(&GenSpec, range: SampleRange) -> SampleBuffer`, so
//! > any window can be produced independently. Rayon splits the range across
//! > cores.
//!
//! Independence is the whole design constraint. Every node evaluates as a
//! function of its own sample index, never of a running state, so a chunk
//! rendered on one thread equals the same span rendered as part of the whole —
//! which is what the determinism property test asserts (goal G3).
//!
//! Two nodes change the grid their children render on, and validation follows
//! the same rules so a message lands on the right node:
//!
//! - `Concat` gives each part its own origin and its own duration.
//! - `Resample` renders its input on the resampled grid, then reads it back
//!   with linear interpolation.

use std::collections::HashMap;
use std::f64::consts::TAU;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use sp_core::{SampleBuffer, SampleRange, SignalId};

use crate::control::{GenControl, GenProgress, CHUNK_SAMPLES};
use crate::error::{GenError, Result};
use crate::expr;
use crate::noise::{self, Noise, Prbs};
use crate::spec::{GenSpec, ModKind, Node, OscShape};
use crate::tree::{self, ROOT};
use crate::validate::validate;
use crate::waveform;

/// A stored signal read as a generator source, for
/// [`Node::FromSignal`](crate::spec::Node::FromSignal).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSignal {
    pub values: Vec<f64>,
    pub sample_rate_hz: f64,
    pub t0_s: f64,
}

/// The sources a spec's `FromSignal` nodes resolve to, fetched once before a
/// render so the render itself stays pure.
pub type Sources = HashMap<SignalId, SourceSignal>;

/// Every signal a spec reads as a source.
#[must_use]
pub fn referenced_signals(spec: &GenSpec) -> Vec<SignalId> {
    let mut out: Vec<SignalId> = spec
        .root
        .walk()
        .filter_map(|node| match node {
            Node::FromSignal { signal_id } => Some(*signal_id),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// What every node in one render shares.
struct Ctx<'a> {
    seed: u64,
    /// The finished signal's place on the absolute timeline, which is what a
    /// `FromSignal` source aligns against.
    t0_s: f64,
    sources: &'a Sources,
}

/// The grid one node renders on.
#[derive(Debug, Clone)]
struct Frame {
    rate_hz: f64,
    /// The span a `Chirp`, `Ramp` or `Envelope` stretches across.
    duration_s: f64,
    /// Seconds from the signal's start to this grid's origin; non-zero inside
    /// a `Concat` part or behind a `Delay`.
    offset_s: f64,
    /// The node's JSON pointer, which is what seeds its noise streams.
    path: String,
}

impl Frame {
    /// The same grid one level down, under the field the child hangs off.
    fn child(&self, field: &str, index: Option<usize>) -> Self {
        Self {
            path: tree::child_pointer(&self.path, field, index),
            ..self.clone()
        }
    }
}

/// Renders the whole signal.
pub fn render(spec: &GenSpec) -> Result<SampleBuffer> {
    render_range(spec, SampleRange::first(spec.sample_count()))
}

/// Renders one window of the signal.
pub fn render_range(spec: &GenSpec, range: SampleRange) -> Result<SampleBuffer> {
    render_with(spec, range, &Sources::new(), &GenControl::new())
}

/// Renders one window with sources resolved and a control attached.
pub fn render_with(
    spec: &GenSpec,
    range: SampleRange,
    sources: &Sources,
    control: &GenControl,
) -> Result<SampleBuffer> {
    let values = render_values(spec, range, sources, control)?;
    Ok(SampleBuffer::from_f64(spec.dtype, &values))
}

/// Renders one window as `f64`, before it is narrowed to the spec's dtype.
///
/// Processing is `f64` throughout and narrows on write (§17.10), so the live
/// preview and the statistics both read this rather than the stored samples.
pub fn render_values(
    spec: &GenSpec,
    range: SampleRange,
    sources: &Sources,
    control: &GenControl,
) -> Result<Vec<f64>> {
    let issues = validate(spec);
    if issues.blocks() {
        return Err(GenError::Invalid(issues));
    }
    for id in referenced_signals(spec) {
        if !sources.contains_key(&id) {
            return Err(GenError::Source {
                id: id.get(),
                reason: "the signal was not resolved before rendering".to_owned(),
            });
        }
    }

    let range = range.intersect(SampleRange::first(spec.sample_count()));
    let mut out = vec![0.0; range.len() as usize];
    if out.is_empty() {
        return Ok(out);
    }

    let ctx = Ctx {
        seed: spec.seed,
        t0_s: spec.timebase.t0_s,
        sources,
    };
    let frame = Frame {
        rate_hz: spec.sample_rate_hz(),
        duration_s: spec.duration_s,
        offset_s: 0.0,
        path: ROOT.to_owned(),
    };

    let chunk = usize::try_from(CHUNK_SAMPLES).unwrap_or(usize::MAX);
    let done = AtomicU64::new(0);
    let total = range.len();

    out.par_chunks_mut(chunk)
        .enumerate()
        .try_for_each(|(index, slice)| -> Result<()> {
            control.check()?;
            let start = range.start + (index * chunk) as u64;
            let span = SampleRange::new(start, start + slice.len() as u64);
            fill(&spec.root, &frame, span, slice, &ctx);
            let so_far = done.fetch_add(span.len(), Ordering::Relaxed) + span.len();
            control.report(GenProgress {
                values_done: so_far,
                values_total: total,
                ..GenProgress::default()
            });
            Ok(())
        })?;

    Ok(out)
}

/// Fills `out` with `node` evaluated over `range` on `frame`'s grid.
///
/// `out.len()` is `range.len()`; index `range.start + k` lands in `out[k]`.
fn fill(node: &Node, frame: &Frame, range: SampleRange, out: &mut [f64], ctx: &Ctx<'_>) {
    let rate = frame.rate_hz;
    let time_of = |index: u64| index as f64 / rate;

    match node {
        Node::Sine { .. } | Node::Square { .. } | Node::Triangle { .. } | Node::Sawtooth { .. } => {
            let osc = node.as_oscillator().expect("a periodic primitive");
            let phase_offset = osc.phase_rad / TAU;
            for (k, slot) in out.iter_mut().enumerate() {
                let t = time_of(range.start + k as u64);
                *slot = osc.amp
                    * waveform::shape(osc.shape, osc.freq_hz * t + phase_offset, osc.duty)
                    + osc.offset;
            }
        }
        Node::Pulse {
            period_s,
            width_s,
            amp,
            rise_s,
            fall_s,
        } => {
            for (k, slot) in out.iter_mut().enumerate() {
                let t = time_of(range.start + k as u64);
                *slot = waveform::pulse(t, *period_s, *width_s, *amp, *rise_s, *fall_s);
            }
        }
        Node::Chirp {
            f0_hz,
            f1_hz,
            sweep,
            amp,
        } => {
            for (k, slot) in out.iter_mut().enumerate() {
                let t = time_of(range.start + k as u64);
                let turns = waveform::chirp_turns(t, *f0_hz, *f1_hz, *sweep, frame.duration_s);
                *slot = amp * waveform::shape(OscShape::Sine, turns, 0.5);
            }
        }
        Node::Dc { level } => out.fill(*level),
        Node::Ramp { start, end } => {
            for (k, slot) in out.iter_mut().enumerate() {
                let t = time_of(range.start + k as u64);
                *slot = waveform::ramp(t, *start, *end, frame.duration_s);
            }
        }
        Node::Step {
            at_s,
            before,
            after,
        } => {
            for (k, slot) in out.iter_mut().enumerate() {
                let t = time_of(range.start + k as u64);
                *slot = if t < *at_s { *before } else { *after };
            }
        }
        Node::Impulse { at_s, amp } => {
            out.fill(0.0);
            let index = (at_s * rate).round();
            if index >= 0.0 && index <= u64::MAX as f64 {
                let index = index as u64;
                if range.contains(index) {
                    out[(index - range.start) as usize] = *amp;
                }
            }
        }
        Node::Noise { kind, amp } => {
            let mut noise = Noise::new(*kind, noise::stream_key(ctx.seed, &frame.path));
            noise.seek(range.start);
            for (k, slot) in out.iter_mut().enumerate() {
                *slot = amp * noise.value(range.start + k as u64);
            }
        }
        Node::Prbs { order, taps, amp } => {
            let taps = taps
                .filter(|mask| *mask != 0)
                .or_else(|| noise::default_taps(*order))
                .unwrap_or(0b11);
            let mut prbs = Prbs::new(*order, taps);
            prbs.seek(range.start);
            for slot in out.iter_mut() {
                *slot = if prbs.next_bit() { *amp } else { -*amp };
            }
        }
        Node::Expr { source } => {
            // Validation rejects an expression that will not compile, so a
            // failure here can only mean the spec changed under us; a flat
            // zero is the honest answer rather than a panic.
            let Ok(program) = expr::Program::compile(source) else {
                out.fill(0.0);
                return;
            };
            let mut stack = program.stack();
            for (k, slot) in out.iter_mut().enumerate() {
                *slot = program.eval_into(time_of(range.start + k as u64), &mut stack);
            }
        }

        Node::Sum { terms } => {
            out.fill(0.0);
            accumulate(terms, frame, range, out, ctx, |acc, value| acc + value);
        }
        Node::Product { terms } => {
            out.fill(1.0);
            accumulate(terms, frame, range, out, ctx, |acc, value| acc * value);
        }
        Node::Concat { parts } => {
            out.fill(0.0);
            let mut start_s = 0.0;
            for (index, part) in parts.iter().enumerate() {
                // Part boundaries land on samples, so a part's own grid is the
                // parent's grid shifted by a whole number of samples.
                let first = (start_s * rate).round().max(0.0) as u64;
                start_s += part.duration_s.max(0.0);
                let last = (start_s * rate).round().max(0.0) as u64;
                let span = range.intersect(SampleRange::new(first, last));
                if span.is_empty() {
                    continue;
                }
                let child = Frame {
                    duration_s: part.duration_s.max(0.0),
                    offset_s: frame.offset_s + first as f64 / rate,
                    ..frame.child(tree::PARTS, Some(index))
                };
                let from = (span.start - range.start) as usize;
                let to = (span.end - range.start) as usize;
                fill(
                    &part.node,
                    &child,
                    SampleRange::new(span.start - first, span.end - first),
                    &mut out[from..to],
                    ctx,
                );
            }
        }
        Node::Gain { input, factor } => {
            fill(input, &frame.child(tree::INPUT, None), range, out, ctx);
            for slot in out.iter_mut() {
                *slot *= factor;
            }
        }
        Node::Delay { input, by_s } => {
            // Whole samples: a sub-sample shift would need interpolation, and
            // `Resample` is the node that owns interpolation.
            let shift = (by_s.max(0.0) * rate).round().max(0.0) as u64;
            out.fill(0.0);
            let span = range.intersect(SampleRange::new(shift, u64::MAX));
            if span.is_empty() {
                return;
            }
            let from = (span.start - range.start) as usize;
            let to = (span.end - range.start) as usize;
            let child = Frame {
                offset_s: frame.offset_s + shift as f64 / rate,
                ..frame.child(tree::INPUT, None)
            };
            fill(
                input,
                &child,
                SampleRange::new(span.start - shift, span.end - shift),
                &mut out[from..to],
                ctx,
            );
        }
        Node::Clip { input, lo, hi } => {
            fill(input, &frame.child(tree::INPUT, None), range, out, ctx);
            for slot in out.iter_mut() {
                *slot = slot.clamp(*lo, *hi);
            }
        }
        Node::Envelope { input, env } => {
            fill(input, &frame.child(tree::INPUT, None), range, out, ctx);
            for (k, slot) in out.iter_mut().enumerate() {
                let t = time_of(range.start + k as u64);
                *slot *= waveform::envelope(t, env, frame.duration_s);
            }
        }
        Node::Modulate {
            carrier,
            modulator,
            kind,
        } => match kind {
            ModKind::Am { depth } => {
                fill(carrier, &frame.child(tree::CARRIER, None), range, out, ctx);
                let mut scratch = vec![0.0; out.len()];
                fill(
                    modulator,
                    &frame.child(tree::MODULATOR, None),
                    range,
                    &mut scratch,
                    ctx,
                );
                for (slot, m) in out.iter_mut().zip(scratch) {
                    *slot *= 1.0 + depth * m;
                }
            }
            ModKind::Fm { dev_hz } => {
                // Validation guarantees both of these.
                let osc = carrier.as_oscillator().expect("an oscillator carrier");
                let phase_offset = osc.phase_rad / TAU;
                for (k, slot) in out.iter_mut().enumerate() {
                    let t = time_of(range.start + k as u64);
                    let deviation = dev_hz * waveform::integral_at(modulator, t, frame.duration_s);
                    *slot = osc.amp
                        * waveform::shape(
                            osc.shape,
                            osc.freq_hz * t + phase_offset + deviation,
                            osc.duty,
                        )
                        + osc.offset;
                }
            }
            ModKind::Pm { dev_rad } => {
                let osc = carrier.as_oscillator().expect("an oscillator carrier");
                let phase_offset = osc.phase_rad / TAU;
                let mut scratch = vec![0.0; out.len()];
                fill(
                    modulator,
                    &frame.child(tree::MODULATOR, None),
                    range,
                    &mut scratch,
                    ctx,
                );
                for (k, slot) in out.iter_mut().enumerate() {
                    let t = time_of(range.start + k as u64);
                    let deviation = dev_rad * scratch[k] / TAU;
                    *slot = osc.amp
                        * waveform::shape(
                            osc.shape,
                            osc.freq_hz * t + phase_offset + deviation,
                            osc.duty,
                        )
                        + osc.offset;
                }
            }
        },
        Node::Resample { input, to_rate_hz } => {
            // Above the output rate there is nothing to reconstruct, and the
            // inner grid would be larger than the output for no gain;
            // validation warns about it, and this is what it means.
            let inner_rate = to_rate_hz.min(rate).max(f64::MIN_POSITIVE);
            let ratio = inner_rate / rate;
            let first = ((range.start as f64) * ratio).floor().max(0.0) as u64;
            let last = (((range.end.saturating_sub(1)) as f64) * ratio)
                .floor()
                .max(0.0) as u64
                + 2;
            let mut scratch = vec![0.0; (last - first) as usize];
            let child = Frame {
                rate_hz: inner_rate,
                ..frame.child(tree::INPUT, None)
            };
            fill(
                input,
                &child,
                SampleRange::new(first, last),
                &mut scratch,
                ctx,
            );
            for (k, slot) in out.iter_mut().enumerate() {
                let u = (range.start + k as u64) as f64 * ratio;
                let base = u.floor();
                let frac = u - base;
                let index = base as u64 - first;
                let a = scratch.get(index as usize).copied().unwrap_or(0.0);
                let b = scratch.get(index as usize + 1).copied().unwrap_or(a);
                *slot = a + (b - a) * frac;
            }
        }
        Node::FromSignal { signal_id } => {
            let Some(source) = ctx.sources.get(signal_id) else {
                out.fill(0.0);
                return;
            };
            for (k, slot) in out.iter_mut().enumerate() {
                let absolute = ctx.t0_s + frame.offset_s + time_of(range.start + k as u64);
                *slot = sample_source(source, absolute);
            }
        }
    }
}

/// Reads a source signal at an absolute time, linearly interpolated and zero
/// outside its span.
fn sample_source(source: &SourceSignal, absolute_s: f64) -> f64 {
    if source.values.is_empty()
        || !source.sample_rate_hz.is_finite()
        || source.sample_rate_hz <= 0.0
    {
        return 0.0;
    }
    let u = (absolute_s - source.t0_s) * source.sample_rate_hz;
    if u < 0.0 || u > (source.values.len() - 1) as f64 {
        return 0.0;
    }
    let base = u.floor();
    let frac = u - base;
    let index = base as usize;
    let a = source.values[index];
    let b = source.values.get(index + 1).copied().unwrap_or(a);
    a + (b - a) * frac
}

/// Folds every term of a `Sum` or `Product` into `out`.
fn accumulate(
    terms: &[Node],
    frame: &Frame,
    range: SampleRange,
    out: &mut [f64],
    ctx: &Ctx<'_>,
    combine: fn(f64, f64) -> f64,
) {
    let mut scratch = vec![0.0; out.len()];
    for (index, term) in terms.iter().enumerate() {
        let child = frame.child(tree::TERMS, Some(index));
        fill(term, &child, range, &mut scratch, ctx);
        for (slot, value) in out.iter_mut().zip(scratch.iter()) {
            *slot = combine(*slot, *value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{ConcatPart, ModKind, Node, NodeKind, NoiseKind, Sweep};

    fn values(spec: &GenSpec) -> Vec<f64> {
        render_values(
            spec,
            SampleRange::first(spec.sample_count()),
            &Sources::new(),
            &GenControl::new(),
        )
        .expect("renders")
    }

    #[test]
    fn a_sine_renders_at_its_frequency_and_amplitude() {
        let spec = GenSpec::new(
            1_000.0,
            1.0,
            Node::Sine {
                freq_hz: 1.0,
                amp: 2.0,
                phase_rad: 0.0,
                offset: 0.5,
            },
        );
        let values = values(&spec);
        assert_eq!(values.len(), 1_000);
        assert!((values[0] - 0.5).abs() < 1e-9);
        assert!((values[250] - 2.5).abs() < 1e-9);
        assert!((values[750] + 1.5).abs() < 1e-9);
    }

    #[test]
    fn a_sum_adds_and_a_product_multiplies() {
        let two = |level| Node::Dc { level };
        let spec = GenSpec::new(
            100.0,
            0.1,
            Node::Sum {
                terms: vec![two(1.0), two(2.0), two(3.0)],
            },
        );
        assert!(values(&spec).iter().all(|v| (*v - 6.0).abs() < 1e-12));

        let spec = GenSpec::new(
            100.0,
            0.1,
            Node::Product {
                terms: vec![two(2.0), two(3.0)],
            },
        );
        assert!(values(&spec).iter().all(|v| (*v - 6.0).abs() < 1e-12));
    }

    #[test]
    fn a_concat_lays_its_parts_end_to_end_and_pads_with_silence() {
        let spec = GenSpec::new(
            100.0,
            1.0,
            Node::Concat {
                parts: vec![
                    ConcatPart {
                        node: Node::Dc { level: 1.0 },
                        duration_s: 0.25,
                    },
                    ConcatPart {
                        node: Node::Dc { level: -1.0 },
                        duration_s: 0.25,
                    },
                ],
            },
        );
        let values = values(&spec);
        assert_eq!(values[0], 1.0);
        assert_eq!(values[24], 1.0);
        assert_eq!(values[25], -1.0);
        assert_eq!(values[49], -1.0);
        assert_eq!(values[50], 0.0);
        assert_eq!(values[99], 0.0);
    }

    #[test]
    fn a_concat_part_sees_time_from_its_own_start() {
        // A ramp in the second part restarts rather than continuing.
        let spec = GenSpec::new(
            100.0,
            1.0,
            Node::Concat {
                parts: vec![
                    ConcatPart {
                        node: Node::Ramp {
                            start: 0.0,
                            end: 1.0,
                        },
                        duration_s: 0.5,
                    },
                    ConcatPart {
                        node: Node::Ramp {
                            start: 0.0,
                            end: 1.0,
                        },
                        duration_s: 0.5,
                    },
                ],
            },
        );
        let values = values(&spec);
        assert!(values[49] > 0.9);
        assert!(values[50] < 0.1);
    }

    #[test]
    fn a_gain_scales_and_a_clip_limits() {
        let spec = GenSpec::new(
            100.0,
            0.1,
            Node::Clip {
                input: Box::new(Node::Gain {
                    input: Box::new(Node::Dc { level: 1.0 }),
                    factor: 10.0,
                }),
                lo: -2.0,
                hi: 3.0,
            },
        );
        assert!(values(&spec).iter().all(|v| *v == 3.0));
    }

    #[test]
    fn a_delay_shifts_by_whole_samples_and_leads_with_silence() {
        let spec = GenSpec::new(
            100.0,
            0.5,
            Node::Delay {
                input: Box::new(Node::Ramp {
                    start: 1.0,
                    end: 1.0,
                }),
                by_s: 0.1,
            },
        );
        let values = values(&spec);
        assert!(values[..10].iter().all(|v| *v == 0.0));
        assert!(values[10..].iter().all(|v| (*v - 1.0).abs() < 1e-12));
    }

    #[test]
    fn an_impulse_lands_on_one_sample() {
        let spec = GenSpec::new(
            1_000.0,
            0.1,
            Node::Impulse {
                at_s: 0.05,
                amp: 3.0,
            },
        );
        let values = values(&spec);
        assert_eq!(values[50], 3.0);
        assert_eq!(values.iter().filter(|v| **v != 0.0).count(), 1);
    }

    #[test]
    fn a_resample_holds_the_slower_grid_between_its_samples() {
        // A 1 kHz output reading a 100 Hz grid: the interpolation is linear,
        // so a ramp stays a ramp but a fast tone aliases.
        let spec = GenSpec::new(
            1_000.0,
            0.1,
            Node::Resample {
                input: Box::new(Node::Ramp {
                    start: 0.0,
                    end: 1.0,
                }),
                to_rate_hz: 100.0,
            },
        );
        let values = values(&spec);
        for (index, value) in values.iter().enumerate() {
            let expected = index as f64 / 1_000.0 / 0.1;
            assert!((value - expected).abs() < 0.02, "{index}: {value}");
        }
    }

    #[test]
    fn am_rides_the_carrier_and_pm_moves_its_phase() {
        let carrier = || {
            Box::new(Node::Sine {
                freq_hz: 100.0,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            })
        };
        let am = GenSpec::new(
            10_000.0,
            0.1,
            Node::Modulate {
                carrier: carrier(),
                modulator: Box::new(Node::Dc { level: 1.0 }),
                kind: ModKind::Am { depth: 0.5 },
            },
        );
        let plain = GenSpec::new(10_000.0, 0.1, *carrier());
        let (am, plain) = (values(&am), values(&plain));
        for (a, p) in am.iter().zip(&plain) {
            assert!((a - 1.5 * p).abs() < 1e-9);
        }

        // A constant modulator under PM is a fixed phase shift.
        let pm = GenSpec::new(
            10_000.0,
            0.1,
            Node::Modulate {
                carrier: carrier(),
                modulator: Box::new(Node::Dc { level: 1.0 }),
                kind: ModKind::Pm {
                    dev_rad: std::f64::consts::FRAC_PI_2,
                },
            },
        );
        let pm = values(&pm);
        // A quarter turn later, a sine is a cosine.
        assert!((pm[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fm_sweeps_the_carrier_by_the_modulator_integral() {
        // A DC modulator is a constant frequency offset.
        let spec = GenSpec::new(
            10_000.0,
            0.1,
            Node::Modulate {
                carrier: Box::new(Node::Sine {
                    freq_hz: 100.0,
                    amp: 1.0,
                    phase_rad: 0.0,
                    offset: 0.0,
                }),
                modulator: Box::new(Node::Dc { level: 1.0 }),
                kind: ModKind::Fm { dev_hz: 50.0 },
            },
        );
        let shifted = GenSpec::new(
            10_000.0,
            0.1,
            Node::Sine {
                freq_hz: 150.0,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            },
        );
        for (a, b) in values(&spec).iter().zip(values(&shifted)) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn a_source_signal_is_read_by_absolute_time() {
        let mut sources = Sources::new();
        sources.insert(
            SignalId::new(7),
            SourceSignal {
                values: vec![0.0, 1.0, 2.0, 3.0],
                sample_rate_hz: 10.0,
                t0_s: 0.0,
            },
        );
        let spec = GenSpec::new(
            10.0,
            0.6,
            Node::FromSignal {
                signal_id: SignalId::new(7),
            },
        );
        let values =
            render_values(&spec, SampleRange::first(6), &sources, &GenControl::new()).unwrap();
        assert_eq!(&values[..4], &[0.0, 1.0, 2.0, 3.0]);
        // Past the source's last sample the output is silent.
        assert_eq!(&values[4..], &[0.0, 0.0]);
    }

    #[test]
    fn an_unresolved_source_is_an_error_rather_than_silence() {
        let spec = GenSpec::new(
            10.0,
            0.5,
            Node::FromSignal {
                signal_id: SignalId::new(1),
            },
        );
        let error = render_values(
            &spec,
            SampleRange::first(5),
            &Sources::new(),
            &GenControl::new(),
        )
        .expect_err("unresolved");
        assert!(matches!(error, GenError::Source { id: 1, .. }));
    }

    #[test]
    fn an_invalid_spec_renders_nothing() {
        let spec = GenSpec::new(
            1_000.0,
            0.1,
            Node::Sine {
                freq_hz: 900.0,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            },
        );
        let error = render(&spec).expect_err("above Nyquist");
        assert!(error.issues().is_some_and(crate::Issues::blocks));
    }

    #[test]
    fn a_window_matches_the_same_span_of_the_whole_render() {
        // Every node kind at once, noise included.
        let spec = GenSpec::new(
            8_000.0,
            0.5,
            Node::Sum {
                terms: vec![
                    Node::Chirp {
                        f0_hz: 100.0,
                        f1_hz: 1_000.0,
                        sweep: Sweep::Log,
                        amp: 1.0,
                    },
                    Node::Noise {
                        kind: NoiseKind::Pink,
                        amp: 0.2,
                    },
                    Node::Prbs {
                        order: 9,
                        taps: None,
                        amp: 0.1,
                    },
                ],
            },
        )
        .with_seed(1234);

        let whole = values(&spec);
        let window = SampleRange::new(1_037, 2_411);
        let part = render_values(&spec, window, &Sources::new(), &GenControl::new()).unwrap();
        assert_eq!(
            part,
            whole[window.start as usize..window.end as usize].to_vec()
        );
    }

    #[test]
    fn every_node_kind_renders_something_finite() {
        let mut sources = Sources::new();
        sources.insert(
            SignalId::new(0),
            SourceSignal {
                values: vec![1.0; 64],
                sample_rate_hz: 48_000.0,
                t0_s: 0.0,
            },
        );
        for kind in NodeKind::ALL {
            let spec = GenSpec::new(48_000.0, 0.01, kind.default_node());
            let values = render_values(
                &spec,
                SampleRange::first(spec.sample_count()),
                &sources,
                &GenControl::new(),
            )
            .unwrap_or_else(|e| panic!("{kind}: {e}"));
            assert_eq!(values.len(), 480, "{kind}");
            assert!(values.iter().all(|v| v.is_finite()), "{kind}");
        }
    }

    #[test]
    fn cancellation_stops_a_render() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let flag = Arc::new(AtomicBool::new(true));
        let control = GenControl::new().with_cancel(flag);
        let spec = GenSpec::new(48_000.0, 10.0, Node::default());
        let error = render_values(
            &spec,
            SampleRange::first(spec.sample_count()),
            &Sources::new(),
            &control,
        )
        .expect_err("cancelled");
        assert!(matches!(error, GenError::Cancelled));
    }

    #[test]
    fn a_render_reports_progress_up_to_the_total() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(GenProgress::default()));
        let sink = seen.clone();
        let control = GenControl::new().with_progress(Arc::new(move |progress| {
            let mut slot = sink.lock().unwrap();
            if progress.values_done > slot.values_done {
                *slot = progress;
            }
        }));
        let spec = GenSpec::new(48_000.0, 5.0, Node::default());
        let total = spec.sample_count();
        render_values(&spec, SampleRange::first(total), &Sources::new(), &control).unwrap();
        let seen = *seen.lock().unwrap();
        assert_eq!(seen.values_done, total);
        assert_eq!(seen.values_total, total);
    }

    #[test]
    fn referenced_signals_are_listed_once_each() {
        let spec = GenSpec::new(
            100.0,
            1.0,
            Node::Sum {
                terms: vec![
                    Node::FromSignal {
                        signal_id: SignalId::new(3),
                    },
                    Node::FromSignal {
                        signal_id: SignalId::new(3),
                    },
                    Node::FromSignal {
                        signal_id: SignalId::new(1),
                    },
                ],
            },
        );
        assert_eq!(
            referenced_signals(&spec),
            [SignalId::new(1), SignalId::new(3)]
        );
    }
}
