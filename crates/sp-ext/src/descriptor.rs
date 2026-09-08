//! The descriptor a library publishes, and its translation into the
//! [`StageDescriptor`] the rest of the system already understands
//! (`docs/DESIGN.md` §9.9).
//!
//! `sp_describe` hands back JSON because JSON is the one thing every toolchain
//! can emit without agreeing on struct layout. It is read once, at load time,
//! and the pipeline editor generates the same parameter form from the result
//! as it would for a Rust stage — which is the point: from the editor down, an
//! external stage is not special (G9).
//!
//! [`StageDescriptor`] is `'static` data, so the strings and slices built here
//! are leaked. That is deliberate and bounded: one leak per library per
//! process, and a descriptor genuinely does live as long as the library it
//! describes, which is never unloaded once a run has referenced it.

use serde::Deserialize;
use sp_core::Domain;
use sp_proc::param::{ParamDefault, ParamKind, ParamSpec};
use sp_proc::stage::{PortKind, PortSpec, StageDescriptor};

/// The prefix every external stage kind must carry, so a library can never
/// shadow a built-in and a recorded run always says its stage came from
/// outside this build.
pub const KIND_PREFIX: &str = "ext.";

/// How the host may call one library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// Separate handles may run on separate threads.
    ThreadSafe,
    /// One call at a time. The default, because it is the safe reading of a
    /// library that did not say (§9.9).
    #[default]
    Sequential,
    /// Wants its own process. Accepted and recorded, but M9 runs it in
    /// process: `sp-stage-host` is M14.
    ProcessIsolated,
}

impl Concurrency {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThreadSafe => "thread_safe",
            Self::Sequential => "sequential",
            Self::ProcessIsolated => "process_isolated",
        }
    }

    /// Whether the host must serialise calls into this library.
    #[must_use]
    pub const fn is_exclusive(self) -> bool {
        !matches!(self, Self::ThreadSafe)
    }
}

/// One declared parameter, in the JSON the library publishes.
#[derive(Debug, Clone, Deserialize)]
pub struct ParamJson {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    pub kind: ParamKindJson,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
}

/// A parameter's type and the range the editor will enforce.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParamKindJson {
    Float {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
    },
    Int {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    Bool,
    Text,
    Enum {
        variants: Vec<String>,
    },
    FreqHz {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
    },
    DurationS,
}

/// One declared port.
#[derive(Debug, Clone, Deserialize)]
pub struct PortJson {
    pub name: String,
    /// `signals`, `signals:analog`, `artifact:spectrum.v1` or `any`.
    #[serde(default = "default_port_kind")]
    pub kind: String,
    #[serde(default = "yes")]
    pub required: bool,
}

fn default_port_kind() -> String {
    "signals".to_owned()
}

const fn yes() -> bool {
    true
}

/// Everything a library says about itself.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryDescriptor {
    pub kind: String,
    pub version: u32,
    pub label: String,
    #[serde(default)]
    pub summary: String,
    /// Whether the same input and parameters always give the same output. An
    /// impure library opts out of caching entirely (§9.5).
    #[serde(default = "yes")]
    pub pure: bool,
    #[serde(default)]
    pub concurrency: Concurrency,
    #[serde(default)]
    pub params: Vec<ParamJson>,
    #[serde(default)]
    pub inputs: Vec<PortJson>,
    #[serde(default)]
    pub outputs: Vec<PortJson>,
}

impl LibraryDescriptor {
    /// Reads a descriptor from the JSON `sp_describe` returned.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The `'static` descriptor the registry, the editor and the cache key all
    /// read, or the reason this one cannot be honoured.
    ///
    /// Everything the host can check, it checks here — at load time, in front
    /// of the user who chose the file — rather than when a run is halfway
    /// through a dataset.
    pub fn into_stage_descriptor(self) -> Result<&'static StageDescriptor, String> {
        if !self.kind.starts_with(KIND_PREFIX) {
            return Err(format!(
                "the kind '{}' is not in the '{KIND_PREFIX}' namespace, so it could shadow a built-in stage",
                self.kind
            ));
        }
        if self.label.trim().is_empty() {
            return Err("the stage has no label to show in the palette".to_owned());
        }

        let params: Vec<ParamSpec> = self
            .params
            .into_iter()
            .map(param_spec)
            .collect::<Result<_, _>>()?;
        let inputs: Vec<PortSpec> = self
            .inputs
            .into_iter()
            .map(port_spec)
            .collect::<Result<_, _>>()?;
        let outputs: Vec<PortSpec> = self
            .outputs
            .into_iter()
            .map(port_spec)
            .collect::<Result<_, _>>()?;

