//! Typed stage outputs that are not signals (`docs/DESIGN.md` §10.1).
//!
//! A detector emits spans, a demodulator emits symbols, an FFT emits a
//! spectrum. Each is an [`Artifact`]: a serialisable struct with a declared
//! schema and a [`ViewHint`] telling the results screen how to draw it. Adding
//! a new artifact kind costs one `impl` — no storage code, no viewer code.

pub mod data;
pub mod registry;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use data::{diff, infer_schema, ArtifactData, Column, DataError, FieldDiff};
pub use registry::{ArtifactRegistry, KindInfo, RegistryError};

/// A typed, persistable stage output.
pub trait Artifact: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Stable identity including a schema generation, e.g. `detections.v1`.
    const KIND: &'static str;
    /// Bumped when the payload layout changes.
    const VERSION: u32;

    /// Field descriptions plus how to draw it.
    fn schema() -> ArtifactSchema;

    /// One line for the stage rail, e.g. `4 detections, best score 0.91`.
    fn summary(&self) -> String;
}

/// What an artifact contains and how to display it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactSchema {
    pub fields: Vec<FieldSpec>,
    pub view: ViewHint,
}

impl ArtifactSchema {
    #[must_use]
    pub fn new(fields: Vec<FieldSpec>, view: ViewHint) -> Self {
        Self { fields, view }
    }

    /// The schema an unregistered kind is read against: no declared fields,
    /// so the viewer shows the payload as a JSON tree (§10.1).
    #[must_use]
    pub fn opaque() -> Self {
        Self {
            fields: Vec::new(),
            view: ViewHint::Tree,
        }
    }

    #[must_use]
    pub fn field(&self, name: &str) -> Option<&FieldSpec> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Whether every field referenced by the view hint exists. The registry
    /// checks this at startup so a mis-declared artifact fails loudly rather
    /// than rendering blank.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.view
            .referenced_fields()
            .into_iter()
            .all(|name| self.field(name).is_some())
    }
}

/// One field of an artifact payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    pub name: String,
    pub kind: FieldKind,
    pub unit: Option<String>,
    pub description: Option<String>,
}

impl FieldSpec {
    #[must_use]
    pub fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            unit: None,
            description: None,
        }
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    #[must_use]
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// The value type of an artifact field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Float,
    Int,
    Bool,
    Text,
    /// A point on the absolute timeline, in seconds. Time-typed fields are what
    /// let a pane follow the global playhead.
    TimeS,
    /// A span on the absolute timeline, as `(start_s, end_s)`.
    SpanS,
    /// A run of values, e.g. a spectrum's magnitudes.
    FloatArray,
}

impl FieldKind {
    /// Whether the field carries timeline position, and so participates in
    /// playhead synchronisation.
    #[must_use]
    pub const fn is_temporal(self) -> bool {
        matches!(self, Self::TimeS | Self::SpanS)
    }
}

/// Names a field of the artifact by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FieldRef(pub String);

impl FieldRef {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for FieldRef {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

/// A column of a [`ViewHint::Table`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub field: FieldRef,
    pub header: String,
    /// Decimal places for float columns; `None` uses the default formatter.
    pub precision: Option<u8>,
}

impl ColumnSpec {
    #[must_use]
    pub fn new(field: impl Into<FieldRef>, header: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            header: header.into(),
            precision: None,
        }
    }

    #[must_use]
    pub fn with_precision(mut self, precision: u8) -> Self {
        self.precision = Some(precision);
        self
    }
}

/// How overlay artifacts are drawn on the scope's time axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayForm {
    /// Shaded time spans, e.g. detections.
    Spans,
    /// Vertical lines at instants, e.g. edges.
    Markers,
    /// Stems with an amplitude, e.g. symbol decisions.
    Stems,
    /// Horizontal bands at amplitude thresholds.
    Bands,
}

