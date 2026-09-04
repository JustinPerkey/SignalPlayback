//! Up-front spec validation (`docs/DESIGN.md` §8.2).
//!
//! > Node parameter validation happens up front: negative frequencies, duty
//! > outside (0,1), Nyquist violations become blocking errors with a fix
//! > suggestion.
//!
//! Every issue carries the JSON pointer of the node it belongs to, which is
//! the same address the tree editor selects with and a sweep targets
//! ([`crate::tree`]), so the generator screen can put the message next to the
//! field that caused it.

use std::fmt;

use crate::expr;
use crate::spec::{ConcatPart, EnvelopeSpec, GenSpec, ModKind, Node, Sweep};
use crate::train::{FieldValue, Pri, TrainSpec};
use crate::tree::{self, ROOT};

/// Pulses above which a train is worth a second look before it is written.
/// Nothing stops one — goal G2 asks for 10 M pulse records — but a slipped
/// decimal in the PRI is far more likely than the intent.
const LARGE_TRAIN: u64 = 10_000_000;

/// Whether an issue blocks generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Generation would produce nonsense; it does not start.
    Error,
    /// Generation proceeds, but the result is probably not what was meant.
    Warning,
}

impl Severity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warning => "Warning",
        }
    }
}

/// One problem found in a spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    /// JSON pointer of the offending node, or [`ROOT`]'s parent (`""`) for a
    /// problem with the spec's own settings.
    pub pointer: String,
    /// The parameter, when the issue is about one field.
    pub field: Option<String>,
    pub message: String,
    /// What to change to make it go away.
    pub fix: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(f, "{field}: {} — {}", self.message, self.fix),
            None => write!(f, "{} — {}", self.message, self.fix),
        }
    }
}

/// Everything validation found, in tree order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issues(Vec<Issue>);

impl Issues {
    #[must_use]
    pub fn as_slice(&self) -> &[Issue] {
        &self.0
    }

    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.0
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Issue> {
        self.0
            .iter()
            .filter(|issue| issue.severity == Severity::Warning)
    }

    /// Issues attached to one node.
    pub fn at<'a>(&'a self, pointer: &'a str) -> impl Iterator<Item = &'a Issue> {
        self.0.iter().filter(move |issue| issue.pointer == pointer)
    }

    /// Whether anything blocks generation.
    #[must_use]
    pub fn blocks(&self) -> bool {
        self.errors().next().is_some()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    fn push(
        &mut self,
        severity: Severity,
        pointer: &str,
        field: Option<&str>,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) {
        self.0.push(Issue {
            severity,
            pointer: pointer.to_owned(),
            field: field.map(str::to_owned),
            message: message.into(),
            fix: fix.into(),
        });
    }
}

impl fmt::Display for Issues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for issue in &self.0 {
            if !first {
                f.write_str("; ")?;
            }
            first = false;
            write!(f, "{issue}")?;
        }
        Ok(())
    }
}

/// Checks a spec and everything in it.
#[must_use]
pub fn validate(spec: &GenSpec) -> Issues {
    let mut issues = Issues::default();
    let rate = spec.sample_rate_hz();

    if !spec.timebase.is_regular() || !rate.is_finite() || rate <= 0.0 {
        issues.push(
            Severity::Error,
            "",
            Some("sample_rate_hz"),
            "a generated signal needs a regular sample rate",
            "set a sample rate above zero",
        );
    }
    if !spec.duration_s.is_finite() || spec.duration_s <= 0.0 {
        issues.push(
            Severity::Error,
            "",
            Some("duration_s"),
            "the duration is not a positive number of seconds",
            "set a duration above zero",
        );
    }
    if !spec.timebase.t0_s.is_finite() {
        issues.push(
            Severity::Error,
            "",
            Some("t0_s"),
            "the start time is not a number",
            "set a finite start time",
        );
    }
    if issues.blocks() {
        // Every node check below reads the rate or the duration; reporting
        // them against a broken timebase would only add noise.
        return issues;
    }

    node(&mut issues, &spec.root, ROOT, rate, spec.duration_s);
    issues
}