        let descriptor = StageDescriptor {
            kind: leak(self.kind),
            version: self.version,
            label: leak(self.label),
            summary: leak(self.summary),
            inputs: Vec::leak(inputs),
            outputs: Vec::leak(outputs),
            params: Vec::leak(params),
            pure: self.pure,
        };
        if !descriptor.is_consistent() {
            return Err("a port or parameter name is used twice".to_owned());
        }
        Ok(Box::leak(Box::new(descriptor)))
    }
}

fn param_spec(param: ParamJson) -> Result<ParamSpec, String> {
    if param.name.trim().is_empty() {
        return Err("a parameter has no name".to_owned());
    }
    let kind = param_kind(&param.name, param.kind)?;
    let default = param_default(&param.name, kind, param.default.as_ref())?;
    let mut spec = ParamSpec::new(
        leak(param.name.clone()),
        leak(param.label.unwrap_or(param.name)),
        kind,
        default,
    );
    if let Some(help) = param.help {
        spec = spec.with_help(leak(help));
    }
    if let Some(unit) = param.unit {
        spec = spec.with_unit(leak(unit));
    }
    Ok(spec)
}

fn param_kind(name: &str, kind: ParamKindJson) -> Result<ParamKind, String> {
    Ok(match kind {
        ParamKindJson::Float { min, max } => ParamKind::Float { min, max },
        ParamKindJson::Int { min, max } => ParamKind::Int { min, max },
        ParamKindJson::Bool => ParamKind::Bool,
        ParamKindJson::Text => ParamKind::Text,
        ParamKindJson::Enum { variants } => {
            if variants.is_empty() {
                return Err(format!("parameter '{name}' is an enum with no variants"));
            }
            let variants: Vec<&'static str> = variants.into_iter().map(leak).collect();
            ParamKind::Enum {
                variants: Vec::leak(variants),
            }
        }
        ParamKindJson::FreqHz { min, max } => ParamKind::FreqHz { min, max },
        ParamKindJson::DurationS => ParamKind::DurationS,
    })
}

/// A default has to be of the type the parameter declared, or the generated
/// form would open with a value the library's own open call would reject.
fn param_default(
    name: &str,
    kind: ParamKind,
    value: Option<&serde_json::Value>,
) -> Result<ParamDefault, String> {
    let Some(value) = value else {
        return Ok(ParamDefault::Required);
    };
    let mismatch = || format!("the default for '{name}' is not {}", kind.type_name());
    Ok(match kind {
        ParamKind::Float { .. } | ParamKind::FreqHz { .. } | ParamKind::DurationS => {
            ParamDefault::Float(value.as_f64().ok_or_else(mismatch)?)
        }
        ParamKind::Int { .. } => ParamDefault::Int(value.as_i64().ok_or_else(mismatch)?),
        ParamKind::Bool => ParamDefault::Bool(value.as_bool().ok_or_else(mismatch)?),
        ParamKind::Text => {
            ParamDefault::Text(leak(value.as_str().ok_or_else(mismatch)?.to_owned()))
        }
        ParamKind::Enum { variants } => {
            let text = value.as_str().ok_or_else(mismatch)?;
            if !variants.contains(&text) {
                return Err(format!(
                    "the default for '{name}' is '{text}', which is not one of {}",
                    variants.join(", ")
                ));
            }
            ParamDefault::Text(leak(text.to_owned()))
        }
    })
}

fn port_spec(port: PortJson) -> Result<PortSpec, String> {
    if port.name.trim().is_empty() {
        return Err("a port has no name".to_owned());
    }
    let kind = port_kind(&port.name, &port.kind)?;
    let name = leak(port.name);
    Ok(if port.required {
        PortSpec::required(name, kind)
    } else {
        PortSpec::optional(name, kind)
    })
}

fn port_kind(name: &str, kind: &str) -> Result<PortKind, String> {
    match kind {
        "any" => Ok(PortKind::Any),
        "signals" => Ok(PortKind::ANY_SIGNALS),
        _ => {
            if let Some(domain) = kind.strip_prefix("signals:") {
                let domain = Domain::ALL
                    .into_iter()
                    .find(|d| d.as_str() == domain)
                    .ok_or_else(|| format!("port '{name}' names an unknown domain '{domain}'"))?;
                return Ok(PortKind::Signals {
                    domain: Some(domain),
                });
            }
            if let Some(artifact) = kind.strip_prefix("artifact:") {
                if artifact.is_empty() {
                    return Err(format!("port '{name}' names no artifact kind"));
                }
                return Ok(PortKind::Artifact(leak(artifact.to_owned())));
            }
            Err(format!("port '{name}' has an unknown kind '{kind}'"))
        }
    }
}

