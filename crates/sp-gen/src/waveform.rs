//! Primitive oscillators, envelopes, and the closed-form integrals FM needs
//! (`docs/DESIGN.md` §8.1, §8.2).
//!
//! Every function here is a pure function of `t`, so a window renders the same
//! whether it is produced alone or as one chunk of a larger range — the
//! property `render` is parallelised on (§8.2).

use std::f64::consts::{PI, TAU};

use crate::spec::{EnvelopeSpec, Node, OscShape, Sweep};

/// Fractional part in `[0, 1)`, for negative arguments too.
#[must_use]
pub fn turns(x: f64) -> f64 {
    let r = x - x.floor();
    // `x.floor()` loses the last bit for very large `x`; clamp rather than
    // return a fraction of exactly 1.0, which would step a waveshaper.
    if r >= 1.0 {
        0.0
    } else {
        r
    }
}

/// The waveshaper of an oscillator, taking phase in turns rather than radians
/// so FM and PM can rewrite the phase and reuse the same shape.
#[must_use]
pub fn shape(shape: OscShape, phase_turns: f64, duty: f64) -> f64 {
    let r = turns(phase_turns);
    match shape {
        OscShape::Sine => (TAU * phase_turns).sin(),
        OscShape::Square => {
            if r < duty {
                1.0
            } else {
                -1.0
            }
        }
        // Exact triangle, and continuous at the corners without a branch.
        OscShape::Triangle => (TAU * phase_turns).sin().asin() * (2.0 / PI),
        OscShape::SawRising => 2.0 * r - 1.0,
        OscShape::SawFalling => 1.0 - 2.0 * r,
    }
}

/// A trapezoidal pulse train: `rise_s` up, `width_s` at `amp`, `fall_s` down,
/// then zero for the rest of the period.
#[must_use]
pub fn pulse(t: f64, period_s: f64, width_s: f64, amp: f64, rise_s: f64, fall_s: f64) -> f64 {
    if period_s <= 0.0 {
        return 0.0;
    }
    // `t / period_s` can land a hair either side of a whole number, so the
    // remainder can come back very slightly negative; clamping it is what
    // keeps a zero-length rise from dividing by zero at a period boundary.
    let local = (t - (t / period_s).floor() * period_s).clamp(0.0, period_s);
    let flat_end = rise_s + width_s;
    let fall_end = flat_end + fall_s;
    if rise_s > 0.0 && local < rise_s {
        amp * (local / rise_s)
    } else if local < flat_end {
        amp
    } else if fall_s > 0.0 && local < fall_end {
        amp * (1.0 - (local - flat_end) / fall_s)
    } else {
        0.0
    }
}

/// A chirp's phase in turns at `t`, swept across `duration_s`.
#[must_use]
pub fn chirp_turns(t: f64, f0_hz: f64, f1_hz: f64, sweep: Sweep, duration_s: f64) -> f64 {
    if duration_s <= 0.0 {
        return f0_hz * t;
    }
    match sweep {
        Sweep::Linear => f0_hz * t + (f1_hz - f0_hz) * t * t / (2.0 * duration_s),
        Sweep::Quadratic => {
            f0_hz * t + (f1_hz - f0_hz) * t * t * t / (3.0 * duration_s * duration_s)
        }
        Sweep::Log => {
            // f(t) = f0 * k^(t/T); its integral needs f0 > 0 and k != 1, both
            // of which validation guarantees for a log sweep.
            let k = f1_hz / f0_hz;
            if !k.is_finite() || k <= 0.0 || (k - 1.0).abs() < 1e-12 {
                f0_hz * t
            } else {
                f0_hz * duration_s / k.ln() * (k.powf(t / duration_s) - 1.0)
            }
        }
    }
}

/// A straight line from `start` to `end` across `duration_s`.
#[must_use]
pub fn ramp(t: f64, start: f64, end: f64, duration_s: f64) -> f64 {
    if duration_s <= 0.0 {
        return start;
    }
    start + (end - start) * (t / duration_s)
}

