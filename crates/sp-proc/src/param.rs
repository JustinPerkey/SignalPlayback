//! Stage parameters: what a stage declares, and what the user supplied
//! (`docs/DESIGN.md` §9.2).
//!
//! A [`ParamSpec`] is `'static` data on the stage's descriptor, which is what
//! lets the pipeline editor generate a form without the stage knowing about
//! widgets. A [`ParamSet`] is the values for one stage instance: it is what
//! `configure` reads, what is saved in `pipeline_stage.params_json`, and what
//! feeds the cache key — so its JSON form is canonical, with keys in a fixed
//! order and defaults filled in.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sp_core::props::PropKind;
use sp_core::PropertyValue;

use crate::error::ConfigError;

/// The type of one parameter, including the range the editor enforces.
///
/// This is a `const`-friendly cousin of [`PropKind`]: descriptors are static,
/// so the enum variants cannot own heap data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    Float {
        min: Option<f64>,
        max: Option<f64>,
    },
    Int {
        min: Option<i64>,
        max: Option<i64>,
    },
    Bool,
    Text,
    Enum {
        variants: &'static [&'static str],
    },
    /// A frequency in hertz, rendered with unit scaling.
    FreqHz {
        min: Option<f64>,
        max: Option<f64>,
    },
    DurationS,
}

impl ParamKind {
    /// A short type name, used in error messages.
    #[must_use]
    pub const fn type_name(self) -> &'static str {
        match self {
            Self::Float { .. } => "a number",
            Self::Int { .. } => "a whole number",
            Self::Bool => "true or false",
            Self::Text => "text",
            Self::Enum { .. } => "one of a fixed set",
            Self::FreqHz { .. } => "a frequency in Hz",
            Self::DurationS => "a duration in seconds",
        }
    }

    /// The property kind the editor generates a widget from, so a stage
    /// parameter and a signal property are edited by the same code.
    #[must_use]
    pub fn to_prop_kind(self) -> PropKind {
        match self {
            Self::Float { min, max } => PropKind::Float {
                min,
                max,
                step: None,
            },
            Self::Int { min, max } => PropKind::Int { min, max },
            Self::Bool => PropKind::Bool,
            Self::Text => PropKind::Text {
                pattern: None,
                max_len: None,
            },
            Self::Enum { variants } => PropKind::Enum {
                variants: variants.iter().map(|v| (*v).to_owned()).collect(),
            },
            Self::FreqHz { min, max } => PropKind::FreqHz { min, max },
            Self::DurationS => PropKind::DurationS,
        }
    }
}

/// A parameter's default, in the only forms a `const` can hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamDefault {
    /// No default: the user must supply a value.
    Required,
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(&'static str),
}

impl ParamDefault {
    #[must_use]
    pub fn value(self) -> Option<PropertyValue> {
        match self {
            Self::Required => None,
            Self::Float(v) => Some(PropertyValue::from(v)),
            Self::Int(v) => Some(PropertyValue::from(v)),
            Self::Bool(v) => Some(PropertyValue::from(v)),
            Self::Text(v) => Some(PropertyValue::from(v)),
        }
    }
}

/// One declared parameter of a stage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
    pub default: ParamDefault,
    /// One line of help, shown under the field.
    pub help: &'static str,
    pub unit: Option<&'static str>,
}

impl ParamSpec {
    /// A parameter with no default, which the user must set.
    #[must_use]
    pub const fn required(name: &'static str, label: &'static str, kind: ParamKind) -> Self {
        Self {
            name,
            label,
            kind,
            default: ParamDefault::Required,
            help: "",
            unit: None,
        }
    }

    #[must_use]
    pub const fn new(
        name: &'static str,
        label: &'static str,
        kind: ParamKind,
        default: ParamDefault,
    ) -> Self {
        Self {
            name,
            label,
            kind,
            default,
            help: "",
            unit: None,
        }
    }

    #[must_use]
    pub const fn with_help(mut self, help: &'static str) -> Self {
        self.help = help;
        self
    }

    #[must_use]
    pub const fn with_unit(mut self, unit: &'static str) -> Self {
        self.unit = Some(unit);
        self
    }

    #[must_use]
    pub fn is_required(&self) -> bool {
        matches!(self.default, ParamDefault::Required)
    }

