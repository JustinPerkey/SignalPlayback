//! Batch/sweep mode (`docs/DESIGN.md` §8.3).
//!
//! > Mark one numeric parameter as swept (`start`, `stop`, `step`, or an
//! > explicit list) to emit a whole group of signals in one action — e.g. 20
//! > sine waves from 100 Hz to 2 kHz. The sweep becomes a `signal_group` with
//! > the swept value stored as a property, which is exactly the shape a
//! > pipeline wants as test input.
//!
//! A parameter is addressed by the pointer of its node ([`crate::tree`]) plus
//! its field name, which together are a JSON pointer into the serialised spec.
//! Substituting through JSON rather than through a match over [`Node`] means a
//! new node variant is sweepable the day it is added, with no code here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::spec::GenSpec;
use crate::tree::{self, ROOT};

/// Keys that hold child nodes rather than parameters; those are swept through
/// their own pointers.
const CHILD_KEYS: [&str; 6] = [
    tree::TERMS,
    tree::PARTS,
    tree::INPUT,
    tree::CARRIER,
    tree::MODULATOR,
    "node",
];

/// The most rungs one sweep may produce. A slipped decimal in `step` would
/// otherwise ask for a library of millions of signals.
pub const MAX_RUNGS: usize = 4_096;

/// One numeric parameter a sweep can target.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamRef {
    /// Pointer of the owning node, or `""` for a spec-level setting.
    pub pointer: String,
    /// Field path within the node, e.g. `freq_hz` or `env/attack_s`.
    pub field: String,
    /// How the parameter reads in the target picker.
    pub label: String,
    pub value: f64,
    /// Whether the stored value is an integer, so a swept value is rounded
    /// back into range rather than failing to deserialise.
    pub integral: bool,
}

impl ParamRef {
    /// The JSON pointer of the parameter itself.
    #[must_use]
    pub fn json_pointer(&self) -> String {
        format!("{}/{}", self.pointer, self.field)
    }
}

/// The values a sweep steps through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SweepValues {
    /// `start` to `stop` inclusive, in steps of `step`.
    Range { start: f64, stop: f64, step: f64 },
    /// Exactly these values, in this order.
    List { values: Vec<f64> },
}

impl Default for SweepValues {
    fn default() -> Self {
        Self::Range {
            start: 100.0,
            stop: 2_000.0,
            step: 100.0,
        }
    }
}

impl SweepValues {
    /// The rungs, or why there are none.
    pub fn values(&self) -> Result<Vec<f64>, String> {
        match self {
            Self::List { values } => {
                if values.is_empty() {
                    return Err("the list has no values".to_owned());
                }
                if let Some(bad) = values.iter().find(|v| !v.is_finite()) {
                    return Err(format!("'{bad}' is not a finite value"));
                }
                if values.len() > MAX_RUNGS {
                    return Err(format!(
                        "{} values is more than the {MAX_RUNGS} a sweep may produce",
                        values.len()
                    ));
                }
                Ok(values.clone())
            }
            Self::Range { start, stop, step } => {
                if !start.is_finite() || !stop.is_finite() || !step.is_finite() {
                    return Err("start, stop and step must all be numbers".to_owned());
                }
                if *step == 0.0 {
                    return Err("a step of zero never reaches the stop value".to_owned());
                }
                if (stop - start).signum() != step.signum() && stop != start {
                    return Err(format!(
                        "a step of {step} moves away from {stop}; flip its sign"
                    ));
                }
                // Count first: stepping until a comparison fails accumulates
                // float error, and a rung count is what the UI wants anyway.
                let span = (stop - start).abs();
                let rungs = (span / step.abs()).floor() as usize + 1;
                if rungs > MAX_RUNGS {
                    return Err(format!(
                        "{rungs} rungs is more than the {MAX_RUNGS} a sweep may produce; \
                         use a larger step"
                    ));
                }
                Ok((0..rungs).map(|i| start + step * i as f64).collect())
            }
        }
    }

    /// How many signals the sweep would produce, when it is valid.
    #[must_use]
    pub fn count(&self) -> Option<usize> {
        self.values().ok().map(|v| v.len())
    }
}

/// A parameter sweep: one target, and the values to put in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSweep {
    /// Pointer of the node holding the swept parameter.
    pub pointer: String,
    /// Field path within that node.
    pub field: String,
    /// The property key the swept value is stored under, on every signal in
    /// the group.
    pub property_key: String,
    pub values: SweepValues,
}

impl ParamSweep {
    /// A sweep over `param`, storing the value under the field's own name.
    #[must_use]
    pub fn over(param: &ParamRef, values: SweepValues) -> Self {
        Self {
            pointer: param.pointer.clone(),
            field: param.field.clone(),
            property_key: param.field.replace('/', "_"),
            values,
        }
    }

    #[must_use]
    pub fn json_pointer(&self) -> String {
        format!("{}/{}", self.pointer, self.field)
    }
}