/// The envelope's gain at `t`, over a signal of `duration_s`.
#[must_use]
pub fn envelope(t: f64, env: &EnvelopeSpec, duration_s: f64) -> f64 {
    match *env {
        EnvelopeSpec::Adsr {
            attack_s,
            decay_s,
            sustain,
            release_s,
        } => {
            let release_at = (duration_s - release_s).max(0.0);
            if t < 0.0 {
                0.0
            } else if attack_s > 0.0 && t < attack_s {
                t / attack_s
            } else if decay_s > 0.0 && t < attack_s + decay_s {
                1.0 + (sustain - 1.0) * (t - attack_s) / decay_s
            } else if t < release_at {
                sustain
            } else if release_s > 0.0 && t < duration_s {
                sustain * (1.0 - (t - release_at) / release_s)
            } else {
                0.0
            }
        }
        EnvelopeSpec::Gaussian { center_s, sigma_s } => {
            if sigma_s <= 0.0 {
                return 0.0;
            }
            let z = (t - center_s) / sigma_s;
            (-0.5 * z * z).exp()
        }
        EnvelopeSpec::Tukey { alpha } => {
            if alpha <= 0.0 || duration_s <= 0.0 {
                return 1.0;
            }
            let taper = alpha.min(1.0) * duration_s / 2.0;
            if t < taper {
                0.5 * (1.0 + (PI * (t / taper - 1.0)).cos())
            } else if t > duration_s - taper {
                0.5 * (1.0 + (PI * ((t - (duration_s - taper)) / taper)).cos())
            } else {
                1.0
            }
        }
    }
}

/// `∫₀ᵗ node(u) du`, in closed form.
///
/// Only defined for the nodes [`Node::is_integrable`] accepts; validation
/// rejects an FM modulator that is anything else, so the `0.0` fallback is
/// unreachable from a rendered spec.
#[must_use]
pub fn integral_at(node: &Node, t: f64, duration_s: f64) -> f64 {
    match node {
        Node::Dc { level } => level * t,
        Node::Sine {
            freq_hz,
            amp,
            phase_rad,
            offset,
        } => {
            let dc = offset * t;
            if *freq_hz == 0.0 {
                return dc + amp * phase_rad.sin() * t;
            }
            dc + amp * (phase_rad.cos() - (TAU * freq_hz * t + phase_rad).cos()) / (TAU * freq_hz)
        }
        Node::Square {
            freq_hz,
            amp,
            duty,
            phase_rad,
        } => {
            let offset_turns = phase_rad / TAU;
            if *freq_hz == 0.0 {
                return amp * shape(OscShape::Square, offset_turns, *duty) * t;
            }
            let square_area = |x: f64| {
                let r = turns(x);
                let partial = if r < *duty { r } else { 2.0 * duty - r };
                x.floor() * (2.0 * duty - 1.0) + partial
            };
            amp * (square_area(freq_hz * t + offset_turns) - square_area(offset_turns)) / freq_hz
        }
        Node::Triangle {
            freq_hz,
            amp,
            phase_rad,
        } => {
            let offset_turns = phase_rad / TAU;
            if *freq_hz == 0.0 {
                return amp * shape(OscShape::Triangle, offset_turns, 0.5) * t;
            }
            amp * (triangle_area(freq_hz * t + offset_turns) - triangle_area(offset_turns))
                / freq_hz
        }
        Node::Sawtooth {
            freq_hz,
            amp,
            rising,
        } => {
            let sign = if *rising { 1.0 } else { -1.0 };
            if *freq_hz == 0.0 {
                return -(sign * amp) * t;
            }
            // ∫₀¹ (2·frac(u) − 1) du = 0, so only the fractional part
            // contributes and there is no whole-turn term.
            let saw_area = |x: f64| {
                let r = turns(x);
                r * r - r
            };
            sign * amp * (saw_area(freq_hz * t) - saw_area(0.0)) / freq_hz
        }
        Node::Ramp { start, end } => {
            if duration_s <= 0.0 {
                start * t
            } else {
                start * t + (end - start) * t * t / (2.0 * duration_s)
            }
        }
        Node::Step {
            at_s,
            before,
            after,
        } => before * t.min(*at_s).max(0.0) + after * (t - at_s).max(0.0),
        Node::Sum { terms } => terms
            .iter()
            .map(|term| integral_at(term, t, duration_s))
            .sum(),
        Node::Gain { input, factor } => factor * integral_at(input, t, duration_s),
        Node::Delay { input, by_s } => {
            if t <= *by_s {
                0.0
            } else {
                integral_at(input, t - by_s, duration_s)
            }
        }
        _ => 0.0,
    }
}

