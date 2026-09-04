//! The generator spec: a serialisable DAG describing a signal
//! (`docs/DESIGN.md` §8.1).
//!
//! A spec plus its seed is the whole truth about a generated signal; the
//! samples are a cache of it, so deleting a blob is always safe (§8.1) and
//! re-rendering reproduces what was there (goal G3).
//!
//! **Node time.** A node sees `t` measured from the *start of the signal*, not
//! from the absolute timeline: `t = index / sample_rate_hz`. `t0_s` places the
//! finished signal on the timeline (§11.3) and never reaches a node, so moving
//! a signal in time cannot change its samples.

use std::fmt;

use serde::{Deserialize, Serialize};
use sp_core::{DType, Domain, SignalId, Timebase};

/// How a [`Node::Chirp`] sweeps from `f0_hz` to `f1_hz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sweep {
    #[default]
    Linear,
    Log,
    Quadratic,
}

impl Sweep {
    pub const ALL: [Self; 3] = [Self::Linear, Self::Log, Self::Quadratic];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Log => "Logarithmic",
            Self::Quadratic => "Quadratic",
        }
    }
}

/// The spectral shape of a [`Node::Noise`] source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseKind {
    #[default]
    Gaussian,
    Uniform,
    Pink,
    Brown,
}

impl NoiseKind {
    pub const ALL: [Self; 4] = [Self::Gaussian, Self::Uniform, Self::Pink, Self::Brown];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gaussian => "Gaussian",
            Self::Uniform => "Uniform",
            Self::Pink => "Pink (1/f)",
            Self::Brown => "Brown (1/f^2)",
        }
    }
}

/// The amplitude envelope applied by [`Node::Envelope`]. Every variant is
/// defined over the spec's whole duration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "shape")]
pub enum EnvelopeSpec {
    Adsr {
        attack_s: f64,
        decay_s: f64,
        sustain: f64,
        release_s: f64,
    },
    Gaussian {
        center_s: f64,
        sigma_s: f64,
    },
    /// Tapered cosine; `alpha` is the fraction of the duration spent tapering,
    /// so 0 is rectangular and 1 is a Hann window.
    Tukey {
        alpha: f64,
    },
}

impl EnvelopeSpec {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Adsr { .. } => "ADSR",
            Self::Gaussian { .. } => "Gaussian",
            Self::Tukey { .. } => "Tukey",
        }
    }
}

/// How [`Node::Modulate`] combines its carrier and modulator.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum ModKind {
    /// `carrier(t) * (1 + depth * m(t))`. Accepts any carrier.
    Am { depth: f64 },
    /// Frequency modulation: the carrier's phase advances by
    /// `2*pi * dev_hz * integral of m from 0 to t`. Needs an oscillator
    /// carrier and an integrable modulator (see [`Node::is_integrable`]).
    Fm { dev_hz: f64 },
    /// Phase modulation: `dev_rad * m(t)` is added to the carrier's phase.
    /// Needs an oscillator carrier.
    Pm { dev_rad: f64 },
}

impl ModKind {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Am { .. } => "AM",
            Self::Fm { .. } => "FM",
            Self::Pm { .. } => "PM",
        }
    }

    /// Whether the carrier's phase is rewritten, which only an oscillator
    /// primitive can express.
    #[must_use]
    pub const fn needs_oscillator_carrier(&self) -> bool {
        matches!(self, Self::Fm { .. } | Self::Pm { .. })
    }
}

/// One node of the generator DAG (§8.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "node")]
pub enum Node {
    // -- Primitives ------------------------------------------------------
    Sine {
        freq_hz: f64,
        amp: f64,
        phase_rad: f64,
        offset: f64,
    },
    Square {
        freq_hz: f64,
        amp: f64,
        duty: f64,
        phase_rad: f64,
    },
    Triangle {
        freq_hz: f64,
        amp: f64,
        phase_rad: f64,
    },
    Sawtooth {
        freq_hz: f64,
        amp: f64,
        rising: bool,
    },
    /// A trapezoidal pulse train: `rise_s` up, `width_s` at `amp`, `fall_s`
    /// down, then zero until the next period.
    Pulse {
        period_s: f64,
        width_s: f64,
        amp: f64,
        rise_s: f64,
        fall_s: f64,
    },
    Chirp {
        f0_hz: f64,
        f1_hz: f64,
        sweep: Sweep,
        amp: f64,
    },
    Dc {
        level: f64,
    },
    /// A straight line from `start` to `end` across the spec's duration.
    Ramp {
        start: f64,
        end: f64,
    },
    Step {
        at_s: f64,
        before: f64,
        after: f64,
    },
    /// A single non-zero sample, at the index nearest `at_s`.
    Impulse {
        at_s: f64,
        amp: f64,
    },
    Noise {
        kind: NoiseKind,
        amp: f64,
    },
    /// Maximal-length LFSR sequence at one bit per sample, as `+/-amp`.
    Prbs {
        order: u8,
        /// Feedback tap mask; `None` uses the built-in maximal-length
        /// polynomial for `order`.
        taps: Option<u32>,
        amp: f64,
    },
    /// `f(t)` with `t` in seconds, evaluated by [`crate::expr`].
    Expr {
        source: String,
    },

