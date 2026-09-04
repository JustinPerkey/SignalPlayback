//! User-definable signal properties (`docs/DESIGN.md` §6.3).
//!
//! Anything beyond a signal's fixed core is a property: declared once, then
//! typed, validated, displayed and queried consistently. Values are held as
//! JSON so an attribute with no matching definition survives import → database
//! → export verbatim (§6.5, goal G1).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::signal::ParseTokenError;

/// A property value. JSON rather than a closed enum so unrecognised attributes
/// are never lost.
pub type PropertyValue = Value;

/// Which entity a property definition attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PropScope {
    Dataset,
    Group,
    Signal,
}

impl PropScope {
    pub const ALL: [Self; 3] = [Self::Dataset, Self::Group, Self::Signal];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dataset => "dataset",
            Self::Group => "group",
            Self::Signal => "signal",
        }
    }
}

impl fmt::Display for PropScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PropScope {
    type Err = ParseTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|scope| scope.as_str() == s)
            .ok_or_else(|| ParseTokenError {
                kind: "property scope",
                token: s.to_owned(),
            })
    }
}

/// The type of a property, including its validation rules and the editor
/// widget it generates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PropKind {
    Float {
        min: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
    },
    Int {
        min: Option<i64>,
        max: Option<i64>,
    },
    Bool,
    Text {
        pattern: Option<String>,
        max_len: Option<usize>,
    },
    Enum {
        variants: Vec<String>,
    },
    /// A frequency, rendered with unit scaling (Hz / kHz / MHz).
    FreqHz {
        min: Option<f64>,
        max: Option<f64>,
    },
    DurationS,
    TimeUtc,
    Ratio {
        as_db: bool,
    },
    /// Points at another signal in the library.
    SignalRef,
}

impl PropKind {
    /// Whether values of this kind are stored in `signal_property.num_value`
    /// (as opposed to `txt_value`); this is what the indexed mirror keys on.
    #[must_use]
    pub const fn is_numeric(&self) -> bool {
        matches!(
            self,
            Self::Float { .. }
                | Self::Int { .. }
                | Self::FreqHz { .. }
                | Self::DurationS
                | Self::Ratio { .. }
                | Self::SignalRef
        )
    }

    /// A short type name for the properties screen.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Float { .. } => "float",
            Self::Int { .. } => "int",
            Self::Bool => "bool",
            Self::Text { .. } => "text",
            Self::Enum { .. } => "enum",
            Self::FreqHz { .. } => "frequency",
            Self::DurationS => "duration",
            Self::TimeUtc => "timestamp",
            Self::Ratio { .. } => "ratio",
            Self::SignalRef => "signal ref",
        }
    }
}

/// A property declaration: one row of the `property_def` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyDef {
    /// `snake_case`, unique within the scope. This is the key used in
    /// `attributes` and in queries.
    pub key: String,
    /// Display label, e.g. `PRF`.
    pub label: String,
    pub scope: PropScope,
    pub kind: PropKind,
    /// Engineering unit, e.g. `Hz`, `dB`, `s`.
    pub unit: Option<String>,
    pub default: Option<PropertyValue>,
    pub required: bool,
    /// Groups fields in the editor, e.g. `RF`, `Timing`.
    pub section: Option<String>,
    pub ordinal: i32,
}

