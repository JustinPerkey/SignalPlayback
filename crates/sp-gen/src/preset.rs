//! The built-in preset library (`docs/DESIGN.md` §8.3, §8.4).
//!
//! > Presets are `GenSpec` JSON files; a small built-in library ships with the
//! > app.
//!
//! A preset file is a `GenSpec` with a name and a description wrapped around
//! it, so the browser has something to list; a bare `GenSpec` file loads too,
//! taking its name from the file. The built-ins are embedded in the binary,
//! which keeps the single-executable promise (goal G4).
//!
//! The library leans on §8.4: a known-answer vector (a tone at a known
//! frequency, a pulse train with known edges), an impairment source to sweep,
//! and inputs for the digital stages that expect an already-sliced signal.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{GenError, Result};
use crate::spec::GenSpec;

/// A named spec, as stored in a preset file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub spec: GenSpec,
}

impl Preset {
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, spec: GenSpec) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            spec,
        }
    }

    /// Parses a preset file. A file holding a bare `GenSpec` is accepted and
    /// named after `fallback_name`, so a spec exported from the generator can
    /// be loaded straight back.
    pub fn from_json(json: &str, fallback_name: &str) -> Result<Self> {
        match serde_json::from_str::<Self>(json) {
            Ok(preset) => Ok(preset),
            Err(wrapped) => match GenSpec::from_json(json) {
                Ok(spec) => Ok(Self::new(fallback_name, String::new(), spec)),
                // The wrapped form is the documented one, so its error is the
                // one worth showing.
                Err(_) => Err(GenError::Json(wrapped)),
            },
        }
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Reads a preset from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path)?;
        let fallback = path
            .file_stem()
            .map_or_else(|| "Preset".to_owned(), |s| s.to_string_lossy().into_owned());
        Self::from_json(&json, &fallback)
    }

    /// Writes a preset to disk.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, self.to_json()?)?;
        Ok(())
    }
}

/// The embedded preset files, in the order the browser lists them.
const BUILT_IN: [(&str, &str); 9] = [
    ("tone-1khz", include_str!("../presets/tone-1khz.json")),
    ("two-tone", include_str!("../presets/two-tone.json")),
    ("noisy-tone", include_str!("../presets/noisy-tone.json")),
    ("linear-chirp", include_str!("../presets/linear-chirp.json")),
    ("pulse-train", include_str!("../presets/pulse-train.json")),
    ("am-tone", include_str!("../presets/am-tone.json")),
    ("fm-tone", include_str!("../presets/fm-tone.json")),
    ("prbs9-nrz", include_str!("../presets/prbs9-nrz.json")),
    ("step-edge", include_str!("../presets/step-edge.json")),
];

/// Every preset that ships with the application.
///
/// A malformed built-in is a build-time mistake, not a runtime condition, and
/// the test below is what catches it; a bad file is skipped rather than
/// stopping the generator screen from opening.
#[must_use]
pub fn built_in() -> Vec<Preset> {
    BUILT_IN
        .iter()
        .filter_map(|(name, json)| match Preset::from_json(json, name) {
            Ok(preset) => Some(preset),
            Err(error) => {
                tracing::error!(%error, preset = name, "a built-in preset will not parse");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Node;
    use crate::validate::validate;

    #[test]
    fn every_built_in_preset_loads_validates_and_renders() {
        let presets = built_in();
        assert_eq!(presets.len(), BUILT_IN.len(), "every built-in parses");
        for preset in presets {
            assert!(!preset.name.is_empty());
            assert!(
                !preset.description.is_empty(),
                "{} has no description",
                preset.name
            );
            let issues = validate(&preset.spec);
            assert!(!issues.blocks(), "{}: {issues}", preset.name);

            let buffer = crate::render::render(&preset.spec)
                .unwrap_or_else(|error| panic!("{}: {error}", preset.name));
            assert_eq!(
                buffer.len() as u64,
                preset.spec.sample_count(),
                "{}",
                preset.name
            );
            assert!(
                buffer.values().all(f64::is_finite),
                "{} renders a non-finite sample",
                preset.name
            );
            assert!(
                buffer.values().any(|v| v != 0.0),
                "{} renders silence",
                preset.name
            );
        }
    }

    #[test]
    fn built_in_names_are_unique() {
        let presets = built_in();
        for (index, preset) in presets.iter().enumerate() {
            assert!(
                !presets[..index].iter().any(|p| p.name == preset.name),
                "{} is listed twice",
                preset.name
            );
        }
    }

    #[test]
    fn a_preset_round_trips_through_json() {
        let preset = Preset::new(
            "Tone",
            "A tone.",
            GenSpec::new(48_000.0, 1.0, Node::default()),
        );
        let json = preset.to_json().unwrap();
        assert_eq!(Preset::from_json(&json, "ignored").unwrap(), preset);
    }

    #[test]
    fn a_bare_spec_file_loads_under_the_file_name() {
        let spec = GenSpec::new(48_000.0, 1.0, Node::default());
        let preset = Preset::from_json(&spec.to_json().unwrap(), "my-spec").unwrap();
        assert_eq!(preset.name, "my-spec");
        assert_eq!(preset.spec, spec);
    }

    #[test]
    fn a_file_that_is_neither_reports_the_wrapped_forms_error() {
        let error = Preset::from_json("{\"name\": \"x\"}", "x").expect_err("no spec");
        assert!(matches!(error, GenError::Json(_)));
    }

    #[test]
    fn a_preset_survives_a_trip_through_the_file_system() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("tone.json");
        let preset = Preset::new(
            "Tone",
            "A tone.",
            GenSpec::new(48_000.0, 1.0, Node::default()),
        );
        preset.save(&path).unwrap();
        assert_eq!(Preset::load(&path).unwrap(), preset);
    }
}