/// Every numeric parameter in the spec, in tree order, for the target picker.
#[must_use]
pub fn parameters(spec: &GenSpec) -> Vec<ParamRef> {
    let mut out = Vec::new();
    // The spec's own settings come first; sweeping the duration is how an
    // impairment ladder varies capture length (§8.4).
    if let Ok(Value::Object(map)) = serde_json::to_value(spec) {
        for key in ["duration_s", "seed"] {
            if let Some(value) = map.get(key) {
                collect(value, "", key, "signal", &mut out);
            }
        }
    }
    for node_ref in tree::outline(spec) {
        let Some(node) = tree::node_at(spec, &node_ref.pointer) else {
            continue;
        };
        let Ok(Value::Object(map)) = serde_json::to_value(node) else {
            continue;
        };
        let owner = format!("{} {}", node_ref.slot, node_ref.kind.label());
        for (key, value) in &map {
            if key == "node" || CHILD_KEYS.contains(&key.as_str()) {
                // `parts` still holds a per-part duration, which is a
                // parameter of the concatenation rather than of the part.
                if key == tree::PARTS {
                    collect(value, &node_ref.pointer, key, &owner, &mut out);
                }
                continue;
            }
            collect(value, &node_ref.pointer, key, &owner, &mut out);
        }
    }
    out
}