impl PropertyDef {
    /// A definition with everything optional left empty.
    #[must_use]
    pub fn new(key: impl Into<String>, scope: PropScope, kind: PropKind) -> Self {
        let key = key.into();
        Self {
            label: key.clone(),
            key,
            scope,
            kind,
            unit: None,
            default: None,
            required: false,
            section: None,
            ordinal: 0,
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    #[must_use]
    pub fn with_default(mut self, default: PropertyValue) -> Self {
        self.default = Some(default);
        self
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    #[must_use]
    pub fn in_section(mut self, section: impl Into<String>) -> Self {
        self.section = Some(section.into());
        self
    }

    /// Checks a value against the definition. `null` is accepted unless the
    /// property is required, so an optional field can be explicitly empty.
    pub fn validate(&self, value: &PropertyValue) -> Result<(), PropertyError> {
        if value.is_null() {
            return if self.required {
                Err(PropertyError::Missing {
                    key: self.key.clone(),
                })
            } else {
                Ok(())
            };
        }

        let key = || self.key.clone();
        let wrong_type = |expected: &'static str| PropertyError::WrongType {
            key: self.key.clone(),
            expected,
            found: json_type_name(value),
        };

        match &self.kind {
            PropKind::Float { min, max, .. } => {
                let v = value.as_f64().ok_or_else(|| wrong_type("number"))?;
                check_range(&self.key, v, *min, *max)?;
            }
            PropKind::FreqHz { min, max } => {
                let v = value.as_f64().ok_or_else(|| wrong_type("number"))?;
                check_range(&self.key, v, min.or(Some(0.0)), *max)?;
            }
            PropKind::DurationS => {
                let v = value.as_f64().ok_or_else(|| wrong_type("number"))?;
                check_range(&self.key, v, Some(0.0), None)?;
            }
            PropKind::Ratio { .. } => {
                value.as_f64().ok_or_else(|| wrong_type("number"))?;
            }
            PropKind::Int { min, max } => {
                let v = value.as_i64().ok_or_else(|| wrong_type("integer"))?;
                check_range(
                    &self.key,
                    v as f64,
                    min.map(|m| m as f64),
                    max.map(|m| m as f64),
                )?;
            }
            PropKind::SignalRef => {
                value.as_i64().ok_or_else(|| wrong_type("signal id"))?;
            }
            PropKind::Bool => {
                value.as_bool().ok_or_else(|| wrong_type("boolean"))?;
            }
            PropKind::Text { max_len, .. } => {
                let s = value.as_str().ok_or_else(|| wrong_type("string"))?;
                if let Some(max_len) = max_len {
                    if s.chars().count() > *max_len {
                        return Err(PropertyError::TooLong {
                            key: key(),
                            max_len: *max_len,
                        });
                    }
                }
            }
            PropKind::TimeUtc => {
                value.as_str().ok_or_else(|| wrong_type("timestamp"))?;
            }
            PropKind::Enum { variants } => {
                let s = value.as_str().ok_or_else(|| wrong_type("string"))?;
                if !variants.iter().any(|v| v == s) {
                    return Err(PropertyError::NotAVariant {
                        key: key(),
                        found: s.to_owned(),
                        variants: variants.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Parses a raw string — a CSV cell, or text typed into the inspector —
    /// into a validated value of this property's kind.
    pub fn parse(&self, raw: &str) -> Result<PropertyValue, PropertyError> {
        let raw = raw.trim();
        if raw.is_empty() {
            let value = self.default.clone().unwrap_or(Value::Null);
            self.validate(&value)?;
            return Ok(value);
        }

        let parse_number = |raw: &str| -> Result<Value, PropertyError> {
            raw.parse::<f64>()
                .map(Value::from)
                .map_err(|_| PropertyError::Unparseable {
                    key: self.key.clone(),
                    expected: "number",
                    raw: raw.to_owned(),
                })
        };

        let value = match &self.kind {
            PropKind::Float { .. }
            | PropKind::FreqHz { .. }
            | PropKind::DurationS
            | PropKind::Ratio { .. } => parse_number(raw)?,
            PropKind::Int { .. } | PropKind::SignalRef => {
                Value::from(raw.parse::<i64>().map_err(|_| PropertyError::Unparseable {
                    key: self.key.clone(),
                    expected: "integer",
                    raw: raw.to_owned(),
                })?)
            }
            PropKind::Bool => match raw.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "y" | "on" => Value::Bool(true),
                "0" | "false" | "no" | "n" | "off" => Value::Bool(false),
                _ => {
                    return Err(PropertyError::Unparseable {
                        key: self.key.clone(),
                        expected: "boolean",
                        raw: raw.to_owned(),
                    })
                }
            },
            PropKind::Text { .. } | PropKind::TimeUtc | PropKind::Enum { .. } => {
                Value::String(raw.to_owned())
            }
        };
        self.validate(&value)?;
        Ok(value)
    }
}

fn check_range(
    key: &str,
    value: f64,
    min: Option<f64>,
    max: Option<f64>,
) -> Result<(), PropertyError> {
    if !value.is_finite() {
        return Err(PropertyError::NotFinite {
            key: key.to_owned(),
        });
    }
    let below = min.is_some_and(|min| value < min);
    let above = max.is_some_and(|max| value > max);
    if below || above {
        return Err(PropertyError::OutOfRange {
            key: key.to_owned(),
            value,
            min,
            max,
        });
    }
    Ok(())
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Why a property value was rejected.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PropertyError {
    #[error("'{key}' is required but was not supplied")]
    Missing { key: String },
    #[error("'{key}' expects a {expected} but found a {found}")]
    WrongType {
        key: String,
        expected: &'static str,
        found: &'static str,
    },
    #[error("'{key}' = {value} is outside {}..{}",
        min.map_or_else(|| "-inf".to_owned(), |m| m.to_string()),
        max.map_or_else(|| "+inf".to_owned(), |m| m.to_string()))]
    OutOfRange {
        key: String,
        value: f64,
        min: Option<f64>,
        max: Option<f64>,
    },
    #[error("'{key}' must be a finite number")]
    NotFinite { key: String },
    #[error("'{key}' is longer than {max_len} characters")]
    TooLong { key: String, max_len: usize },
    #[error("'{key}' = '{found}' is not one of: {}", variants.join(", "))]
    NotAVariant {
        key: String,
        found: String,
        variants: Vec<String>,
    },
    #[error("'{key}' could not read '{raw}' as a {expected}")]
    Unparseable {
        key: String,
        expected: &'static str,
        raw: String,
    },
}

/// The property values carried by a dataset, group or signal.
///
/// Keys with no matching [`PropertyDef`] are kept as-is; the inspector shows
/// them under "Unrecognised" with a promote-to-property action.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Attributes(BTreeMap<String, PropertyValue>);

impl Attributes {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&PropertyValue> {
        self.0.get(key)
    }

    #[must_use]
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.0.get(key)?.as_f64()
    }

    #[must_use]
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.0.get(key)?.as_i64()
    }

    #[must_use]
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.0.get(key)?.as_bool()
    }

    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key)?.as_str()
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<PropertyValue>) {
        self.0.insert(key.into(), value.into());
    }

    pub fn remove(&mut self, key: &str) -> Option<PropertyValue> {
        self.0.remove(key)
    }

    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &PropertyValue)> {
        self.0.iter()
    }

