//! Artifact types the built-in stages emit (`docs/DESIGN.md` §10.1).
//!
//! Each is a plain serialisable struct with a declared schema and a view
//! hint; adding one costs an `impl` and no storage or viewer code. The payload
//! convention is parallel arrays — one entry per row of the table, one per
//! detection — which is what the table and overlay viewers read.

use serde::{Deserialize, Serialize};
use sp_core::artifact::{
    Artifact, ArtifactSchema, ColumnSpec, FieldKind, FieldSpec, OverlayForm, ViewHint,
};

/// Per-signal summary statistics for one group.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Statistics {
    pub names: Vec<String>,
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    pub mean: Vec<f64>,
    pub rms: Vec<f64>,
}

impl Statistics {
    pub fn push(&mut self, name: impl Into<String>, min: f64, max: f64, mean: f64, rms: f64) {
        self.names.push(name.into());
        self.min.push(min);
        self.max.push(max);
        self.mean.push(mean);
        self.rms.push(rms);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

impl Artifact for Statistics {
    const KIND: &'static str = "statistics.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("names", FieldKind::Text),
                FieldSpec::new("min", FieldKind::Float),
                FieldSpec::new("max", FieldKind::Float),
                FieldSpec::new("mean", FieldKind::Float),
                FieldSpec::new("rms", FieldKind::Float),
            ],
            ViewHint::Table {
                columns: vec![
                    ColumnSpec::new("names", "Signal"),
                    ColumnSpec::new("min", "Min").with_precision(4),
                    ColumnSpec::new("max", "Max").with_precision(4),
                    ColumnSpec::new("mean", "Mean").with_precision(4),
                    ColumnSpec::new("rms", "RMS").with_precision(4),
                ],
            },
        )
    }

    fn summary(&self) -> String {
        match self.names.len() {
            1 => format!("{}: rms {:.3}", self.names[0], self.rms[0]),
            n => format!("{n} signals summarised"),
        }
    }
}

/// Time spans where a signal crossed a threshold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Detections {
    /// `(start_s, end_s)` on the absolute timeline.
    pub spans: Vec<(f64, f64)>,
    /// The extreme value reached inside each span.
    pub peak: Vec<f64>,
    /// Which signal of the group each detection came from.
    pub signal: Vec<String>,
}

impl Detections {
    pub fn push(&mut self, signal: impl Into<String>, span: (f64, f64), peak: f64) {
        self.spans.push(span);
        self.peak.push(peak);
        self.signal.push(signal.into());
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The longest span, which is what a pulse-width check reads.
    #[must_use]
    pub fn widest_s(&self) -> f64 {
        self.spans
            .iter()
            .map(|(start, end)| end - start)
            .fold(0.0, f64::max)
    }
}

impl Artifact for Detections {
    const KIND: &'static str = "detections.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("spans", FieldKind::SpanS).with_unit("s"),
                FieldSpec::new("peak", FieldKind::Float),
                FieldSpec::new("signal", FieldKind::Text),
            ],
            // Detections belong on the scope's own time axis, aligned with the
            // signal they were found in, rather than in a docked pane (§10.3).
            ViewHint::Overlay {
                form: OverlayForm::Spans,
            },
        )
    }

    fn summary(&self) -> String {
        match self.spans.len() {
            0 => "no detections".to_owned(),
            n => format!(
                "{n} detection(s), widest {:.4} s, peak {:.3}",
                self.widest_s(),
                self.peak.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_schemas_are_self_consistent() {
        assert!(Statistics::schema().is_consistent());
        assert!(Detections::schema().is_consistent());
    }

    #[test]
    fn a_detection_span_is_temporal_so_it_follows_the_playhead() {
        let schema = Detections::schema();
        assert!(schema.field("spans").unwrap().kind.is_temporal());
        assert!(schema.view.is_overlay());
    }

    #[test]
    fn summaries_are_what_the_stage_rail_shows() {
        let mut detections = Detections::default();
        detections.push("rf", (1.0, 1.25), 0.9);
        assert_eq!(
            detections.summary(),
            "1 detection(s), widest 0.2500 s, peak 0.900"
        );
        assert_eq!(Detections::default().summary(), "no detections");

        let mut stats = Statistics::default();
        stats.push("rf", -1.0, 1.0, 0.0, 0.707);
        assert_eq!(stats.summary(), "rf: rms 0.707");
    }

    #[test]
    fn payloads_round_trip_through_json() {
        let mut detections = Detections::default();
        detections.push("rf", (0.5, 0.75), 1.5);
        let json = serde_json::to_string(&detections).unwrap();
        let back: Detections = serde_json::from_str(&json).unwrap();
        assert_eq!(back, detections);
    }
}