/// Checks a pulse-train spec (§8.5).
///
/// Issues are addressed the way a waveform spec's are, so the generator screen
/// renders both through one list: `""` for the train's own settings, `/pri`
/// for the interval, `/fields/<n>` for a column.
#[must_use]
pub fn validate_train(spec: &TrainSpec) -> Issues {
    let mut issues = Issues::default();

    if spec.groups == 0 {
        issues.push(
            Severity::Error,
            "",
            Some("groups"),
            "a train with no groups holds nothing",
            "use one group or more",
        );
    }
    if spec.pulses_per_group == 0 {
        issues.push(
            Severity::Error,
            "",
            Some("pulses_per_group"),
            "a group with no pulses holds nothing",
            "use one pulse or more",
        );
    }
    if !spec.t0_s.is_finite() {
        issues.push(
            Severity::Error,
            "",
            Some("t0_s"),
            "the start time is not a number",
            "enter a time in seconds",
        );
    }
    if spec.pulses() > LARGE_TRAIN {
        issues.push(
            Severity::Warning,
            "",
            Some("pulses_per_group"),
            format!(
                "{} pulses is a large train; check the PRI and the counts",
                spec.pulses()
            ),
            "reduce the groups or the pulses per group if that was not meant",
        );
    }

    pri(&mut issues, &spec.pri, spec.pulses());

    if spec.fields.is_empty() {
        issues.push(
            Severity::Error,
            "",
            Some("fields"),
            "a pulse record is a time of arrival plus at least one field",
            "add a field such as pulse width, power or angle",
        );
    }
    let mut seen: Vec<String> = Vec::new();
    for (index, field) in spec.fields.iter().enumerate() {
        let pointer = format!("/fields/{index}");
        let key = sp_core::pulse::normalise_key(&field.name);
        if field.name.trim().is_empty() || key.is_empty() {
            issues.push(
                Severity::Error,
                &pointer,
                Some("name"),
                "a field needs a name, which is also its property key",
                "give it a name such as 'pulse width'",
            );
        } else if seen.contains(&key) {
            issues.push(
                Severity::Error,
                &pointer,
                Some("name"),
                format!(
                    "'{}' collides with an earlier field's key '{key}'",
                    field.name
                ),
                "rename one of them; keys are the normalised names",
            );
        } else {
            seen.push(key);
        }
        field_value(&mut issues, &field.value, &pointer);
    }

    issues
}

fn pri(issues: &mut Issues, spec: &Pri, pulses: u64) {
    let positive = |issues: &mut Issues, field: &str, value: f64| {
        if !value.is_finite() || value <= 0.0 {
            issues.push(
                Severity::Error,
                "/pri",
                Some(field),
                format!("{field} is not a positive number of seconds"),
                "set an interval above zero",
            );
        }
    };
    match spec {
        Pri::Fixed { pri_s } => positive(issues, "pri_s", *pri_s),
        Pri::Stagger { positions } => {
            if positions.is_empty() {
                issues.push(
                    Severity::Error,
                    "/pri",
                    Some("positions"),
                    "a stagger needs at least one position",
                    "add an interval, or switch to a fixed PRI",
                );
            }
            for (index, position) in positions.iter().enumerate() {
                if !position.is_finite() || *position <= 0.0 {
                    issues.push(
                        Severity::Error,
                        "/pri",
                        Some(&format!("positions/{index}")),
                        format!("position {index} is not a positive number of seconds"),
                        "every stagger position is an interval above zero",
                    );
                }
            }
        }
        Pri::Jitter { pri_s, fraction } => {
            positive(issues, "pri_s", *pri_s);
            if !fraction.is_finite() || !(0.0..1.0).contains(fraction) {
                issues.push(
                    Severity::Error,
                    "/pri",
                    Some("fraction"),
                    format!("a jitter of {fraction} is outside 0..1"),
                    "use a fraction of the PRI; 0.05 is 5% peak-to-peak",
                );
            }
        }
        Pri::Drift { pri_s, per_pulse_s } => {
            positive(issues, "pri_s", *pri_s);
            if !per_pulse_s.is_finite() {
                issues.push(
                    Severity::Error,
                    "/pri",
                    Some("per_pulse_s"),
                    "the drift is not a number",
                    "enter a change per pulse in seconds",
                );
            } else if pulses > 1 {
                let last = pri_s + per_pulse_s * (pulses - 1) as f64;
                if last <= 0.0 {
                    issues.push(
                        Severity::Error,
                        "/pri",
                        Some("per_pulse_s"),
                        format!("the interval drifts to {last} s before the train ends"),
                        "reduce the drift, shorten the train, or raise the PRI",
                    );
                }
            }
        }
    }
}