    /// Checks one value against this declaration.
    pub fn check(&self, value: &PropertyValue) -> Result<(), ConfigError> {
        let name = self.name.to_owned();
        match self.kind {
            ParamKind::Float { min, max } | ParamKind::FreqHz { min, max } => {
                let found = value.as_f64().ok_or_else(|| self.wrong_type(value))?;
                check_range(&name, found, min, max)
            }
            ParamKind::DurationS => {
                let found = value.as_f64().ok_or_else(|| self.wrong_type(value))?;
                check_range(&name, found, Some(0.0), None)
            }
            ParamKind::Int { min, max } => {
                let found = value.as_i64().ok_or_else(|| self.wrong_type(value))?;
                check_range(
                    &name,
                    found as f64,
                    min.map(|m| m as f64),
                    max.map(|m| m as f64),
                )
            }
            ParamKind::Bool => value
                .as_bool()
                .map(|_| ())
                .ok_or_else(|| self.wrong_type(value)),
            ParamKind::Text => value
                .as_str()
                .map(|_| ())
                .ok_or_else(|| self.wrong_type(value)),
            ParamKind::Enum { variants } => {
                let found = value.as_str().ok_or_else(|| self.wrong_type(value))?;
                if variants.contains(&found) {
                    Ok(())
                } else {
                    Err(ConfigError::NotAVariant {
                        name,
                        value: found.to_owned(),
                        options: variants.join(", "),
                    })
                }
            }
        }
    }

    fn wrong_type(&self, value: &PropertyValue) -> ConfigError {
        ConfigError::WrongType {
            name: self.name.to_owned(),
            expected: self.kind.type_name(),
            found: json_type_name(value),
        }
    }
}

fn check_range(
    name: &str,
    value: f64,
    min: Option<f64>,
    max: Option<f64>,
) -> Result<(), ConfigError> {
    let too_low = min.is_some_and(|m| value < m);
    let too_high = max.is_some_and(|m| value > m);
    if too_low || too_high {
        return Err(ConfigError::OutOfRange {
            name: name.to_owned(),
            value,
            min: min.unwrap_or(f64::NEG_INFINITY),
            max: max.unwrap_or(f64::INFINITY),
        });
    }
    Ok(())
}

fn json_type_name(value: &PropertyValue) -> &'static str {
    match value {
        PropertyValue::Null => "nothing",
        PropertyValue::Bool(_) => "true or false",
        PropertyValue::Number(_) => "a number",
        PropertyValue::String(_) => "text",
        PropertyValue::Array(_) => "a list",
        PropertyValue::Object(_) => "an object",
    }
}

/// The parameter values for one stage instance.
///
/// Ordered, so `to_json` is canonical and two sets that differ only in the
/// order they were typed hash the same (§9.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParamSet(BTreeMap<String, PropertyValue>);