/// `∫₀ˣ` of the unit triangle in turns. It integrates to zero over a whole
/// turn, so only the fractional part matters.
fn triangle_area(x: f64) -> f64 {
    let r = turns(x);
    if r <= 0.25 {
        2.0 * r * r
    } else if r <= 0.75 {
        let d = r - 0.25;
        0.125 + d - 2.0 * d * d
    } else {
        let d = r - 0.75;
        0.125 - d + 2.0 * d * d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numeric check of a closed-form integral: the midpoint rule over a fine
    /// grid has to land on it.
    fn numeric_integral(node: &Node, t: f64, duration_s: f64, steps: usize) -> f64 {
        let dt = t / steps as f64;
        (0..steps)
            .map(|i| {
                let mid = (i as f64 + 0.5) * dt;
                value_of(node, mid, duration_s) * dt
            })
            .sum()
    }

    /// The subset of nodes the integral covers, evaluated directly.
    fn value_of(node: &Node, t: f64, duration_s: f64) -> f64 {
        match node {
            Node::Dc { level } => *level,
            Node::Sine {
                freq_hz,
                amp,
                phase_rad,
                offset,
            } => amp * shape(OscShape::Sine, freq_hz * t + phase_rad / TAU, 0.5) + offset,
            Node::Square {
                freq_hz,
                amp,
                duty,
                phase_rad,
            } => amp * shape(OscShape::Square, freq_hz * t + phase_rad / TAU, *duty),
            Node::Triangle {
                freq_hz,
                amp,
                phase_rad,
            } => amp * shape(OscShape::Triangle, freq_hz * t + phase_rad / TAU, 0.5),
            Node::Sawtooth {
                freq_hz,
                amp,
                rising,
            } => {
                let s = if *rising {
                    OscShape::SawRising
                } else {
                    OscShape::SawFalling
                };
                amp * shape(s, freq_hz * t, 0.5)
            }
            Node::Ramp { start, end } => ramp(t, *start, *end, duration_s),
            Node::Step {
                at_s,
                before,
                after,
            } => {
                if t < *at_s {
                    *before
                } else {
                    *after
                }
            }
            Node::Sum { terms } => terms.iter().map(|n| value_of(n, t, duration_s)).sum(),
            Node::Gain { input, factor } => factor * value_of(input, t, duration_s),
            Node::Delay { input, by_s } => {
                if t < *by_s {
                    0.0
                } else {
                    value_of(input, t - by_s, duration_s)
                }
            }
            other => panic!("{other:?} is not integrable"),
        }
    }

    #[test]
    fn shapes_hit_their_landmarks() {
        assert!((shape(OscShape::Sine, 0.25, 0.5) - 1.0).abs() < 1e-12);
        assert_eq!(shape(OscShape::Square, 0.1, 0.5), 1.0);
        assert_eq!(shape(OscShape::Square, 0.6, 0.5), -1.0);
        assert_eq!(shape(OscShape::Square, 0.3, 0.25), -1.0);
        assert!((shape(OscShape::Triangle, 0.25, 0.5) - 1.0).abs() < 1e-9);
        assert!(shape(OscShape::Triangle, 0.0, 0.5).abs() < 1e-9);
        assert_eq!(shape(OscShape::SawRising, 0.0, 0.5), -1.0);
        assert!((shape(OscShape::SawRising, 0.75, 0.5) - 0.5).abs() < 1e-12);
        assert_eq!(shape(OscShape::SawFalling, 0.0, 0.5), 1.0);
    }

    #[test]
    fn shapes_are_periodic_across_the_negative_axis() {
        for shape_kind in [
            OscShape::Sine,
            OscShape::Square,
            OscShape::Triangle,
            OscShape::SawRising,
            OscShape::SawFalling,
        ] {
            let a = shape(shape_kind, 0.3, 0.5);
            let b = shape(shape_kind, -2.7, 0.5);
            assert!((a - b).abs() < 1e-9, "{shape_kind:?}: {a} vs {b}");
        }
    }

    #[test]
    fn a_trapezoidal_pulse_rises_holds_and_falls() {
        let p = |t| pulse(t, 1.0, 0.2, 2.0, 0.1, 0.1);
        assert_eq!(p(0.0), 0.0);
        assert!((p(0.05) - 1.0).abs() < 1e-12);
        assert_eq!(p(0.15), 2.0);
        assert!((p(0.35) - 1.0).abs() < 1e-12);
        assert_eq!(p(0.5), 0.0);
        // The train repeats, backwards in time too.
        assert_eq!(p(1.15), 2.0);
        assert_eq!(p(-0.85), 2.0);
    }

    #[test]
    fn a_zero_period_pulse_is_silent_rather_than_a_division_by_zero() {
        assert_eq!(pulse(0.5, 0.0, 0.1, 1.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn a_square_edged_pulse_survives_a_period_boundary() {
        // 0.009 / 0.001 rounds up, so the remainder comes back just below
        // zero and a zero-length rise would divide by it.
        for i in 0..480u64 {
            let t = i as f64 / 48_000.0;
            let v = pulse(t, 1e-3, 1e-4, 1.0, 0.0, 0.0);
            assert!(v.is_finite(), "sample {i} at {t} s is {v}");
            assert!(v == 0.0 || v == 1.0, "sample {i} is {v}");
        }
    }

    #[test]
    fn an_adsr_with_no_attack_or_decay_starts_at_its_sustain() {
        let env = EnvelopeSpec::Adsr {
            attack_s: 0.0,
            decay_s: 0.0,
            sustain: 0.5,
            release_s: 0.0,
        };
        assert_eq!(envelope(0.0, &env, 1.0), 0.5);
        assert_eq!(envelope(0.5, &env, 1.0), 0.5);
    }

    #[test]
    fn chirp_phase_starts_and_ends_at_the_swept_rates() {
        // The instantaneous frequency is the phase derivative; check it at
        // both ends of each sweep.
        let d = 1e-6;
        for sweep in Sweep::ALL {
            let phase = |t| chirp_turns(t, 100.0, 2_000.0, sweep, 1.0);
            let f_start = (phase(d) - phase(0.0)) / d;
            let f_end = (phase(1.0) - phase(1.0 - d)) / d;
            assert!((f_start - 100.0).abs() < 1.0, "{sweep:?}: {f_start}");
            assert!((f_end - 2_000.0).abs() < 1.0, "{sweep:?}: {f_end}");
        }
    }

    #[test]
    fn envelopes_cover_their_span() {
        let adsr = EnvelopeSpec::Adsr {
            attack_s: 0.1,
            decay_s: 0.1,
            sustain: 0.5,
            release_s: 0.2,
        };
        assert_eq!(envelope(0.0, &adsr, 1.0), 0.0);
        assert!((envelope(0.05, &adsr, 1.0) - 0.5).abs() < 1e-12);
        assert!((envelope(0.2, &adsr, 1.0) - 0.5).abs() < 1e-12);
        assert!((envelope(0.5, &adsr, 1.0) - 0.5).abs() < 1e-12);
        assert!(envelope(0.999, &adsr, 1.0) < 0.01);

        let tukey = EnvelopeSpec::Tukey { alpha: 0.5 };
        assert!(envelope(0.0, &tukey, 1.0) < 1e-12);
        assert!((envelope(0.5, &tukey, 1.0) - 1.0).abs() < 1e-12);
        assert!(envelope(1.0, &tukey, 1.0) < 1e-12);
        assert_eq!(envelope(0.3, &EnvelopeSpec::Tukey { alpha: 0.0 }, 1.0), 1.0);

        let gauss = EnvelopeSpec::Gaussian {
            center_s: 0.5,
            sigma_s: 0.1,
        };
        assert!((envelope(0.5, &gauss, 1.0) - 1.0).abs() < 1e-12);
        assert!(envelope(0.0, &gauss, 1.0) < 1e-5);
    }

    #[test]
    fn closed_form_integrals_match_a_numeric_one() {
        let cases: Vec<Node> = vec![
            Node::Dc { level: 1.5 },
            Node::Sine {
                freq_hz: 3.0,
                amp: 2.0,
                phase_rad: 0.7,
                offset: 0.25,
            },
            Node::Sine {
                freq_hz: 0.0,
                amp: 2.0,
                phase_rad: 0.5,
                offset: 0.0,
            },
            Node::Square {
                freq_hz: 2.0,
                amp: 1.5,
                duty: 0.3,
                phase_rad: 1.1,
            },
            Node::Triangle {
                freq_hz: 2.5,
                amp: 1.25,
                phase_rad: 0.4,
            },
            Node::Sawtooth {
                freq_hz: 1.75,
                amp: 0.8,
                rising: true,
            },
            Node::Sawtooth {
                freq_hz: 1.75,
                amp: 0.8,
                rising: false,
            },
            Node::Ramp {
                start: -1.0,
                end: 3.0,
            },
            Node::Step {
                at_s: 0.35,
                before: -0.5,
                after: 2.0,
            },
            Node::Sum {
                terms: vec![
                    Node::Dc { level: 0.5 },
                    Node::Sine {
                        freq_hz: 4.0,
                        amp: 1.0,
                        phase_rad: 0.0,
                        offset: 0.0,
                    },
                ],
            },
            Node::Gain {
                input: Box::new(Node::Dc { level: 2.0 }),
                factor: 3.0,
            },
            Node::Delay {
                input: Box::new(Node::Sine {
                    freq_hz: 2.0,
                    amp: 1.0,
                    phase_rad: 0.0,
                    offset: 0.0,
                }),
                by_s: 0.2,
            },
        ];

        for node in cases {
            assert!(node.is_integrable(), "{node:?}");
            for t in [0.13, 0.5, 0.87, 1.0] {
                let exact = integral_at(&node, t, 1.0);
                let numeric = numeric_integral(&node, t, 1.0, 200_000);
                assert!(
                    (exact - numeric).abs() < 2e-4,
                    "{node:?} at {t}: {exact} vs {numeric}"
                );
            }
        }
    }

    #[test]
    fn a_non_integrable_node_falls_back_to_zero_rather_than_nan() {
        let noise = Node::Noise {
            kind: crate::spec::NoiseKind::Gaussian,
            amp: 1.0,
        };
        assert_eq!(integral_at(&noise, 1.0, 1.0), 0.0);
    }
}
