//! The artifact kind registry (`docs/DESIGN.md` §10.1).
//!
//! A stored artifact row carries a kind string and a JSON payload; the Rust
//! type that wrote it is not available to the viewer, and after a plugin is
//! unloaded it may not exist at all. The registry is the lookup from
//! `"detections.v1"` back to an [`ArtifactSchema`], which is everything a
//! viewer needs to draw the payload.
//!
//! Registering an artifact is the other half of "a new artifact type costs one
//! `impl`": the kind declares its own schema, and a mis-declared one — a view
//! hint naming a field that does not exist — is refused here rather than
//! rendering blank.

use std::collections::BTreeMap;

use super::data::{ArtifactData, DataError};
use super::{Artifact, ArtifactSchema};

/// An artifact kind that cannot be registered as declared.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("two artifacts are registered as '{0}'")]
    Duplicate(String),

    #[error("the artifact '{0}' has a view hint naming a field it does not declare")]
    Inconsistent(String),
}

/// What is known about one registered kind.
#[derive(Debug, Clone, PartialEq)]
pub struct KindInfo {
    pub kind: String,
    pub version: u32,
    pub schema: ArtifactSchema,
}

/// Every artifact kind this build can display.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRegistry {
    entries: BTreeMap<String, KindInfo>,
}

impl ArtifactRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `A` under its own `KIND`, so the kind can never disagree
    /// with the type that writes it.
    pub fn register<A: Artifact>(&mut self) -> Result<(), RegistryError> {
        let schema = A::schema();
        if !schema.is_consistent() {
            return Err(RegistryError::Inconsistent(A::KIND.to_owned()));
        }
        if self.entries.contains_key(A::KIND) {
            return Err(RegistryError::Duplicate(A::KIND.to_owned()));
        }
        self.entries.insert(
            A::KIND.to_owned(),
            KindInfo {
                kind: A::KIND.to_owned(),
                version: A::VERSION,
                schema,
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn get(&self, kind: &str) -> Option<&KindInfo> {
        self.entries.get(kind)
    }

    #[must_use]
    pub fn schema(&self, kind: &str) -> Option<&ArtifactSchema> {
        self.entries.get(kind).map(|entry| &entry.schema)
    }

    #[must_use]
    pub fn contains(&self, kind: &str) -> bool {
        self.entries.contains_key(kind)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn kinds(&self) -> impl Iterator<Item = &KindInfo> {
        self.entries.values()
    }

    /// Decodes a stored payload for display.
    ///
    /// An unregistered kind is not an error: it decodes against a
    /// [`ArtifactSchema::opaque`] schema and shows as a JSON tree, which is
    /// the documented fallback (§10.1) and is what a payload written by a
    /// build with more stages in it does.
    pub fn decode(&self, kind: &str, payload: &str) -> Result<ArtifactData, DataError> {
        let schema = self
            .schema(kind)
            .cloned()
            .unwrap_or_else(ArtifactSchema::opaque);
        ArtifactData::decode(schema, payload)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::artifact::{FieldKind, FieldSpec, ViewHint};

    #[derive(Serialize, Deserialize)]
    struct Spectrum {
        freq_hz: Vec<f64>,
        mag_db: Vec<f64>,
    }

    impl Artifact for Spectrum {
        const KIND: &'static str = "spectrum.v1";
        const VERSION: u32 = 1;

        fn schema() -> ArtifactSchema {
            ArtifactSchema::new(
                vec![
                    FieldSpec::new("freq_hz", FieldKind::FloatArray).with_unit("Hz"),
                    FieldSpec::new("mag_db", FieldKind::FloatArray).with_unit("dB"),
                ],
                ViewHint::Series {
                    x: "freq_hz".into(),
                    y: vec!["mag_db".into()],
                    x_log: true,
                    y_log: false,
                },
            )
        }

        fn summary(&self) -> String {
            format!("{} bins", self.freq_hz.len())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Broken;

    impl Artifact for Broken {
        const KIND: &'static str = "broken.v1";
        const VERSION: u32 = 1;

        fn schema() -> ArtifactSchema {
            ArtifactSchema::new(
                Vec::new(),
                ViewHint::Series {
                    x: "absent".into(),
                    y: Vec::new(),
                    x_log: false,
                    y_log: false,
                },
            )
        }

        fn summary(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn a_registered_kind_decodes_its_own_payload() {
        let mut registry = ArtifactRegistry::new();
        registry.register::<Spectrum>().unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.contains("spectrum.v1"));
        assert_eq!(registry.get("spectrum.v1").unwrap().version, 1);

        let data = registry
            .decode(
                "spectrum.v1",
                r#"{"freq_hz":[1.0,2.0],"mag_db":[-3.0,-6.0]}"#,
            )
            .unwrap();
        assert_eq!(data.rows(), 2);
        assert_eq!(data.column("mag_db").unwrap().number_at(1), Some(-6.0));
    }

    #[test]
    fn a_kind_nothing_registered_still_displays_as_a_tree() {
        let registry = ArtifactRegistry::new();
        let data = registry
            .decode("mystery.v3", r#"{"anything":[1,2,3]}"#)
            .unwrap();
        assert!(matches!(data.schema().view, ViewHint::Tree));
        // No fields are declared, so nothing is decoded — the viewer falls
        // back to the raw payload.
        assert_eq!(data.rows(), 0);
    }

    #[test]
    fn registering_the_same_kind_twice_is_refused() {
        let mut registry = ArtifactRegistry::new();
        registry.register::<Spectrum>().unwrap();
        assert_eq!(
            registry.register::<Spectrum>(),
            Err(RegistryError::Duplicate("spectrum.v1".to_owned()))
        );
    }

    #[test]
    fn a_view_hint_naming_an_absent_field_fails_at_registration() {
        let mut registry = ArtifactRegistry::new();
        assert_eq!(
            registry.register::<Broken>(),
            Err(RegistryError::Inconsistent("broken.v1".to_owned()))
        );
        assert!(registry.is_empty());
    }
}