    // -- Combinators -----------------------------------------------------
    Sum {
        terms: Vec<Node>,
    },
    Product {
        terms: Vec<Node>,
    },
    /// Parts laid end to end; past the last part the output is zero. Each
    /// part sees `t` from its own start.
    Concat {
        parts: Vec<ConcatPart>,
    },
    Gain {
        input: Box<Node>,
        factor: f64,
    },
    /// `input(t - by_s)`, zero before `by_s`.
    Delay {
        input: Box<Node>,
        by_s: f64,
    },
    Clip {
        input: Box<Node>,
        lo: f64,
        hi: f64,
    },
    Envelope {
        input: Box<Node>,
        env: EnvelopeSpec,
    },
    Modulate {
        carrier: Box<Node>,
        modulator: Box<Node>,
        kind: ModKind,
    },
    /// Samples `input` at `to_rate_hz` and reads it back with linear
    /// interpolation - a decimation, aliasing included.
    Resample {
        input: Box<Node>,
        to_rate_hz: f64,
    },
    /// Reads a stored signal as a source, aligned by absolute time.
    FromSignal {
        signal_id: SignalId,
    },
}

/// One entry of a [`Node::Concat`]. A struct rather than the design's tuple so
/// the JSON names its fields, which is what the parameter form and the sweep
/// pointer address (§8.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConcatPart {
    pub node: Node,
    pub duration_s: f64,
}

impl Default for Node {
    fn default() -> Self {
        Self::Sine {
            freq_hz: 1_000.0,
            amp: 1.0,
            phase_rad: 0.0,
            offset: 0.0,
        }
    }
}

/// The identity of a node variant, for menus and for building a default node
/// of a kind the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Sine,
    Square,
    Triangle,
    Sawtooth,
    Pulse,
    Chirp,
    Dc,
    Ramp,
    Step,
    Impulse,
    Noise,
    Prbs,
    Expr,
    Sum,
    Product,
    Concat,
    Gain,
    Delay,
    Clip,
    Envelope,
    Modulate,
    Resample,
    FromSignal,
}

