//! Digital stages: slice, symbol decode, bit pack (`docs/DESIGN.md` §9.8).
//!
//! Three stages that turn a waveform into a bitstream, and they are three
//! rather than one on purpose: each step is a chip on the rail with its own
//! output, so "the bits are wrong" can be answered with *where* — at the
//! slicer's level, at the clock's phase, or at the packer's bit order.
//!
//! ```text
//! analog ──slice──► logic ──symbols──► Symbols ──bits──► Bits
//! ```
//!
//! [`SymbolDecode`] publishes an artifact rather than a signal. A group's
//! signals share its timebase (§9.3), and symbols are on the symbol clock,
//! not on the sample clock — so a symbol-rate signal added to the group would
//! be recorded with the wrong timebase. The artifact carries its own instants
//! and draws as stems over the waveform it was sliced from, which is what a
//! reader wanted to see anyway.

use sp_core::artifact::Artifact;
use sp_core::{Attributes, Diagnostic, Domain};
use sp_proc::error::{ConfigError, StageError};
use sp_proc::param::{ParamDefault, ParamKind, ParamSet, ParamSpec};
use sp_proc::stage::{
    ArtifactOut, PortKind, PortSpec, PropertyPatch, SignalOut, Stage, StageCtx, StageDescriptor,
    StageOutput,
};
use sp_proc::{GroupFrame, SignalRef};

use crate::artifacts::{Bits, Symbols};
use crate::{SIGNALS_IN, SIGNALS_OUT};

/// The group's signal a stage works on: the one named, or the first.
fn chosen<'a>(input: &'a GroupFrame, name: &str) -> Option<&'a SignalRef> {
    if name.is_empty() {
        input.signals.first()
    } else {
        input.signal_named(name)
    }
}

/// The diagnostic for a name that matched nothing, which names what was
/// there — an error that says "not found" and stops is half an error.
fn no_such_signal(input: &GroupFrame, name: &str) -> Diagnostic {
    let had: Vec<&str> = input.signals.iter().map(SignalRef::name).collect();
    Diagnostic::warn(format!(
        "no signal named '{name}' in this group; it has {}",
        if had.is_empty() {
            "none".to_owned()
        } else {
            had.join(", ")
        }
    ))
}

// ---------------------------------------------------------------- slice ----

const SLICE_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("signal", "Signal", ParamKind::Text, ParamDefault::Text(""))
        .with_help("Which signal to slice. Empty means the first in the group."),
    ParamSpec::new(
        "level",
        "Decision level",
        ParamKind::Float {
            min: None,
            max: None,
        },
        ParamDefault::Float(0.0),
    ),
    ParamSpec::new(
        "hysteresis",
        "Hysteresis",
        ParamKind::Float {
            min: Some(0.0),
            max: None,
        },
        ParamDefault::Float(0.0),
    )
    .with_help("How far past the level the signal must come back before the output changes."),
    ParamSpec::new(
        "invert",
        "Invert",
        ParamKind::Bool,
        ParamDefault::Bool(false),
    ),
    ParamSpec::new(
        "name",
        "Output name",
        ParamKind::Text,
        ParamDefault::Text("logic"),
    ),
];

static SLICE: StageDescriptor = StageDescriptor::new("dsp.digital.slice", 1, "Slice")
    .describing("Turns a waveform into a two-level logic signal.")
    .reading(SIGNALS_IN)
    .writing(SIGNALS_OUT)
    .taking(SLICE_PARAMS);

/// A comparator with hysteresis: one analog signal in, one logic signal added
/// beside it.
///
/// The input stays in the group. A slicer that replaced its input would throw
/// away the only evidence of *why* a bit went the way it did, which is the
/// opposite of what the rail is for.
#[derive(Debug)]
pub struct Slice {
    signal: String,
    level: f64,
    hysteresis: f64,
    invert: bool,
    name: String,
}

impl Default for Slice {
    fn default() -> Self {
        Self {
            signal: String::new(),
            level: 0.0,
            hysteresis: 0.0,
            invert: false,
            name: "logic".to_owned(),
        }
    }
}