/// One leak per string, at load time. See the module note.
fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(json: &str) -> Result<&'static StageDescriptor, String> {
        LibraryDescriptor::parse(json)
            .map_err(|error| error.to_string())?
            .into_stage_descriptor()
    }

    const FULL: &str = r#"{
        "kind": "ext.vendor.equaliser",
        "version": 3,
        "label": "Vendor equaliser",
        "summary": "The shipped equaliser, as built",
        "concurrency": "thread_safe",
        "params": [
            {"name": "gain_db", "kind": {"type": "float", "min": -20, "max": 20},
             "default": 0.0, "unit": "dB", "help": "Flat gain before the curve"},
            {"name": "curve", "label": "Curve",
             "kind": {"type": "enum", "variants": ["flat", "tilt"]}, "default": "flat"}
        ],
        "inputs": [{"name": "signals", "kind": "signals"}],
        "outputs": [{"name": "signals", "kind": "signals"},
                    {"name": "spectrum", "kind": "artifact:spectrum.v1", "required": false}]
    }"#;

    #[test]
    fn a_full_descriptor_becomes_the_shape_a_rust_stage_declares() {
        let descriptor = descriptor(FULL).unwrap();
        assert_eq!(descriptor.kind, "ext.vendor.equaliser");
        assert_eq!(descriptor.version, 3);
        assert!(descriptor.pure, "a library that says nothing is pure");
        assert_eq!(descriptor.params.len(), 2);
        assert_eq!(descriptor.params[0].unit, Some("dB"));
        assert_eq!(descriptor.params[1].label, "Curve");
        assert_eq!(
            descriptor.outputs[1].kind,
            PortKind::Artifact("spectrum.v1")
        );
        assert!(!descriptor.outputs[1].required);
    }

    #[test]
    fn a_parameter_with_no_label_is_labelled_by_its_name() {
        assert_eq!(descriptor(FULL).unwrap().params[0].label, "gain_db");
    }

    #[test]
    fn a_kind_outside_the_ext_namespace_is_refused() {
        // Otherwise a library could claim 'dsp.filter.biquad' and quietly
        // stand in for a built-in.
        let error =
            descriptor(&FULL.replace("ext.vendor.equaliser", "dsp.filter.biquad")).unwrap_err();
        assert!(error.contains("'ext.' namespace"), "{error}");
    }

    #[test]
    fn a_default_of_the_wrong_type_is_caught_at_load_time() {
        let error =
            descriptor(&FULL.replace(r#""default": 0.0"#, r#""default": "loud""#)).unwrap_err();
        assert_eq!(error, "the default for 'gain_db' is not a number");
    }

    #[test]
    fn an_enum_default_must_be_one_of_its_variants() {
        let error =
            descriptor(&FULL.replace(r#""default": "flat""#, r#""default": "steep""#)).unwrap_err();
        assert!(error.contains("flat, tilt"), "{error}");
    }

    #[test]
    fn a_repeated_parameter_name_is_refused() {
        let error =
            descriptor(&FULL.replace(r#""name": "curve""#, r#""name": "gain_db""#)).unwrap_err();
        assert_eq!(error, "a port or parameter name is used twice");
    }

    #[test]
    fn a_port_naming_an_unknown_domain_says_so() {
        let error = descriptor(&FULL.replace(
            r#"[{"name": "signals", "kind": "signals"}]"#,
            r#"[{"name": "signals", "kind": "signals:radar"}]"#,
        ))
        .unwrap_err();
        assert!(error.contains("radar"), "{error}");
    }

    #[test]
    fn a_parameter_with_no_default_is_required_of_the_user() {
        let json = r#"{"kind":"ext.a.b","version":1,"label":"B",
            "params":[{"name":"taps","kind":{"type":"int","min":1}}]}"#;
        assert!(descriptor(json).unwrap().params[0].is_required());
    }

    #[test]
    fn a_library_that_says_nothing_about_threads_is_sequential() {
        let descriptor =
            LibraryDescriptor::parse(r#"{"kind":"ext.a.b","version":1,"label":"B"}"#).unwrap();
        assert_eq!(descriptor.concurrency, Concurrency::Sequential);
        assert!(descriptor.concurrency.is_exclusive());
    }
}