fn field_value(issues: &mut Issues, value: &FieldValue, pointer: &str) {
    let finite = |issues: &mut Issues, field: &str, value: f64| {
        if !value.is_finite() {
            issues.push(
                Severity::Error,
                pointer,
                Some(field),
                format!("{field} is not a number"),
                "enter a finite value",
            );
        }
    };
    match value {
        FieldValue::Constant { value } => finite(issues, "value", *value),
        FieldValue::Uniform { lo, hi } => {
            finite(issues, "lo", *lo);
            finite(issues, "hi", *hi);
            if lo.is_finite() && hi.is_finite() && lo >= hi {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("lo"),
                    format!("the low bound {lo} is not below the high bound {hi}"),
                    "swap them, or use a constant",
                );
            }
        }
        FieldValue::Gaussian { mean, sigma } => {
            finite(issues, "mean", *mean);
            if !sigma.is_finite() || *sigma < 0.0 {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("sigma"),
                    "a spread cannot be negative",
                    "use zero for a constant field",
                );
            }
        }
        FieldValue::Ramp { start, end } => {
            finite(issues, "start", *start);
            finite(issues, "end", *end);
        }
        FieldValue::Sequence { values } => {
            if values.is_empty() {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("values"),
                    "the sequence has no values",
                    "add a value, or use a constant",
                );
            }
            if let Some(bad) = values.iter().find(|v| !v.is_finite()) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("values"),
                    format!("'{bad}' is not a finite value"),
                    "every value in the sequence is a number",
                );
            }
        }
        FieldValue::Scan {
            mean,
            amp,
            period_pulses,
        } => {
            finite(issues, "mean", *mean);
            finite(issues, "amp", *amp);
            if !period_pulses.is_finite() || *period_pulses <= 0.0 {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("period_pulses"),
                    "a scan needs a period of at least one pulse",
                    "set how many pulses one sweep of the scan takes",
                );
            }
        }
    }
}

/// One issue standing for a sweep that will not expand, so the generator
/// screen can show it in the same list as the spec's own problems (§8.3).
#[must_use]
pub fn sweep_issue(json_pointer: &str, reason: &str) -> Issues {
    let mut issues = Issues::default();
    issues.push(
        Severity::Error,
        json_pointer,
        Some("sweep"),
        format!("the sweep does not expand: {reason}"),
        "adjust the start, stop and step so the sweep terminates",
    );
    issues
}