impl Slice {
    /// The logic levels of `values`, as 0.0 and 1.0.
    fn slice(&self, values: &[f64]) -> Vec<f64> {
        let mut high = false;
        let mut out = Vec::with_capacity(values.len());
        for &value in values {
            // The band is centred on the level, so hysteresis costs symmetry
            // rather than a shifted decision point.
            let bound = if high {
                self.level - self.hysteresis / 2.0
            } else {
                self.level + self.hysteresis / 2.0
            };
            if value > bound {
                high = true;
            } else if value < bound {
                high = false;
            }
            // A NaN leaves the output where it was: the comparator holds.
            out.push(f64::from(u8::from(high != self.invert)));
        }
        out
    }
}

impl Stage for Slice {
    fn descriptor(&self) -> &'static StageDescriptor {
        &SLICE
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(SLICE_PARAMS)?;
        self.signal = params.str_or("signal", "").to_owned();
        self.level = params.f64_or("level", 0.0);
        self.hysteresis = params.f64_or("hysteresis", 0.0);
        self.invert = params.bool_or("invert", false);
        self.name = params.str_or("name", "logic").trim().to_owned();
        if self.name.is_empty() {
            return Err(ConfigError::rejected("the output signal needs a name"));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        ctx.check()?;
        let mut output = StageOutput::passthrough_of(input);
        let Some(source) = chosen(input, &self.signal) else {
            if !input.signals.is_empty() {
                output.diagnose(no_such_signal(input, &self.signal));
            }
            return Ok(output);
        };
        if source.is_empty() {
            output.diagnose(Diagnostic::info(format!(
                "'{}' has no samples to slice",
                source.name()
            )));
            return Ok(output);
        }

        let values = source.read_values()?;
        ctx.check()?;
        let logic = self.slice(&values);
        let transitions = logic.windows(2).filter(|pair| pair[0] != pair[1]).count();
        let high = logic.iter().filter(|&&v| v > 0.5).count();

        let mut attrs = Attributes::new();
        attrs.insert("sliced_from", source.name());
        attrs.insert("level", self.level);
        output.signals.push(SignalOut::Add {
            name: self.name.clone(),
            domain: Domain::DigitalLogic,
            samples: crate::buffer_like(source, &logic),
            attrs,
        });
        output.metric("transitions", transitions as f64);
        output.metric("duty", high as f64 / logic.len() as f64);
        Ok(output)
    }
}

// -------------------------------------------------------------- symbols ----

const SYMBOL_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("signal", "Signal", ParamKind::Text, ParamDefault::Text(""))
        .with_help("Which signal to decode. Empty means the first in the group."),
    ParamSpec::required(
        "symbol_rate_hz",
        "Symbol rate",
        ParamKind::FreqHz {
            min: Some(0.0),
            max: None,
        },
    )
    .with_unit("Hz"),
    ParamSpec::new(
        "format",
        "Format",
        ParamKind::Enum {
            variants: &["nrz", "pam4"],
        },
        ParamDefault::Text("nrz"),
    )
    .with_help("Two levels, or four."),
    ParamSpec::new(
        "level",
        "Centre",
        ParamKind::Float {
            min: None,
            max: None,
        },
        ParamDefault::Float(0.0),
    )
    .with_help("The middle of the eye: the decision level for NRZ."),
    ParamSpec::new(
        "spacing",
        "Level spacing",
        ParamKind::Float {
            min: Some(f64::MIN_POSITIVE),
            max: None,
        },
        ParamDefault::Float(2.0),
    )
    .with_help("Between adjacent nominal levels. NRZ at ±1 is a spacing of 2."),
    ParamSpec::new(
        "align",
        "Clock",
        ParamKind::Enum {
            variants: &["first-edge", "start"],
        },
        ParamDefault::Text("first-edge"),
    )
    .with_help("Recover the phase from the first crossing, or start at the first sample."),
    ParamSpec::new(
        "phase",
        "Sampling phase",
        ParamKind::Float {
            min: Some(0.0),
            max: Some(1.0),
        },
        ParamDefault::Float(0.5),
    )
    .with_help("Where in the symbol period to decide. 0.5 is the centre of the eye."),
];

const SYMBOL_OUTPUTS: &[PortSpec] = &[
    PortSpec::required("symbols", PortKind::Artifact("symbols.v1")),
    PortSpec::required("signals", PortKind::ANY_SIGNALS),
];

static SYMBOLS: StageDescriptor = StageDescriptor::new("dsp.digital.symbols", 1, "Symbol decode")
    .describing("Samples one symbol per clock period and decides which level it is.")
    .reading(SIGNALS_IN)
    .writing(SYMBOL_OUTPUTS)
    .taking(SYMBOL_PARAMS);