/// How the results screen should render an artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "view")]
pub enum ViewHint {
    /// Rows in a sortable table. The default for anything tabular.
    Table { columns: Vec<ColumnSpec> },
    /// Drawn on the scope's time axis, aligned with the signals.
    Overlay { form: OverlayForm },
    /// A line chart with its own axes: spectrum, filter response, ROC.
    Series {
        x: FieldRef,
        y: Vec<FieldRef>,
        x_log: bool,
        y_log: bool,
    },
    /// A 2-D intensity map: spectrogram, correlation surface.
    Heatmap {
        rows: FieldRef,
        cols: FieldRef,
        values: FieldRef,
    },
    /// Points in a plane: constellation, feature space.
    Scatter {
        x: FieldRef,
        y: FieldRef,
        colour: Option<FieldRef>,
    },
    /// Key/value scalars.
    Scalars,
    /// Fallback: a JSON tree. Always available, never the best choice.
    Tree,
}

impl ViewHint {
    /// Every field this view reads, used to validate a schema.
    #[must_use]
    pub fn referenced_fields(&self) -> Vec<&str> {
        match self {
            Self::Table { columns } => columns.iter().map(|c| c.field.as_str()).collect(),
            Self::Series { x, y, .. } => std::iter::once(x.as_str())
                .chain(y.iter().map(FieldRef::as_str))
                .collect(),
            Self::Heatmap { rows, cols, values } => {
                vec![rows.as_str(), cols.as_str(), values.as_str()]
            }
            Self::Scatter { x, y, colour } => {
                let mut refs = vec![x.as_str(), y.as_str()];
                refs.extend(colour.as_ref().map(FieldRef::as_str));
                refs
            }
            Self::Overlay { .. } | Self::Scalars | Self::Tree => Vec::new(),
        }
    }

    /// Overlay artifacts share the scope's axes instead of getting their own
    /// docked pane (§10.3).
    #[must_use]
    pub const fn is_overlay(&self) -> bool {
        matches!(self, Self::Overlay { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize)]
    struct Detections {
        spans: Vec<(f64, f64)>,
        scores: Vec<f64>,
    }

    impl Artifact for Detections {
        const KIND: &'static str = "detections.v1";
        const VERSION: u32 = 1;

        fn schema() -> ArtifactSchema {
            ArtifactSchema::new(
                vec![
                    FieldSpec::new("spans", FieldKind::SpanS).with_unit("s"),
                    FieldSpec::new("scores", FieldKind::Float),
                ],
                ViewHint::Table {
                    columns: vec![
                        ColumnSpec::new("spans", "Span"),
                        ColumnSpec::new("scores", "Score").with_precision(3),
                    ],
                },
            )
        }

        fn summary(&self) -> String {
            let best = self.scores.iter().copied().fold(f64::NAN, f64::max);
            format!("{} detections, best score {best:.2}", self.spans.len())
        }
    }

    #[test]
    fn a_declared_artifact_is_self_consistent() {
        let schema = Detections::schema();
        assert!(schema.is_consistent());
        assert!(schema.field("spans").unwrap().kind.is_temporal());
        assert_eq!(Detections::KIND, "detections.v1");
    }

    #[test]
    fn a_view_referencing_an_absent_field_is_caught() {
        let schema = ArtifactSchema::new(
            vec![FieldSpec::new("freq_hz", FieldKind::FloatArray)],
            ViewHint::Series {
                x: "freq_hz".into(),
                y: vec!["mag_db".into()],
                x_log: true,
                y_log: false,
            },
        );
        assert!(!schema.is_consistent());
    }

    #[test]
    fn summary_is_what_the_stage_rail_shows() {
        let detections = Detections {
            spans: vec![(0.0, 1.0), (2.0, 2.5)],
            scores: vec![0.4, 0.91],
        };
        assert_eq!(detections.summary(), "2 detections, best score 0.91");
    }

    #[test]
    fn overlays_do_not_claim_a_docked_pane() {
        assert!(ViewHint::Overlay {
            form: OverlayForm::Spans
        }
        .is_overlay());
        assert!(!ViewHint::Scalars.is_overlay());
    }
}