/// Validates one node and its children. `rate_hz` and `duration_s` are the
/// grid the node renders on, which `Resample` and `Concat` change for their
/// children exactly as the renderer does.
fn node(issues: &mut Issues, node: &Node, pointer: &str, rate_hz: f64, duration_s: f64) {
    let nyquist = rate_hz / 2.0;

    let finite = |issues: &mut Issues, field: &str, value: f64| {
        if !value.is_finite() {
            issues.push(
                Severity::Error,
                pointer,
                Some(field),
                format!("{field} is not a number"),
                "enter a finite value",
            );
        }
    };

    let frequency = |issues: &mut Issues, field: &str, value: f64| {
        if !value.is_finite() {
            issues.push(
                Severity::Error,
                pointer,
                Some(field),
                format!("{field} is not a number"),
                "enter a finite frequency",
            );
        } else if value < 0.0 {
            issues.push(
                Severity::Error,
                pointer,
                Some(field),
                format!("{field} is negative"),
                "use a frequency of zero or more; a phase of pi flips a waveform instead",
            );
        } else if value >= nyquist {
            issues.push(
                Severity::Error,
                pointer,
                Some(field),
                format!("{field} is {value} Hz, at or above the Nyquist frequency of {nyquist} Hz"),
                format!(
                    "lower it below {nyquist} Hz, or raise the sample rate above {} Hz",
                    2.0 * value
                ),
            );
        }
    };

    match node {
        Node::Sine {
            freq_hz,
            amp,
            phase_rad,
            offset,
        } => {
            frequency(issues, "freq_hz", *freq_hz);
            finite(issues, "amp", *amp);
            finite(issues, "phase_rad", *phase_rad);
            finite(issues, "offset", *offset);
        }
        Node::Square {
            freq_hz,
            amp,
            duty,
            phase_rad,
        } => {
            frequency(issues, "freq_hz", *freq_hz);
            finite(issues, "amp", *amp);
            finite(issues, "phase_rad", *phase_rad);
            if !(duty.is_finite() && *duty > 0.0 && *duty < 1.0) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("duty"),
                    format!("a duty cycle of {duty} is outside (0, 1)"),
                    "use a fraction between 0 and 1, exclusive; 0.5 is a square wave",
                );
            }
            // A square wave's harmonics run to infinity, so it always aliases;
            // say so once, at the point where it is audible in the result.
            if *freq_hz > nyquist / 10.0 && *freq_hz < nyquist {
                issues.push(
                    Severity::Warning,
                    pointer,
                    Some("freq_hz"),
                    "a square wave above a tenth of Nyquist aliases visibly",
                    "raise the sample rate, or expect the edges to ring",
                );
            }
        }
        Node::Triangle {
            freq_hz,
            amp,
            phase_rad,
        } => {
            frequency(issues, "freq_hz", *freq_hz);
            finite(issues, "amp", *amp);
            finite(issues, "phase_rad", *phase_rad);
        }
        Node::Sawtooth { freq_hz, amp, .. } => {
            frequency(issues, "freq_hz", *freq_hz);
            finite(issues, "amp", *amp);
        }
        Node::Pulse {
            period_s,
            width_s,
            amp,
            rise_s,
            fall_s,
        } => {
            finite(issues, "amp", *amp);
            if !(period_s.is_finite() && *period_s > 0.0) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("period_s"),
                    "the period is not a positive number of seconds",
                    "set a period above zero",
                );
            }
            for (field, value) in [
                ("width_s", *width_s),
                ("rise_s", *rise_s),
                ("fall_s", *fall_s),
            ] {
                if !value.is_finite() || value < 0.0 {
                    issues.push(
                        Severity::Error,
                        pointer,
                        Some(field),
                        format!("{field} is negative or not a number"),
                        "use zero or more seconds",
                    );
                }
            }
            let span = rise_s + width_s + fall_s;
            if period_s.is_finite() && *period_s > 0.0 && span > *period_s {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("width_s"),
                    format!("rise + width + fall is {span} s, longer than the {period_s} s period"),
                    format!("shorten the pulse to at most {period_s} s, or lengthen the period"),
                );
            }
            if period_s.is_finite() && *period_s > 0.0 && *period_s < 2.0 / rate_hz {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("period_s"),
                    "the period is shorter than two samples",
                    format!("use a period of at least {} s", 2.0 / rate_hz),
                );
            }
        }
        Node::Chirp {
            f0_hz,
            f1_hz,
            sweep,
            amp,
        } => {
            frequency(issues, "f0_hz", *f0_hz);
            frequency(issues, "f1_hz", *f1_hz);
            finite(issues, "amp", *amp);
            if *sweep == Sweep::Log && (*f0_hz <= 0.0 || *f1_hz <= 0.0) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("f0_hz"),
                    "a logarithmic sweep cannot start or end at zero",
                    "use frequencies above zero, or switch the sweep to linear",
                );
            }
        }
        Node::Dc { level } => finite(issues, "level", *level),
        Node::Ramp { start, end } => {
            finite(issues, "start", *start);
            finite(issues, "end", *end);
        }
        Node::Step {
            at_s,
            before,
            after,
        } => {
            finite(issues, "at_s", *at_s);
            finite(issues, "before", *before);
            finite(issues, "after", *after);
            outside_the_signal(issues, pointer, "at_s", *at_s, duration_s);
        }
        Node::Impulse { at_s, amp } => {
            finite(issues, "at_s", *at_s);
            finite(issues, "amp", *amp);
            outside_the_signal(issues, pointer, "at_s", *at_s, duration_s);
        }
        Node::Noise { amp, .. } => finite(issues, "amp", *amp),
        Node::Prbs { order, taps, amp } => {
            finite(issues, "amp", *amp);
            if crate::noise::default_taps(*order).is_none() {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("order"),
                    format!("a PRBS order of {order} is outside 2..=32"),
                    "use an order between 2 and 32; 9 and 23 are the common ones",
                );
            }
            if let Some(taps) = taps {
                if *taps == 0 {
                    issues.push(
                        Severity::Error,
                        pointer,
                        Some("taps"),
                        "a tap mask of zero never feeds back",
                        "clear the mask to use the built-in maximal-length taps",
                    );
                }
            }
        }
        Node::Expr { source } => {
            if let Err(error) = expr::Program::compile(source) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("source"),
                    format!("the expression does not parse: {error}"),
                    "the variable is 't' in seconds; 'pi' and 'e' are constants",
                );
            }
        }

        Node::Sum { terms } | Node::Product { terms } => {
            if terms.is_empty() {
                issues.push(
                    Severity::Error,
                    pointer,
                    None,
                    "the combinator has no terms",
                    "add at least one term, or replace the node with a primitive",
                );
            }
            for (index, term) in terms.iter().enumerate() {
                self::node(
                    issues,
                    term,
                    &tree::child_pointer(pointer, tree::TERMS, Some(index)),
                    rate_hz,
                    duration_s,
                );
            }
        }
        Node::Concat { parts } => {
            if parts.is_empty() {
                issues.push(
                    Severity::Error,
                    pointer,
                    None,
                    "the concatenation has no parts",
                    "add at least one part, or replace the node with a primitive",
                );
            }
            let total: f64 = parts.iter().map(|part| part.duration_s).sum();
            if total.is_finite() && total < duration_s - 1.0 / rate_hz {
                issues.push(
                    Severity::Warning,
                    pointer,
                    None,
                    format!(
                        "the parts total {total} s but the signal is {duration_s} s; the rest is silence"
                    ),
                    "lengthen a part, add another, or shorten the signal",
                );
            }
            for (index, ConcatPart { node, duration_s }) in parts.iter().enumerate() {
                if !duration_s.is_finite() || *duration_s <= 0.0 {
                    // A part's duration belongs to the Concat's own form, not
                    // to the part, so the message lands on the Concat.
                    issues.push(
                        Severity::Error,
                        pointer,
                        Some(&format!("{}/{index}/duration_s", tree::PARTS)),
                        format!("part {index} has no positive duration"),
                        "set a duration above zero",
                    );
                }
                self::node(
                    issues,
                    node,
                    &tree::child_pointer(pointer, tree::PARTS, Some(index)),
                    rate_hz,
                    duration_s.max(0.0),
                );
            }
        }
        Node::Gain { input, factor } => {
            finite(issues, "factor", *factor);
            child(issues, input, pointer, tree::INPUT, rate_hz, duration_s);
        }
        Node::Delay { input, by_s } => {
            finite(issues, "by_s", *by_s);
            if by_s.is_finite() && *by_s < 0.0 {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("by_s"),
                    "a negative delay would need samples from the future",
                    "use zero or more seconds",
                );
            }
            outside_the_signal(issues, pointer, "by_s", *by_s, duration_s);
            child(issues, input, pointer, tree::INPUT, rate_hz, duration_s);
        }
        Node::Clip { input, lo, hi } => {
            finite(issues, "lo", *lo);
            finite(issues, "hi", *hi);
            if lo.is_finite() && hi.is_finite() && lo > hi {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("lo"),
                    format!("the low limit {lo} is above the high limit {hi}"),
                    "swap them",
                );
            }
            child(issues, input, pointer, tree::INPUT, rate_hz, duration_s);
        }
        Node::Envelope { input, env } => {
            envelope(issues, env, pointer, duration_s);
            child(issues, input, pointer, tree::INPUT, rate_hz, duration_s);
        }
        Node::Modulate {
            carrier,
            modulator,
            kind,
        } => {
            match kind {
                ModKind::Am { depth } => finite(issues, "depth", *depth),
                ModKind::Fm { dev_hz } => {
                    finite(issues, "dev_hz", *dev_hz);
                    if !modulator.is_integrable() {
                        issues.push(
                            Severity::Error,
                            pointer,
                            Some("modulator"),
                            "FM integrates its modulator, and this one has no closed-form integral",
                            "use a Sine, Square, Triangle, Sawtooth, DC, Ramp or Step modulator \
                             (or a Sum or Gain of those), or switch to PM",
                        );
                    }
                }
                ModKind::Pm { dev_rad } => finite(issues, "dev_rad", *dev_rad),
            }
            if kind.needs_oscillator_carrier() && carrier.as_oscillator().is_none() {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("carrier"),
                    format!(
                        "{} rewrites the carrier's phase, which a {} carrier does not have",
                        kind.label(),
                        carrier.kind()
                    ),
                    "use a Sine, Square, Triangle or Sawtooth carrier, or switch to AM",
                );
            }
            child(issues, carrier, pointer, tree::CARRIER, rate_hz, duration_s);
            child(
                issues,
                modulator,
                pointer,
                tree::MODULATOR,
                rate_hz,
                duration_s,
            );
        }
        Node::Resample { input, to_rate_hz } => {
            if !to_rate_hz.is_finite() || *to_rate_hz <= 0.0 {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("to_rate_hz"),
                    "the resample rate is not a positive frequency",
                    "set a rate above zero",
                );
            } else if *to_rate_hz >= rate_hz {
                issues.push(
                    Severity::Warning,
                    pointer,
                    Some("to_rate_hz"),
                    format!(
                        "resampling to {to_rate_hz} Hz from a {rate_hz} Hz grid adds no detail"
                    ),
                    "a resample below the output rate is what models a slower converter",
                );
            }
            // The child renders on the resampled grid, which is the whole
            // point of the node: its own Nyquist is what constrains it.
            let child_rate = if to_rate_hz.is_finite() && *to_rate_hz > 0.0 {
                *to_rate_hz
            } else {
                rate_hz
            };
            child(issues, input, pointer, tree::INPUT, child_rate, duration_s);
        }
        Node::FromSignal { .. } => {}
    }
}