/// Decides one symbol per clock period, and records how much room each
/// decision had.
#[derive(Debug)]
pub struct SymbolDecode {
    signal: String,
    symbol_rate_hz: f64,
    levels: u32,
    level: f64,
    spacing: f64,
    from_first_edge: bool,
    phase: f64,
}

impl Default for SymbolDecode {
    fn default() -> Self {
        Self {
            signal: String::new(),
            symbol_rate_hz: 0.0,
            levels: 2,
            level: 0.0,
            spacing: 2.0,
            from_first_edge: true,
            phase: 0.5,
        }
    }
}

impl SymbolDecode {
    /// The nominal value of level `k`, counting from the lowest.
    fn nominal(&self, k: u32) -> f64 {
        let offset = f64::from(k) - f64::from(self.levels - 1) / 2.0;
        self.level + offset * self.spacing
    }

    /// The decision boundaries between adjacent levels.
    fn boundaries(&self) -> Vec<f64> {
        (0..self.levels - 1)
            .map(|k| (self.nominal(k) + self.nominal(k + 1)) / 2.0)
            .collect()
    }

    /// Which level `value` is, and how far it sat from the nearest boundary.
    fn decide(&self, value: f64, boundaries: &[f64]) -> (i64, f64) {
        let symbol = boundaries.iter().filter(|&&b| value > b).count() as i64;
        let margin = boundaries
            .iter()
            .map(|b| (value - b).abs())
            .fold(f64::INFINITY, f64::min);
        (symbol, margin)
    }

    /// The instant the first symbol is decided at.
    ///
    /// Aligned to the first crossing of the centre level when asked for, which
    /// is clock recovery enough for a capture that starts mid-idle: the phase
    /// comes from the data rather than from where the file happened to begin.
    fn first_instant(&self, values: &[f64], t0: f64, period: f64) -> f64 {
        let width = 1.0 / self.symbol_rate_hz;
        if !self.from_first_edge {
            return t0 + self.phase * width;
        }
        let edge = values.windows(2).position(|pair| {
            (pair[0] < self.level && pair[1] >= self.level)
                || (pair[0] >= self.level && pair[1] < self.level)
        });
        let Some(edge) = edge else {
            return t0 + self.phase * width;
        };
        // The crossing is a symbol boundary; step back to the first whole
        // symbol that still starts inside the capture.
        let mut at = t0 + (edge + 1) as f64 * period + self.phase * width;
        while at - width >= t0 {
            at -= width;
        }
        at
    }
}

impl Stage for SymbolDecode {
    fn descriptor(&self) -> &'static StageDescriptor {
        &SYMBOLS
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(SYMBOL_PARAMS)?;
        self.signal = params.str_or("signal", "").to_owned();
        self.symbol_rate_hz = params.f64_or("symbol_rate_hz", 0.0);
        if self.symbol_rate_hz <= 0.0 || self.symbol_rate_hz.is_nan() {
            return Err(ConfigError::rejected("the symbol rate must be above zero"));
        }
        self.levels = if params.str_or("format", "nrz") == "pam4" {
            4
        } else {
            2
        };
        self.level = params.f64_or("level", 0.0);
        self.spacing = params.f64_or("spacing", 2.0);
        if self.spacing <= 0.0 || self.spacing.is_nan() {
            return Err(ConfigError::rejected(
                "the level spacing must be above zero",
            ));
        }
        self.from_first_edge = params.str_or("align", "first-edge") != "start";
        self.phase = params.f64_or("phase", 0.5);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        ctx.check()?;
        let mut output = StageOutput::passthrough_of(input);
        let source = chosen(input, &self.signal);
        let mut symbols = Symbols::new(source.map_or("", SignalRef::name), self.levels);

        match source {
            Some(source) if !source.is_empty() => {
                self.decode(ctx, source, &mut symbols)?;
                if symbols.is_empty() {
                    let held = source
                        .timebase()
                        .duration_s(source.sample_count())
                        .unwrap_or(0.0);
                    output.diagnose(Diagnostic::warn(format!(
                        "{held:.6} s of '{}' is less than one symbol at {:.1} Hz",
                        source.name(),
                        self.symbol_rate_hz
                    )));
                }
            }
            Some(source) => output.diagnose(Diagnostic::info(format!(
                "'{}' has no samples to decode",
                source.name()
            ))),
            None => {
                if !input.signals.is_empty() {
                    output.diagnose(no_such_signal(input, &self.signal));
                }
            }
        }