impl NodeKind {
    /// Primitives first, then combinators, which is the order the palette
    /// shows them in.
    pub const ALL: [Self; 23] = [
        Self::Sine,
        Self::Square,
        Self::Triangle,
        Self::Sawtooth,
        Self::Pulse,
        Self::Chirp,
        Self::Dc,
        Self::Ramp,
        Self::Step,
        Self::Impulse,
        Self::Noise,
        Self::Prbs,
        Self::Expr,
        Self::Sum,
        Self::Product,
        Self::Concat,
        Self::Gain,
        Self::Delay,
        Self::Clip,
        Self::Envelope,
        Self::Modulate,
        Self::Resample,
        Self::FromSignal,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Square => "Square",
            Self::Triangle => "Triangle",
            Self::Sawtooth => "Sawtooth",
            Self::Pulse => "Pulse",
            Self::Chirp => "Chirp",
            Self::Dc => "DC",
            Self::Ramp => "Ramp",
            Self::Step => "Step",
            Self::Impulse => "Impulse",
            Self::Noise => "Noise",
            Self::Prbs => "PRBS",
            Self::Expr => "Expression",
            Self::Sum => "Sum",
            Self::Product => "Product",
            Self::Concat => "Concat",
            Self::Gain => "Gain",
            Self::Delay => "Delay",
            Self::Clip => "Clip",
            Self::Envelope => "Envelope",
            Self::Modulate => "Modulate",
            Self::Resample => "Resample",
            Self::FromSignal => "From signal",
        }
    }

    /// Whether the variant combines other nodes, which is what the tree
    /// editor uses to decide if a node can take children.
    #[must_use]
    pub const fn is_combinator(self) -> bool {
        matches!(
            self,
            Self::Sum
                | Self::Product
                | Self::Concat
                | Self::Gain
                | Self::Delay
                | Self::Clip
                | Self::Envelope
                | Self::Modulate
                | Self::Resample
        )
    }

    /// Whether a child can be added to or removed from a node of this kind.
    /// The fixed-arity combinators replace a child instead.
    #[must_use]
    pub const fn has_variable_arity(self) -> bool {
        matches!(self, Self::Sum | Self::Product | Self::Concat)
    }

    /// A node of this kind with sensible starting values.
    #[must_use]
    pub fn default_node(self) -> Node {
        let child = || Box::new(Node::default());
        match self {
            Self::Sine => Node::default(),
            Self::Square => Node::Square {
                freq_hz: 1_000.0,
                amp: 1.0,
                duty: 0.5,
                phase_rad: 0.0,
            },
            Self::Triangle => Node::Triangle {
                freq_hz: 1_000.0,
                amp: 1.0,
                phase_rad: 0.0,
            },
            Self::Sawtooth => Node::Sawtooth {
                freq_hz: 1_000.0,
                amp: 1.0,
                rising: true,
            },
            Self::Pulse => Node::Pulse {
                period_s: 1e-3,
                width_s: 1e-4,
                amp: 1.0,
                rise_s: 0.0,
                fall_s: 0.0,
            },
            Self::Chirp => Node::Chirp {
                f0_hz: 100.0,
                f1_hz: 2_000.0,
                sweep: Sweep::Linear,
                amp: 1.0,
            },
            Self::Dc => Node::Dc { level: 1.0 },
            Self::Ramp => Node::Ramp {
                start: 0.0,
                end: 1.0,
            },
            Self::Step => Node::Step {
                at_s: 0.0,
                before: 0.0,
                after: 1.0,
            },
            Self::Impulse => Node::Impulse {
                at_s: 0.0,
                amp: 1.0,
            },
            Self::Noise => Node::Noise {
                kind: NoiseKind::Gaussian,
                amp: 0.1,
            },
            Self::Prbs => Node::Prbs {
                order: 9,
                taps: None,
                amp: 1.0,
            },
            Self::Expr => Node::Expr {
                source: "sin(2*pi*1000*t)".to_owned(),
            },
            Self::Sum => Node::Sum {
                terms: vec![Node::default()],
            },
            Self::Product => Node::Product {
                terms: vec![Node::default()],
            },
            Self::Concat => Node::Concat {
                parts: vec![ConcatPart {
                    node: Node::default(),
                    duration_s: 1.0,
                }],
            },
            Self::Gain => Node::Gain {
                input: child(),
                factor: 1.0,
            },
            Self::Delay => Node::Delay {
                input: child(),
                by_s: 0.0,
            },
            Self::Clip => Node::Clip {
                input: child(),
                lo: -1.0,
                hi: 1.0,
            },
            Self::Envelope => Node::Envelope {
                input: child(),
                env: EnvelopeSpec::Tukey { alpha: 0.1 },
            },
            Self::Modulate => Node::Modulate {
                carrier: child(),
                modulator: Box::new(Node::Sine {
                    freq_hz: 50.0,
                    amp: 1.0,
                    phase_rad: 0.0,
                    offset: 0.0,
                }),
                kind: ModKind::Am { depth: 0.5 },
            },
            Self::Resample => Node::Resample {
                input: child(),
                to_rate_hz: 8_000.0,
            },
            Self::FromSignal => Node::FromSignal {
                signal_id: SignalId::new(0),
            },
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A periodic primitive seen as an oscillator, which is what FM and PM need in
/// order to rewrite a carrier's phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oscillator {
    pub shape: OscShape,
    pub freq_hz: f64,
    pub amp: f64,
    pub phase_rad: f64,
    /// Duty cycle; meaningful for [`OscShape::Square`] only.
    pub duty: f64,
    pub offset: f64,
}

/// The waveshaper an [`Oscillator`] applies to its phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscShape {
    Sine,
    Square,
    Triangle,
    SawRising,
    SawFalling,
}