fn child(
    issues: &mut Issues,
    node_ref: &Node,
    pointer: &str,
    field: &str,
    rate_hz: f64,
    duration_s: f64,
) {
    node(
        issues,
        node_ref,
        &tree::child_pointer(pointer, field, None),
        rate_hz,
        duration_s,
    );
}

fn envelope(issues: &mut Issues, env: &EnvelopeSpec, pointer: &str, duration_s: f64) {
    match *env {
        EnvelopeSpec::Adsr {
            attack_s,
            decay_s,
            sustain,
            release_s,
        } => {
            for (field, value) in [
                ("attack_s", attack_s),
                ("decay_s", decay_s),
                ("release_s", release_s),
            ] {
                if !value.is_finite() || value < 0.0 {
                    issues.push(
                        Severity::Error,
                        pointer,
                        Some(field),
                        format!("{field} is negative or not a number"),
                        "use zero or more seconds",
                    );
                }
            }
            if !sustain.is_finite() || !(0.0..=1.0).contains(&sustain) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("sustain"),
                    format!("a sustain level of {sustain} is outside 0..=1"),
                    "use a fraction of the peak between 0 and 1",
                );
            }
            let span = attack_s + decay_s + release_s;
            if span.is_finite() && span > duration_s {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("attack_s"),
                    format!(
                        "attack + decay + release is {span} s, longer than the {duration_s} s signal"
                    ),
                    "shorten a stage or lengthen the signal",
                );
            }
        }
        EnvelopeSpec::Gaussian { center_s, sigma_s } => {
            if !center_s.is_finite() {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("center_s"),
                    "the centre is not a number",
                    "enter a time in seconds",
                );
            }
            if !sigma_s.is_finite() || sigma_s <= 0.0 {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("sigma_s"),
                    "a Gaussian envelope needs a width above zero",
                    "set sigma to a positive number of seconds",
                );
            }
            outside_the_signal(issues, pointer, "center_s", center_s, duration_s);
        }
        EnvelopeSpec::Tukey { alpha } => {
            if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
                issues.push(
                    Severity::Error,
                    pointer,
                    Some("alpha"),
                    format!("a Tukey alpha of {alpha} is outside 0..=1"),
                    "use 0 for a rectangle, 1 for a Hann window",
                );
            }
        }
    }
}