        output.metric("symbols", symbols.len() as f64);
        output.metric("worst_margin", symbols.worst_margin().unwrap_or(0.0));
        output.metric(
            "mean_margin",
            if symbols.is_empty() {
                0.0
            } else {
                symbols.margin.iter().sum::<f64>() / symbols.len() as f64
            },
        );
        output
            .properties
            .push(PropertyPatch::group("symbol_count", symbols.len() as i64));
        output
            .artifacts
            .push(ArtifactOut::publish("symbols", &symbols)?);
        Ok(output)
    }
}

impl SymbolDecode {
    fn decode(
        &self,
        ctx: &StageCtx,
        source: &SignalRef,
        into: &mut Symbols,
    ) -> Result<(), StageError> {
        let timebase = source.timebase();
        let Some(period) = timebase.sample_period_s() else {
            return Err(StageError::rejected(
                "symbol decode needs a regular timebase; this group is irregular",
            ));
        };
        let width = 1.0 / self.symbol_rate_hz;
        if width < period {
            return Err(StageError::rejected(format!(
                "a symbol rate of {:.1} Hz is faster than the {:.1} Hz sample rate",
                self.symbol_rate_hz,
                1.0 / period
            )));
        }

        let count = source.sample_count();
        let values = source.read_values()?;
        let t0 = timebase.time_of(0).unwrap_or(0.0);
        let end = timebase.time_of(count.saturating_sub(1)).unwrap_or(t0);
        let boundaries = self.boundaries();

        let mut at = self.first_instant(&values, t0, period);
        while at <= end {
            ctx.check()?;
            let Some(index) = timebase.index_at(at, count) else {
                break;
            };
            let value = values[index as usize];
            // A NaN at the decision instant is not a symbol; recording one
            // would put a hole in the bitstream that nothing downstream could
            // see.
            if value.is_nan() {
                at += width;
                continue;
            }
            let (symbol, margin) = self.decide(value, &boundaries);
            into.push(at, value, symbol, margin);
            at += width;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- bits ----

const BIT_PARAMS: &[ParamSpec] = &[
    ParamSpec::new(
        "width",
        "Word width",
        ParamKind::Int {
            min: Some(1),
            max: Some(64),
        },
        ParamDefault::Int(8),
    )
    .with_unit("bits"),
    ParamSpec::new(
        "msb_first",
        "First bit is the most significant",
        ParamKind::Bool,
        ParamDefault::Bool(true),
    ),
    ParamSpec::new(
        "invert",
        "Invert",
        ParamKind::Bool,
        ParamDefault::Bool(false),
    ),
];

const BIT_INPUTS: &[PortSpec] = &[
    PortSpec::required("signals", PortKind::ANY_SIGNALS),
    PortSpec::required("symbols", PortKind::Artifact("symbols.v1")),
];

const BIT_OUTPUTS: &[PortSpec] = &[PortSpec::required("bits", PortKind::Artifact("bits.v1"))];

static BITS: StageDescriptor = StageDescriptor::new("dsp.digital.bits", 1, "Bit pack")
    .describing("Packs decoded symbols into words.")
    .reading(BIT_INPUTS)
    .writing(BIT_OUTPUTS)
    .taking(BIT_PARAMS);

/// Packs the symbols an upstream stage decoded into words.
///
/// It reads a `symbols.v1` artifact rather than samples, which is the typed
/// port model doing its job (§9.3): the packer never has to know what a
/// waveform looks like, and the decoder never has to know about bit order.
#[derive(Debug)]
pub struct BitPack {
    width: u32,
    msb_first: bool,
    invert: bool,
}

impl Default for BitPack {
    fn default() -> Self {
        Self {
            width: 8,
            msb_first: true,
            invert: false,
        }
    }
}

impl BitPack {
    /// The bits of `symbols`, in the order they were decoded.
    fn stream(&self, symbols: &Symbols) -> Vec<u8> {
        let per = symbols.bits_per_symbol();
        let mut bits = Vec::with_capacity(symbols.len() * per as usize);
        for &symbol in &symbols.symbol {
            let value = symbol.max(0) as u64;
            // Most significant bit of the symbol first, so a PAM-4 level of 2
            // is `10` rather than `01`.
            for bit in (0..per).rev() {
                let set = value >> bit & 1 == 1;
                bits.push(u8::from(set != self.invert));
            }
        }
        bits
    }

    fn pack(&self, symbols: &Symbols) -> Bits {
        let mut packed = Bits::new(symbols.signal.clone(), self.width);
        for word in self.stream(symbols).chunks(self.width as usize) {
            let mut value = 0u64;
            for (position, &bit) in word.iter().enumerate() {
                let shift = if self.msb_first {
                    // A short final word stays left-aligned within its own
                    // width, so the first bit is still the one written first.
                    word.len() - 1 - position
                } else {
                    position
                };
                value |= u64::from(bit) << shift;
            }
            packed.push(value, word.len() as u32);
        }
        packed
    }
}

impl Stage for BitPack {
    fn descriptor(&self) -> &'static StageDescriptor {
        &BITS
    }

    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        params.validate(BIT_PARAMS)?;
        self.width = u32::try_from(params.i64_or("width", 8)).unwrap_or(8);
        self.msb_first = params.bool_or("msb_first", true);
        self.invert = params.bool_or("invert", false);
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        ctx.check()?;
        let mut output = StageOutput::passthrough_of(input);
        let port = input.inbound.latest_of_kind(Symbols::KIND).ok_or_else(|| {
            StageError::MissingInput {
                port: "symbols".to_owned(),
            }
        })?;
        let symbols: Symbols = port.read()?;

        let packed = self.pack(&symbols);
        output.metric("bits", packed.count as f64);
        output.metric("words", packed.len() as f64);
        output
            .properties
            .push(PropertyPatch::group("bit_count", packed.count as i64));
        if packed.is_empty() {
            output.diagnose(Diagnostic::info("no symbols to pack"));
        }
        output
            .artifacts
            .push(ArtifactOut::publish("bits", &packed)?);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use sp_core::artifact::Artifact;
    use sp_proc::frame::PortValue;

    use super::*;
    use crate::tests::{frame, process, try_process, values_of};

    fn configured<S: Stage + Default>(params: ParamSet) -> S {
        let mut stage = S::default();
        stage.configure(&params).unwrap();
        stage
    }

    /// An NRZ waveform at ±1, ten samples per symbol on the 1 kHz test
    /// timebase — so a 100 Hz symbol rate.
    fn nrz(bits: &[u8]) -> Vec<f64> {
        bits.iter()
            .flat_map(|&bit| [if bit == 1 { 1.0 } else { -1.0 }; 10])
            .collect()
    }

    fn artifact_of<A: serde::de::DeserializeOwned>(output: &StageOutput) -> A {
        serde_json::from_str(&output.artifacts[0].payload_json).unwrap()
    }

    /// A frame carrying an artifact on an inbound port, as the stage before
    /// it would have left things.
    fn with_inbound<A: Artifact>(mut input: GroupFrame, port: &str, value: &A) -> GroupFrame {
        input.inbound.publish(PortValue {
            port: port.to_owned(),
            kind: A::KIND.to_owned(),
            kind_version: A::VERSION,
            payload_json: serde_json::to_string(value).unwrap(),
            summary: Some(value.summary()),
            stage_ordinal: 0,
        });
        input
    }

    // ------------------------------------------------------------ slice ----

    #[test]
    fn slicing_adds_a_logic_signal_and_leaves_the_waveform_alone() {
        let mut stage = configured::<Slice>(ParamSet::new());
        let input = frame(&[("rf", vec![-1.0, 1.0, 1.0, -1.0])]);
        let (next, output) = process(&mut stage, &input);

        assert_eq!(next.signals.len(), 2);
        assert_eq!(values_of(&next, 0), [-1.0, 1.0, 1.0, -1.0], "untouched");
        assert_eq!(next.signals[1].name(), "logic");
        assert_eq!(next.signals[1].domain(), Domain::DigitalLogic);
        assert_eq!(values_of(&next, 1), [0.0, 1.0, 1.0, 0.0]);
        assert_eq!(output.metrics.get("transitions"), Some(&2.0));
        assert_eq!(output.metrics.get("duty"), Some(&0.5));
        assert_eq!(
            next.signals[1].attributes().get_str("sliced_from"),
            Some("rf")
        );
    }

    #[test]
    fn hysteresis_is_what_stops_a_noisy_crossing_from_chattering() {
        let values = vec![-1.0, 0.2, -0.2, 1.0, -1.0];
        let mut plain = configured::<Slice>(ParamSet::new());
        let (chattering, _) = process(&mut plain, &frame(&[("rf", values.clone())]));
        assert_eq!(values_of(&chattering, 1), [0.0, 1.0, 0.0, 1.0, 0.0]);

        let mut sticky = configured::<Slice>(ParamSet::new().with("hysteresis", 1.0));
        let (steady, output) = process(&mut sticky, &frame(&[("rf", values)]));
        assert_eq!(values_of(&steady, 1), [0.0, 0.0, 0.0, 1.0, 0.0]);
        assert_eq!(output.metrics.get("transitions"), Some(&2.0));
    }

    #[test]
    fn inverting_flips_the_output_without_moving_the_decision_point() {
        let mut stage = configured::<Slice>(ParamSet::new().with("invert", true));
        let (next, _) = process(&mut stage, &frame(&[("rf", vec![-1.0, 1.0])]));
        assert_eq!(values_of(&next, 1), [1.0, 0.0]);
    }

    #[test]
    fn a_name_that_matches_nothing_says_what_the_group_did_have() {
        let mut stage = configured::<Slice>(ParamSet::new().with("signal", "iq"));
        let (next, output) = process(&mut stage, &frame(&[("rf", vec![1.0])]));
        assert_eq!(next.signals.len(), 1, "nothing was added");
        assert_eq!(output.diagnostics.len(), 1);
        assert!(output.diagnostics[0].message.contains("rf"));
    }

    #[test]
    fn an_unnamed_output_is_refused_before_the_run() {
        let mut stage = Slice::default();
        let error = stage
            .configure(&ParamSet::new().with("name", "  "))
            .unwrap_err();
        assert!(error.to_string().contains("needs a name"));
    }

    // ---------------------------------------------------------- symbols ----

    #[test]
    fn a_clean_nrz_waveform_decodes_to_the_bits_it_was_made_from() {
        let mut stage = configured::<SymbolDecode>(ParamSet::new().with("symbol_rate_hz", 100.0));
        let (_, output) = process(&mut stage, &frame(&[("rf", nrz(&[1, 0, 1, 1, 0]))]));
        let symbols: Symbols = artifact_of(&output);

        assert_eq!(symbols.symbol, [1, 0, 1, 1, 0]);
        assert_eq!(symbols.signal, "rf");
        assert_eq!(symbols.levels, 2);
        // Sampled at the centre of each symbol: 5 ms in, then every 10 ms.
        assert!(
            (symbols.time_s[0] - 0.005).abs() < 1e-9,
            "{:?}",
            symbols.time_s
        );
        assert!((symbols.time_s[4] - 0.045).abs() < 1e-9);
        assert_eq!(symbols.worst_margin(), Some(1.0));
        assert_eq!(output.metrics.get("symbols"), Some(&5.0));
    }

    #[test]
    fn the_clock_is_recovered_from_the_first_edge_rather_than_from_the_file() {
        // A capture that starts three samples into an idle: the symbol
        // boundaries are at 3, 13 and 23 ms, which is not where the file
        // begins. Edge alignment puts every decision at the centre of a
        // symbol; starting blind puts every one 2 ms off it.
        let mut values = vec![1.0; 3];
        values.extend(nrz(&[0, 1, 1]));

        let mut recovered =
            configured::<SymbolDecode>(ParamSet::new().with("symbol_rate_hz", 100.0));
        let (_, output) = process(&mut recovered, &frame(&[("rf", values.clone())]));
        let symbols: Symbols = artifact_of(&output);
        assert_eq!(symbols.symbol, [0, 1, 1]);
        for (index, expected) in [0.008, 0.018, 0.028].into_iter().enumerate() {
            assert!(
                (symbols.time_s[index] - expected).abs() < 1e-9,
                "{:?}",
                symbols.time_s
            );
        }

        let mut naive = configured::<SymbolDecode>(
            ParamSet::new()
                .with("symbol_rate_hz", 100.0)
                .with("align", "start"),
        );
        let (_, from_start) = process(&mut naive, &frame(&[("rf", values)]));
        let drifted: Symbols = artifact_of(&from_start);
        assert!((drifted.time_s[0] - 0.005).abs() < 1e-9, "sampled blind");
    }

    #[test]
    fn the_sampling_phase_moves_where_in_the_eye_the_decision_is_taken() {
        let mut early = configured::<SymbolDecode>(
            ParamSet::new()
                .with("symbol_rate_hz", 100.0)
                .with("align", "start")
                .with("phase", 0.1),
        );
        let (_, output) = process(&mut early, &frame(&[("rf", nrz(&[1, 0]))]));
        let symbols: Symbols = artifact_of(&output);
        assert!(
            (symbols.time_s[0] - 0.001).abs() < 1e-9,
            "{:?}",
            symbols.time_s
        );
    }

    #[test]
    fn pam4_decides_between_four_levels_and_reports_the_room_it_had() {
        // Nominals at -3, -1, +1, +3; boundaries at -2, 0, +2.
        let values: Vec<f64> = [-3.0, -1.0, 1.0, 3.0]
            .iter()
            .flat_map(|&v| [v; 10])
            .collect();
        let mut stage = configured::<SymbolDecode>(
            ParamSet::new()
                .with("symbol_rate_hz", 100.0)
                .with("format", "pam4")
                .with("align", "start"),
        );
        let (_, output) = process(&mut stage, &frame(&[("rf", values)]));
        let symbols: Symbols = artifact_of(&output);
        assert_eq!(symbols.symbol, [0, 1, 2, 3]);
        assert_eq!(symbols.levels, 4);
        assert_eq!(symbols.margin, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn the_margin_is_the_distance_to_the_nearest_boundary() {
        let mut values = vec![1.0; 10];
        // A second symbol that only just made it over the line.
        values.extend([0.05; 10]);
        let mut stage = configured::<SymbolDecode>(
            ParamSet::new()
                .with("symbol_rate_hz", 100.0)
                .with("align", "start"),
        );
        let (_, output) = process(&mut stage, &frame(&[("rf", values)]));
        let symbols: Symbols = artifact_of(&output);
        assert_eq!(symbols.symbol, [1, 1]);
        assert!((symbols.worst_margin().unwrap() - 0.05).abs() < 1e-9);
        assert!((output.metrics["worst_margin"] - 0.05).abs() < 1e-9);
    }

    #[test]
    fn a_symbol_rate_faster_than_the_sample_rate_fails_the_group_and_says_why() {
        let mut stage = configured::<SymbolDecode>(ParamSet::new().with("symbol_rate_hz", 5000.0));
        let error = try_process(&mut stage, &frame(&[("rf", vec![1.0; 20])])).unwrap_err();
        assert!(error.to_string().contains("faster than"), "{error}");
    }

    #[test]
    fn a_symbol_rate_of_zero_is_refused_before_the_run() {
        let mut stage = SymbolDecode::default();
        assert!(stage
            .configure(&ParamSet::new().with("symbol_rate_hz", 0.0))
            .is_err());
        assert!(
            stage.configure(&ParamSet::new()).is_err(),
            "and it is required"
        );
    }

    #[test]
    fn a_signal_shorter_than_one_symbol_warns_rather_than_inventing_one() {
        let mut stage = configured::<SymbolDecode>(ParamSet::new().with("symbol_rate_hz", 1.0));
        let (next, output) = process(&mut stage, &frame(&[("rf", vec![1.0, 1.0])]));
        let symbols: Symbols = artifact_of(&output);
        assert!(symbols.is_empty());
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(next.group.attributes.get_i64("symbol_count"), Some(0));
    }

    // ------------------------------------------------------------- bits ----

    fn symbols_of(bits: &[i64]) -> Symbols {
        let mut symbols = Symbols::new("rf", 2);
        for (index, &bit) in bits.iter().enumerate() {
            symbols.push(index as f64 * 0.01, bit as f64 * 2.0 - 1.0, bit, 1.0);
        }
        symbols
    }

    #[test]
    fn symbols_pack_into_words_first_bit_most_significant() {
        let mut stage = configured::<BitPack>(ParamSet::new());
        let input = with_inbound(
            frame(&[("rf", vec![1.0])]),
            "symbols",
            &symbols_of(&[1, 0, 1, 1, 0, 1, 0, 1, 1, 1]),
        );
        let (next, output) = process(&mut stage, &input);
        let packed: Bits = artifact_of(&output);

        assert_eq!(packed.digits(), "1011010111");
        assert_eq!(packed.bits, ["10110101", "11"]);
        assert_eq!(packed.value, [0b1011_0101, 0b11]);
        assert_eq!(packed.hex, ["B5", "03"]);
        assert_eq!(packed.count, 10);
        assert_eq!(output.metrics.get("bits"), Some(&10.0));
        assert_eq!(output.metrics.get("words"), Some(&2.0));
        assert_eq!(next.group.attributes.get_i64("bit_count"), Some(10));
    }

    #[test]
    fn the_bit_order_within_a_word_is_the_users_to_choose() {
        let mut stage =
            configured::<BitPack>(ParamSet::new().with("width", 4).with("msb_first", false));
        let input = with_inbound(
            frame(&[("rf", vec![1.0])]),
            "symbols",
            &symbols_of(&[1, 0, 0, 0]),
        );
        let (_, output) = process(&mut stage, &input);
        let packed: Bits = artifact_of(&output);
        assert_eq!(packed.value, [1], "the first bit is the least significant");
        assert_eq!(packed.bits, ["0001"]);
    }

    #[test]
    fn a_pam4_symbol_contributes_two_bits() {
        let mut symbols = Symbols::new("rf", 4);
        for (index, symbol) in [2i64, 1].into_iter().enumerate() {
            symbols.push(index as f64 * 0.01, 0.0, symbol, 1.0);
        }
        let mut stage = configured::<BitPack>(ParamSet::new().with("width", 4));
        let input = with_inbound(frame(&[("rf", vec![1.0])]), "symbols", &symbols);
        let (_, output) = process(&mut stage, &input);
        let packed: Bits = artifact_of(&output);
        assert_eq!(packed.bits, ["1001"], "10 then 01");
    }

    #[test]
    fn inverting_flips_every_bit_and_nothing_else() {
        let mut stage =
            configured::<BitPack>(ParamSet::new().with("width", 4).with("invert", true));
        let input = with_inbound(
            frame(&[("rf", vec![1.0])]),
            "symbols",
            &symbols_of(&[1, 0, 1, 0]),
        );
        let (_, output) = process(&mut stage, &input);
        let packed: Bits = artifact_of(&output);
        assert_eq!(packed.bits, ["0101"]);
    }

    #[test]
    fn no_symbols_upstream_still_publishes_a_bitstream_with_nothing_in_it() {
        let mut stage = configured::<BitPack>(ParamSet::new());
        let input = with_inbound(
            frame(&[("rf", vec![1.0])]),
            "symbols",
            &Symbols::new("rf", 2),
        );
        let (_, output) = process(&mut stage, &input);
        let packed: Bits = artifact_of(&output);
        assert!(packed.is_empty());
        assert_eq!(output.diagnostics.len(), 1);
    }

    #[test]
    fn a_packer_with_no_symbols_on_its_port_fails_the_group_by_name() {
        let mut stage = configured::<BitPack>(ParamSet::new());
        let error = try_process(&mut stage, &frame(&[("rf", vec![1.0])])).unwrap_err();
        assert!(matches!(
            error,
            StageError::MissingInput { ref port } if port == "symbols"
        ));
    }

    #[test]
    fn the_three_stages_are_a_chain_from_a_waveform_to_a_bitstream() {
        // What the family is for: slice, decode, pack — each step visible on
        // the rail, and the bits at the end are the bits that went in.
        let sent = [1u8, 0, 1, 1, 0, 0, 1, 0];
        let input = frame(&[("rf", nrz(&sent))]);

        let mut slice = configured::<Slice>(ParamSet::new());
        let (sliced, _) = process(&mut slice, &input);

        let mut decode = configured::<SymbolDecode>(
            ParamSet::new()
                .with("symbol_rate_hz", 100.0)
                .with("signal", "logic")
                .with("level", 0.5)
                .with("spacing", 1.0),
        );
        let (decoded, symbols_out) = process(&mut decode, &sliced);
        let symbols: Symbols = artifact_of(&symbols_out);
        let sent_as_symbols: Vec<i64> = sent.iter().map(|&bit| i64::from(bit)).collect();
        assert_eq!(symbols.symbol, sent_as_symbols);

        let mut pack = configured::<BitPack>(ParamSet::new());
        let (_, bits_out) = process(&mut pack, &with_inbound(decoded, "symbols", &symbols));
        let packed: Bits = artifact_of(&bits_out);
        assert_eq!(packed.digits(), "10110010");
    }
}