/// Adds every numeric leaf under `value` to `out`, addressed as
/// `pointer` + `/` + `field`.
fn collect(value: &Value, pointer: &str, field: &str, owner: &str, out: &mut Vec<ParamRef>) {
    match value {
        Value::Number(number) => {
            let Some(as_f64) = number.as_f64() else {
                return;
            };
            out.push(ParamRef {
                pointer: pointer.to_owned(),
                field: field.to_owned(),
                label: format!("{owner} · {field}"),
                value: as_f64,
                integral: number.is_i64() || number.is_u64(),
            });
        }
        Value::Object(map) => {
            for (key, child) in map {
                if key == "node" || CHILD_KEYS.contains(&key.as_str()) {
                    continue;
                }
                collect(child, pointer, &format!("{field}/{key}"), owner, out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect(child, pointer, &format!("{field}/{index}"), owner, out);
            }
        }
        _ => {}
    }
}

/// A copy of `spec` with one numeric parameter replaced.
///
/// An integer field takes the rounded value, so sweeping a PRBS order across
/// fractional rungs lands on whole orders instead of failing.
pub fn apply(spec: &GenSpec, json_pointer: &str, value: f64) -> Result<GenSpec, String> {
    let current = tree::get_json(spec, json_pointer)
        .ok_or_else(|| format!("'{json_pointer}' is not a parameter of this spec"))?;
    if !current.is_number() {
        return Err(format!("'{json_pointer}' is not a numeric parameter"));
    }
    let replacement = if current.is_i64() || current.is_u64() {
        Value::from(value.round() as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| format!("'{value}' is not a finite value"))?
    };
    tree::set_json(spec, json_pointer, replacement)
        .map_err(|error| format!("'{value}' is not usable there: {error}"))
}

/// One spec per rung, each paired with the value that produced it.
pub fn expand(spec: &GenSpec, sweep: &ParamSweep) -> Result<Vec<(f64, GenSpec)>, String> {
    let pointer = sweep.json_pointer();
    if !pointer.starts_with(ROOT)
        && !pointer.starts_with("/duration_s")
        && !pointer.starts_with("/seed")
    {
        // Anything else would be a pointer at the timebase or the dtype, which
        // is not a per-rung quantity.
        return Err(format!("'{pointer}' is not a sweepable parameter"));
    }
    sweep
        .values
        .values()?
        .into_iter()
        .map(|value| apply(spec, &pointer, value).map(|spec| (value, spec)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{ConcatPart, EnvelopeSpec, Node};

    fn sine_spec() -> GenSpec {
        GenSpec::new(48_000.0, 0.1, Node::default())
    }

    #[test]
    fn the_picker_lists_a_nodes_numeric_parameters() {
        let params = parameters(&sine_spec());
        let fields: Vec<_> = params.iter().map(|p| p.json_pointer()).collect();
        assert!(fields.contains(&"/duration_s".to_owned()));
        assert!(fields.contains(&"/seed".to_owned()));
        assert!(fields.contains(&"/root/freq_hz".to_owned()));
        assert!(fields.contains(&"/root/amp".to_owned()));
        // `node` is the serde tag, not a parameter.
        assert!(!fields.iter().any(|f| f.ends_with("/node")));
    }

    #[test]
    fn nested_parameters_are_reachable_and_children_are_not_duplicated() {
        let spec = GenSpec::new(
            48_000.0,
            1.0,
            Node::Envelope {
                input: Box::new(Node::default()),
                env: EnvelopeSpec::Adsr {
                    attack_s: 0.1,
                    decay_s: 0.1,
                    sustain: 0.5,
                    release_s: 0.1,
                },
            },
        );
        let fields: Vec<_> = parameters(&spec)
            .iter()
            .map(ParamRef::json_pointer)
            .collect();
        assert!(fields.contains(&"/root/env/attack_s".to_owned()));
        // The input's own frequency is listed once, under the input's pointer.
        assert_eq!(
            fields
                .iter()
                .filter(|f| *f == "/root/input/freq_hz")
                .count(),
            1
        );
    }

    #[test]
    fn a_concat_part_duration_is_listed_against_the_concat() {
        let spec = GenSpec::new(
            48_000.0,
            1.0,
            Node::Concat {
                parts: vec![ConcatPart {
                    node: Node::default(),
                    duration_s: 0.5,
                }],
            },
        );
        let fields: Vec<_> = parameters(&spec)
            .iter()
            .map(ParamRef::json_pointer)
            .collect();
        assert!(
            fields.contains(&"/root/parts/0/duration_s".to_owned()),
            "{fields:?}"
        );
    }

    #[test]
    fn every_listed_parameter_can_actually_be_swept() {
        let spec = GenSpec::new(
            48_000.0,
            1.0,
            Node::Sum {
                terms: vec![
                    Node::default(),
                    Node::Prbs {
                        order: 9,
                        taps: None,
                        amp: 1.0,
                    },
                ],
            },
        );
        for param in parameters(&spec) {
            let pointer = param.json_pointer();
            apply(&spec, &pointer, 3.0).unwrap_or_else(|error| panic!("{pointer}: {error}"));
        }
    }

    #[test]
    fn an_integer_parameter_is_rounded_rather_than_rejected() {
        let spec = GenSpec::new(
            48_000.0,
            1.0,
            Node::Prbs {
                order: 9,
                taps: None,
                amp: 1.0,
            },
        );
        let swept = apply(&spec, "/root/order", 11.4).unwrap();
        assert_eq!(
            swept.root,
            Node::Prbs {
                order: 11,
                taps: None,
                amp: 1.0
            }
        );
    }

    #[test]
    fn a_range_sweep_produces_the_rungs_the_design_describes() {
        let spec = sine_spec();
        let sweep = ParamSweep {
            pointer: ROOT.to_owned(),
            field: "freq_hz".to_owned(),
            property_key: "freq_hz".to_owned(),
            values: SweepValues::Range {
                start: 100.0,
                stop: 2_000.0,
                step: 100.0,
            },
        };
        let rungs = expand(&spec, &sweep).unwrap();
        assert_eq!(rungs.len(), 20);
        assert_eq!(rungs[0].0, 100.0);
        assert_eq!(rungs[19].0, 2_000.0);
        let Node::Sine { freq_hz, .. } = rungs[19].1.root else {
            panic!("still a sine");
        };
        assert_eq!(freq_hz, 2_000.0);
    }

    #[test]
    fn a_list_sweep_keeps_its_order() {
        let sweep = SweepValues::List {
            values: vec![3.0, 1.0, 2.0],
        };
        assert_eq!(sweep.values().unwrap(), [3.0, 1.0, 2.0]);
        assert_eq!(sweep.count(), Some(3));
    }

    #[test]
    fn a_sweep_that_cannot_terminate_says_why() {
        for (values, fragment) in [
            (
                SweepValues::Range {
                    start: 0.0,
                    stop: 1.0,
                    step: 0.0,
                },
                "never reaches",
            ),
            (
                SweepValues::Range {
                    start: 0.0,
                    stop: 1.0,
                    step: -0.1,
                },
                "flip its sign",
            ),
            (
                SweepValues::Range {
                    start: 0.0,
                    stop: 1.0,
                    step: 1e-9,
                },
                "more than the",
            ),
            (SweepValues::List { values: vec![] }, "no values"),
            (
                SweepValues::List {
                    values: vec![f64::NAN],
                },
                "not a finite value",
            ),
        ] {
            let error = values.values().expect_err("rejected");
            assert!(
                error.contains(fragment),
                "{error} does not mention {fragment}"
            );
        }
    }

    #[test]
    fn a_pointer_that_is_not_a_parameter_is_refused() {
        let spec = sine_spec();
        assert!(apply(&spec, "/root/nowhere", 1.0).is_err());
        assert!(apply(&spec, "/root/node", 1.0).is_err());
        assert!(apply(&spec, "/dtype", 1.0).is_err());
    }

    #[test]
    fn a_sweep_names_the_property_its_value_is_stored_under() {
        let param = ParamRef {
            pointer: ROOT.to_owned(),
            field: "env/attack_s".to_owned(),
            label: String::new(),
            value: 0.1,
            integral: false,
        };
        let sweep = ParamSweep::over(&param, SweepValues::default());
        assert_eq!(sweep.property_key, "env_attack_s");
        assert_eq!(sweep.json_pointer(), "/root/env/attack_s");
    }
}