/// Flags a time that falls outside the signal, which is nearly always a typo
/// in a unit rather than an intent.
fn outside_the_signal(
    issues: &mut Issues,
    pointer: &str,
    field: &str,
    value: f64,
    duration_s: f64,
) {
    if value.is_finite() && (value < 0.0 || value > duration_s) {
        issues.push(
            Severity::Warning,
            pointer,
            Some(field),
            format!("{field} is {value} s, outside the {duration_s} s signal"),
            format!("use a time between 0 and {duration_s} s"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{NodeKind, NoiseKind};

    fn spec(root: Node) -> GenSpec {
        GenSpec::new(48_000.0, 0.1, root)
    }

    fn messages(spec: &GenSpec) -> Vec<String> {
        validate(spec)
            .as_slice()
            .iter()
            .map(|issue| issue.message.clone())
            .collect()
    }

    #[test]
    fn every_default_node_validates_on_a_sensible_spec() {
        for kind in NodeKind::ALL {
            let spec = GenSpec::new(48_000.0, 1.0, kind.default_node());
            let issues = validate(&spec);
            assert!(!issues.blocks(), "{kind} should not block: {}", issues);
        }
    }

    #[test]
    fn a_broken_timebase_is_reported_before_the_nodes() {
        let mut spec = spec(Node::default());
        spec.duration_s = 0.0;
        let issues = validate(&spec);
        assert!(issues.blocks());
        assert!(issues
            .as_slice()
            .iter()
            .all(|issue| issue.pointer.is_empty()));
    }

    #[test]
    fn a_frequency_at_or_above_nyquist_blocks_with_a_fix() {
        let issues = validate(&spec(Node::Sine {
            freq_hz: 24_000.0,
            amp: 1.0,
            phase_rad: 0.0,
            offset: 0.0,
        }));
        let issue = issues.errors().next().expect("an error");
        assert!(issue.message.contains("Nyquist"), "{issue}");
        assert!(issue.fix.contains("48000"), "{issue}");
    }

    #[test]
    fn a_negative_frequency_blocks() {
        let issues = validate(&spec(Node::Sine {
            freq_hz: -1.0,
            amp: 1.0,
            phase_rad: 0.0,
            offset: 0.0,
        }));
        assert!(issues.blocks());
        assert!(messages(&spec(Node::Sine {
            freq_hz: -1.0,
            amp: 1.0,
            phase_rad: 0.0,
            offset: 0.0,
        }))
        .iter()
        .any(|m| m.contains("negative")));
    }

    #[test]
    fn a_duty_cycle_outside_the_open_unit_interval_blocks() {
        for duty in [0.0, 1.0, -0.5, f64::NAN] {
            let issues = validate(&spec(Node::Square {
                freq_hz: 100.0,
                amp: 1.0,
                duty,
                phase_rad: 0.0,
            }));
            assert!(issues.blocks(), "duty {duty}");
        }
    }

    #[test]
    fn a_pulse_longer_than_its_period_blocks() {
        let issues = validate(&spec(Node::Pulse {
            period_s: 1e-3,
            width_s: 2e-3,
            amp: 1.0,
            rise_s: 0.0,
            fall_s: 0.0,
        }));
        assert!(issues.blocks());
    }

    #[test]
    fn fm_needs_an_oscillator_carrier_and_an_integrable_modulator() {
        let issues = validate(&spec(Node::Modulate {
            carrier: Box::new(Node::Noise {
                kind: NoiseKind::Gaussian,
                amp: 1.0,
            }),
            modulator: Box::new(Node::Noise {
                kind: NoiseKind::Gaussian,
                amp: 1.0,
            }),
            kind: ModKind::Fm { dev_hz: 100.0 },
        }));
        assert_eq!(issues.errors().count(), 2, "{issues}");

        // AM takes anything.
        let issues = validate(&spec(Node::Modulate {
            carrier: Box::new(Node::Noise {
                kind: NoiseKind::Gaussian,
                amp: 1.0,
            }),
            modulator: Box::new(Node::Noise {
                kind: NoiseKind::Gaussian,
                amp: 1.0,
            }),
            kind: ModKind::Am { depth: 0.5 },
        }));
        assert!(!issues.blocks(), "{issues}");
    }

    #[test]
    fn a_broken_expression_reports_where_it_failed() {
        let issues = validate(&spec(Node::Expr {
            source: "sin(t".to_owned(),
        }));
        let issue = issues.errors().next().expect("an error");
        assert!(issue.message.contains("unclosed"), "{issue}");
    }

    #[test]
    fn a_child_of_a_resample_is_checked_against_the_resampled_rate() {
        // 8 kHz has a 4 kHz Nyquist, so a 6 kHz tone under the resample is an
        // error even though the output grid could carry it.
        let issues = validate(&spec(Node::Resample {
            input: Box::new(Node::Sine {
                freq_hz: 6_000.0,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            }),
            to_rate_hz: 8_000.0,
        }));
        let issue = issues.errors().next().expect("an error");
        assert_eq!(issue.pointer, "/root/input");
        assert!(issue.message.contains("Nyquist"), "{issue}");
    }

    #[test]
    fn a_concat_shorter_than_the_signal_warns_but_does_not_block() {
        let issues = validate(&spec(Node::Concat {
            parts: vec![ConcatPart {
                node: Node::default(),
                duration_s: 0.01,
            }],
        }));
        assert!(!issues.blocks(), "{issues}");
        assert_eq!(issues.warnings().count(), 1, "{issues}");
    }

    #[test]
    fn an_adsr_longer_than_the_signal_blocks() {
        let issues = validate(&spec(Node::Envelope {
            input: Box::new(Node::default()),
            env: EnvelopeSpec::Adsr {
                attack_s: 0.5,
                decay_s: 0.5,
                sustain: 0.5,
                release_s: 0.5,
            },
        }));
        assert!(issues.blocks());
    }

    #[test]
    fn issues_are_addressable_by_the_pointer_the_tree_selects_with() {
        let issues = validate(&spec(Node::Sum {
            terms: vec![
                Node::default(),
                Node::Sine {
                    freq_hz: 1e9,
                    amp: 1.0,
                    phase_rad: 0.0,
                    offset: 0.0,
                },
            ],
        }));
        assert_eq!(issues.at("/root/terms/1").count(), 1);
        assert_eq!(issues.at("/root/terms/0").count(), 0);
    }
}