impl ParamSet {
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<PropertyValue>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<PropertyValue>) {
        self.0.insert(name.into(), value.into());
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PropertyValue> {
        self.0.get(name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PropertyValue)> {
        self.0.iter()
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// The canonical JSON form: sorted keys, no whitespace. This is what is
    /// stored and what is hashed.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Checks every value against the declaration and reports the first
    /// problem, naming the parameter.
    pub fn validate(&self, specs: &[ParamSpec]) -> Result<(), ConfigError> {
        for (name, value) in &self.0 {
            let spec = specs
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| ConfigError::Unknown(name.clone()))?;
            spec.check(value)?;
        }
        for spec in specs.iter().filter(|s| s.is_required()) {
            if !self.0.contains_key(spec.name) {
                return Err(ConfigError::Missing {
                    name: spec.name.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// This set with every unset parameter filled in from its default.
    ///
    /// Hashing the resolved form is what keeps the cache key stable when the
    /// user types out a value that was the default anyway.
    pub fn resolved(&self, specs: &[ParamSpec]) -> Result<Self, ConfigError> {
        self.validate(specs)?;
        let mut out = self.clone();
        for spec in specs {
            if !out.0.contains_key(spec.name) {
                if let Some(default) = spec.default.value() {
                    out.0.insert(spec.name.to_owned(), default);
                }
            }
        }
        Ok(out)
    }

    // -- Typed reads, for use inside `configure` after validation. ----------

    /// A float parameter, falling back to the spec's default.
    #[must_use]
    pub fn f64_or(&self, name: &str, fallback: f64) -> f64 {
        self.0
            .get(name)
            .and_then(PropertyValue::as_f64)
            .unwrap_or(fallback)
    }

    #[must_use]
    pub fn i64_or(&self, name: &str, fallback: i64) -> i64 {
        self.0
            .get(name)
            .and_then(PropertyValue::as_i64)
            .unwrap_or(fallback)
    }

    #[must_use]
    pub fn bool_or(&self, name: &str, fallback: bool) -> bool {
        self.0
            .get(name)
            .and_then(PropertyValue::as_bool)
            .unwrap_or(fallback)
    }

    #[must_use]
    pub fn str_or<'a>(&'a self, name: &str, fallback: &'a str) -> &'a str {
        self.0
            .get(name)
            .and_then(PropertyValue::as_str)
            .unwrap_or(fallback)
    }
}

impl FromIterator<(String, PropertyValue)> for ParamSet {
    fn from_iter<T: IntoIterator<Item = (String, PropertyValue)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPECS: &[ParamSpec] = &[
        ParamSpec::new(
            "gain",
            "Gain",
            ParamKind::Float {
                min: Some(0.0),
                max: Some(100.0),
            },
            ParamDefault::Float(1.0),
        ),
        ParamSpec::new(
            "mode",
            "Mode",
            ParamKind::Enum {
                variants: &["peak", "rms"],
            },
            ParamDefault::Text("peak"),
        ),
        ParamSpec::required(
            "cutoff_hz",
            "Cutoff",
            ParamKind::FreqHz {
                min: Some(0.0),
                max: None,
            },
        ),
    ];

    fn full() -> ParamSet {
        ParamSet::new().with("cutoff_hz", 1000.0)
    }

    #[test]
    fn a_value_outside_its_declared_range_is_named() {
        let err = full().with("gain", 200.0).validate(SPECS).unwrap_err();
        assert_eq!(err.parameter(), Some("gain"));
        assert_eq!(err.to_string(), "parameter 'gain' is 200, outside 0..=100");
    }

    #[test]
    fn a_parameter_the_stage_does_not_declare_is_refused() {
        let err = full().with("bandwidth", 1.0).validate(SPECS).unwrap_err();
        assert_eq!(err, ConfigError::Unknown("bandwidth".into()));
    }

    #[test]
    fn a_required_parameter_with_no_value_is_reported() {
        let err = ParamSet::new().validate(SPECS).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Missing {
                name: "cutoff_hz".into()
            }
        );
    }

    #[test]
    fn the_wrong_type_says_what_was_expected() {
        let err = full().with("gain", "loud").validate(SPECS).unwrap_err();
        assert_eq!(
            err.to_string(),
            "parameter 'gain' should be a number, not text"
        );
    }

    #[test]
    fn an_enum_lists_its_variants_when_the_value_is_not_one() {
        let err = full().with("mode", "median").validate(SPECS).unwrap_err();
        assert_eq!(
            err.to_string(),
            "parameter 'mode' is 'median'; expected one of peak, rms"
        );
    }

    #[test]
    fn resolving_fills_in_defaults_and_leaves_set_values_alone() {
        let resolved = full().with("gain", 4.0).resolved(SPECS).unwrap();
        assert_eq!(resolved.f64_or("gain", 0.0), 4.0);
        assert_eq!(resolved.str_or("mode", ""), "peak");
        assert_eq!(resolved.f64_or("cutoff_hz", 0.0), 1000.0);
    }

    #[test]
    fn typing_out_the_default_hashes_the_same_as_leaving_it_alone() {
        // Otherwise editing a field back to its default would invalidate the
        // cache for every stage downstream of it (§9.5).
        let implicit = full().resolved(SPECS).unwrap();
        let explicit = full().with("gain", 1.0).resolved(SPECS).unwrap();
        assert_eq!(implicit.to_json(), explicit.to_json());
    }

    #[test]
    fn the_json_form_is_ordered_regardless_of_insertion_order() {
        let one = ParamSet::new().with("b", 2.0).with("a", 1.0);
        let two = ParamSet::new().with("a", 1.0).with("b", 2.0);
        assert_eq!(one.to_json(), two.to_json());
        assert_eq!(one.to_json(), r#"{"a":1.0,"b":2.0}"#);
    }

    #[test]
    fn a_set_round_trips_through_its_stored_json() {
        let set = full().with("mode", "rms");
        let back = ParamSet::from_json(&set.to_json()).unwrap();
        assert_eq!(back, set);
    }

    #[test]
    fn a_declared_kind_generates_the_same_widget_as_a_property() {
        let PropKind::Enum { variants } = SPECS[1].kind.to_prop_kind() else {
            panic!("an enum parameter should generate an enum property");
        };
        assert_eq!(variants, ["peak", "rms"]);
    }
}