    /// Keys with no definition in `defs`. These are the values the inspector
    /// lists as unrecognised.
    #[must_use]
    pub fn unrecognised<'a>(&'a self, defs: &'a [PropertyDef]) -> Vec<&'a str> {
        self.0
            .keys()
            .filter(|key| !defs.iter().any(|def| &def.key == *key))
            .map(String::as_str)
            .collect()
    }

    /// Validates every value that has a matching definition, and reports every
    /// required definition with no value. Unrecognised keys are left alone.
    pub fn validate(&self, defs: &[PropertyDef]) -> Vec<PropertyError> {
        let mut errors = Vec::new();
        for def in defs {
            match self.0.get(&def.key) {
                Some(value) => {
                    if let Err(err) = def.validate(value) {
                        errors.push(err);
                    }
                }
                None if def.required && def.default.is_none() => {
                    errors.push(PropertyError::Missing {
                        key: def.key.clone(),
                    });
                }
                None => {}
            }
        }
        errors
    }
}

impl FromIterator<(String, PropertyValue)> for Attributes {
    fn from_iter<T: IntoIterator<Item = (String, PropertyValue)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A named, reusable bundle of property definitions, e.g. "Pulse-Doppler
/// capture" or "BPSK link test".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertySet {
    pub name: String,
    pub notes: Option<String>,
    /// Definition keys belonging to the set.
    pub members: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prf() -> PropertyDef {
        PropertyDef::new(
            "prf_hz",
            PropScope::Signal,
            PropKind::FreqHz {
                min: None,
                max: Some(1.0e6),
            },
        )
        .with_label("PRF")
        .with_unit("Hz")
    }