impl Node {
    #[must_use]
    pub const fn kind(&self) -> NodeKind {
        match self {
            Self::Sine { .. } => NodeKind::Sine,
            Self::Square { .. } => NodeKind::Square,
            Self::Triangle { .. } => NodeKind::Triangle,
            Self::Sawtooth { .. } => NodeKind::Sawtooth,
            Self::Pulse { .. } => NodeKind::Pulse,
            Self::Chirp { .. } => NodeKind::Chirp,
            Self::Dc { .. } => NodeKind::Dc,
            Self::Ramp { .. } => NodeKind::Ramp,
            Self::Step { .. } => NodeKind::Step,
            Self::Impulse { .. } => NodeKind::Impulse,
            Self::Noise { .. } => NodeKind::Noise,
            Self::Prbs { .. } => NodeKind::Prbs,
            Self::Expr { .. } => NodeKind::Expr,
            Self::Sum { .. } => NodeKind::Sum,
            Self::Product { .. } => NodeKind::Product,
            Self::Concat { .. } => NodeKind::Concat,
            Self::Gain { .. } => NodeKind::Gain,
            Self::Delay { .. } => NodeKind::Delay,
            Self::Clip { .. } => NodeKind::Clip,
            Self::Envelope { .. } => NodeKind::Envelope,
            Self::Modulate { .. } => NodeKind::Modulate,
            Self::Resample { .. } => NodeKind::Resample,
            Self::FromSignal { .. } => NodeKind::FromSignal,
        }
    }

    /// The node's children, in the order the tree editor lists them, each with
    /// the label it is shown under.
    #[must_use]
    pub fn children(&self) -> Vec<(&'static str, &Self)> {
        match self {
            Self::Sum { terms } | Self::Product { terms } => {
                terms.iter().map(|term| ("term", term)).collect()
            }
            Self::Concat { parts } => parts.iter().map(|part| ("part", &part.node)).collect(),
            Self::Gain { input, .. }
            | Self::Delay { input, .. }
            | Self::Clip { input, .. }
            | Self::Envelope { input, .. }
            | Self::Resample { input, .. } => vec![("input", input)],
            Self::Modulate {
                carrier, modulator, ..
            } => vec![("carrier", carrier), ("modulator", modulator)],
            _ => Vec::new(),
        }
    }

    /// Every node in the tree, parents before children.
    pub fn walk(&self) -> impl Iterator<Item = &Self> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            for (_, child) in node.children().into_iter().rev() {
                stack.push(child);
            }
            Some(node)
        })
    }

    /// The node seen as an oscillator, when it is a periodic primitive.
    #[must_use]
    pub fn as_oscillator(&self) -> Option<Oscillator> {
        let osc = |shape, freq_hz, amp, phase_rad, duty, offset| Oscillator {
            shape,
            freq_hz,
            amp,
            phase_rad,
            duty,
            offset,
        };
        match *self {
            Self::Sine {
                freq_hz,
                amp,
                phase_rad,
                offset,
            } => Some(osc(OscShape::Sine, freq_hz, amp, phase_rad, 0.5, offset)),
            Self::Square {
                freq_hz,
                amp,
                duty,
                phase_rad,
            } => Some(osc(OscShape::Square, freq_hz, amp, phase_rad, duty, 0.0)),
            Self::Triangle {
                freq_hz,
                amp,
                phase_rad,
            } => Some(osc(OscShape::Triangle, freq_hz, amp, phase_rad, 0.5, 0.0)),
            Self::Sawtooth {
                freq_hz,
                amp,
                rising,
            } => {
                let shape = if rising {
                    OscShape::SawRising
                } else {
                    OscShape::SawFalling
                };
                Some(osc(shape, freq_hz, amp, 0.0, 0.5, 0.0))
            }
            _ => None,
        }
    }

    /// Whether this node's integral from zero has a closed form, which is what
    /// FM needs of its modulator. See [`crate::waveform::integral_at`].
    #[must_use]
    pub fn is_integrable(&self) -> bool {
        match self {
            Self::Sine { .. }
            | Self::Square { .. }
            | Self::Triangle { .. }
            | Self::Sawtooth { .. }
            | Self::Dc { .. }
            | Self::Ramp { .. }
            | Self::Step { .. } => true,
            Self::Sum { terms } => terms.iter().all(Self::is_integrable),
            Self::Gain { input, .. } | Self::Delay { input, .. } => input.is_integrable(),
            _ => false,
        }
    }
}