    #[test]
    fn frequency_rejects_negative_and_out_of_range() {
        let def = prf();
        assert!(def.validate(&json!(1_000.0)).is_ok());
        assert!(matches!(
            def.validate(&json!(-1.0)),
            Err(PropertyError::OutOfRange { .. })
        ));
        assert!(matches!(
            def.validate(&json!(2.0e6)),
            Err(PropertyError::OutOfRange { .. })
        ));
        assert!(matches!(
            def.validate(&json!("fast")),
            Err(PropertyError::WrongType { .. })
        ));
    }

    #[test]
    fn parsing_a_csv_cell_produces_a_validated_value() {
        let def = prf();
        assert_eq!(def.parse("1500").unwrap(), json!(1500.0));
        assert!(def.parse("1.5e3").is_ok());
        assert!(matches!(
            def.parse("1,500"),
            Err(PropertyError::Unparseable { .. })
        ));
    }

    #[test]
    fn empty_cell_falls_back_to_the_default() {
        let def = prf().with_default(json!(1_000.0));
        assert_eq!(def.parse("  ").unwrap(), json!(1_000.0));

        let required = PropertyDef::new("mode", PropScope::Signal, PropKind::Bool).required();
        assert!(matches!(
            required.parse(""),
            Err(PropertyError::Missing { .. })
        ));
    }

    #[test]
    fn enum_values_must_be_declared_variants() {
        let def = PropertyDef::new(
            "coding",
            PropScope::Signal,
            PropKind::Enum {
                variants: vec!["nrz".into(), "manchester".into()],
            },
        );
        assert!(def.parse("manchester").is_ok());
        assert!(matches!(
            def.parse("rz"),
            Err(PropertyError::NotAVariant { .. })
        ));
    }

    #[test]
    fn booleans_accept_the_spellings_csv_files_actually_use() {
        let def = PropertyDef::new("inverted", PropScope::Signal, PropKind::Bool);
        for raw in ["1", "true", "TRUE", "yes", "on"] {
            assert_eq!(def.parse(raw).unwrap(), json!(true), "parsing {raw}");
        }
        for raw in ["0", "false", "no", "off"] {
            assert_eq!(def.parse(raw).unwrap(), json!(false), "parsing {raw}");
        }
        assert!(def.parse("maybe").is_err());
    }

    #[test]
    fn unrecognised_attributes_are_preserved_and_reported() {
        let mut attrs = Attributes::new();
        attrs.insert("prf_hz", 1_000.0);
        attrs.insert("operator_note", "check cabling");

        let defs = [prf()];
        assert_eq!(attrs.unrecognised(&defs), ["operator_note"]);
        assert!(attrs.validate(&defs).is_empty());
        // Round-tripping through JSON keeps the undeclared key (G1).
        let json = serde_json::to_string(&attrs).unwrap();
        let back: Attributes = serde_json::from_str(&json).unwrap();
        assert_eq!(back, attrs);
    }

    #[test]
    fn missing_required_property_is_reported_once() {
        let attrs = Attributes::new();
        let defs = [prf().required()];
        let errors = attrs.validate(&defs);
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], PropertyError::Missing { .. }));
    }
}