/// A complete generator spec (§8.1).
///
/// The design writes the duration into `Timebase`; `sp_core::Timebase` carries
/// only the rate and `t0`, because a stored signal's length is its sample
/// count. `duration_s` is therefore a field of its own here, and
/// [`GenSpec::sample_count`] is how the two meet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenSpec {
    pub timebase: Timebase,
    pub duration_s: f64,
    pub dtype: DType,
    pub domain: Domain,
    /// ChaCha12 seed; makes noise reproducible (G3).
    pub seed: u64,
    pub root: Node,
}

impl GenSpec {
    /// A spec of `duration_s` seconds at `sample_rate_hz`, holding `root`.
    #[must_use]
    pub fn new(sample_rate_hz: f64, duration_s: f64, root: Node) -> Self {
        Self {
            timebase: Timebase::regular(sample_rate_hz, 0.0),
            duration_s,
            dtype: DType::F32,
            domain: Domain::Analog,
            seed: 0,
            root,
        }
    }

    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    #[must_use]
    pub fn with_dtype(mut self, dtype: DType) -> Self {
        self.dtype = dtype;
        self
    }

    #[must_use]
    pub fn with_domain(mut self, domain: Domain) -> Self {
        self.domain = domain;
        self
    }

    /// The sample rate, or `f64::NAN` for the irregular timebase a spec is
    /// never valid with. [`crate::validate`] rejects that case, so the
    /// renderer may assume a rate.
    #[must_use]
    pub fn sample_rate_hz(&self) -> f64 {
        self.timebase.sample_rate_hz.unwrap_or(f64::NAN)
    }

    /// How many samples the spec renders to; zero when it cannot render.
    #[must_use]
    pub fn sample_count(&self) -> u64 {
        let rate = self.sample_rate_hz();
        if !rate.is_finite()
            || rate <= 0.0
            || !self.duration_s.is_finite()
            || self.duration_s <= 0.0
        {
            return 0;
        }
        (self.duration_s * rate).round().max(0.0) as u64
    }

    #[must_use]
    pub fn nyquist_hz(&self) -> f64 {
        self.sample_rate_hz() / 2.0
    }

    /// The spec as pretty JSON, which is what a preset file and
    /// `signal.gen_spec` hold.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

impl Default for GenSpec {
    fn default() -> Self {
        Self::new(48_000.0, 0.1, Node::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_kind_builds_a_node_of_that_kind() {
        for kind in NodeKind::ALL {
            assert_eq!(kind.default_node().kind(), kind, "{kind}");
        }
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let spec = GenSpec::new(
            48_000.0,
            0.25,
            Node::Sum {
                terms: vec![
                    Node::default(),
                    Node::Noise {
                        kind: NoiseKind::Pink,
                        amp: 0.1,
                    },
                ],
            },
        )
        .with_seed(7);
        let json = spec.to_json().unwrap();
        assert_eq!(GenSpec::from_json(&json).unwrap(), spec);
    }

    #[test]
    fn sample_count_follows_the_rate_and_duration() {
        assert_eq!(
            GenSpec::new(1_000.0, 0.5, Node::default()).sample_count(),
            500
        );
        // A spec that cannot render is zero samples rather than a panic.
        let mut spec = GenSpec::new(1_000.0, 0.5, Node::default());
        spec.duration_s = -1.0;
        assert_eq!(spec.sample_count(), 0);
        spec.timebase = Timebase::irregular(0.0);
        assert_eq!(spec.sample_count(), 0);
    }

    #[test]
    fn walk_visits_every_node_parents_first() {
        let node = NodeKind::Modulate.default_node();
        let kinds: Vec<_> = node.walk().map(Node::kind).collect();
        assert_eq!(kinds, [NodeKind::Modulate, NodeKind::Sine, NodeKind::Sine]);
    }

    #[test]
    fn only_periodic_primitives_are_oscillators() {
        assert!(NodeKind::Sine.default_node().as_oscillator().is_some());
        assert!(NodeKind::Square.default_node().as_oscillator().is_some());
        assert!(NodeKind::Noise.default_node().as_oscillator().is_none());
        assert!(NodeKind::Sum.default_node().as_oscillator().is_none());
    }

    #[test]
    fn integrability_follows_the_tree() {
        assert!(NodeKind::Dc.default_node().is_integrable());
        assert!(Node::Sum {
            terms: vec![NodeKind::Dc.default_node(), NodeKind::Sine.default_node()],
        }
        .is_integrable());
        assert!(!Node::Sum {
            terms: vec![NodeKind::Dc.default_node(), NodeKind::Noise.default_node()],
        }
        .is_integrable());
    }
}
