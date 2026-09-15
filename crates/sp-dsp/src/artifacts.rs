//! Artifact types the built-in stages emit (`docs/DESIGN.md` §10.1).
//!
//! Each is a plain serialisable struct with a declared schema and a view
//! hint; adding one costs an `impl` and no storage or viewer code. The payload
//! convention is parallel arrays — one entry per row of the table, one per
//! detection — which is what the table and overlay viewers read.

use serde::{Deserialize, Serialize};
use sp_core::artifact::{
    Artifact, ArtifactSchema, ColumnSpec, FieldKind, FieldRef, FieldSpec, OverlayForm, ViewHint,
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

/// The single-sided magnitude spectrum of one signal.
///
/// One trace, not one per signal: a port holds the nearest upstream value
/// (§9.3), so publishing a spectrum per signal would leave only the last. The
/// stage says which signal it transformed, and a group whose other signals
/// matter gets a second FFT stage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spectrum {
    /// The signal the spectrum came from.
    pub signal: String,
    /// Bin centres, DC to Nyquist inclusive.
    pub freq_hz: Vec<f64>,
    /// Amplitude in dB, referenced to an amplitude of 1.0 — so a full-scale
    /// sine peaks at 0 dB whatever the transform length.
    pub magnitude_db: Vec<f64>,
    /// Phase per bin in radians, wrapped to `(-pi, pi]`, measured from the
    /// start of the transformed frame. Empty on a spectrum recorded before
    /// phase was published, which is why it defaults rather than being
    /// required: an old run stays readable.
    #[serde(default)]
    pub phase_rad: Vec<f64>,
}

impl Spectrum {
    #[must_use]
    pub fn new(signal: impl Into<String>, freq_hz: Vec<f64>, magnitude_db: Vec<f64>) -> Self {
        Self {
            signal: signal.into(),
            freq_hz,
            magnitude_db,
            phase_rad: Vec::new(),
        }
    }

    /// The same spectrum with its phase attached.
    #[must_use]
    pub fn with_phase(mut self, phase_rad: Vec<f64>) -> Self {
        self.phase_rad = phase_rad;
        self
    }

    /// The phase of the strongest bin, which is the only phase a scalar
    /// comparison between two runs can be made of.
    #[must_use]
    pub fn peak_phase(&self) -> Option<f64> {
        self.phase_rad.get(self.peak_bin()?).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.freq_hz.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.freq_hz.is_empty()
    }

    /// The strongest bin, as `(frequency, dB)` — what the summary and the
    /// across-groups metric both read.
    #[must_use]
    pub fn peak(&self) -> Option<(f64, f64)> {
        let index = self.peak_bin()?;
        Some((self.freq_hz[index], self.magnitude_db[index]))
    }

    /// The index of the strongest bin.
    #[must_use]
    fn peak_bin(&self) -> Option<usize> {
        self.magnitude_db
            .iter()
            .copied()
            .enumerate()
            .fold(None, |best: Option<(usize, f64)>, (index, db)| match best {
                Some((_, top)) if top >= db => best,
                _ => Some((index, db)),
            })
            .map(|(index, _)| index)
            .filter(|index| *index < self.freq_hz.len())
    }
}

impl Artifact for Spectrum {
    const KIND: &'static str = "spectrum.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("signal", FieldKind::Text),
                FieldSpec::new("freq_hz", FieldKind::FloatArray).with_unit("Hz"),
                FieldSpec::new("magnitude_db", FieldKind::FloatArray).with_unit("dB"),
                FieldSpec::new("phase_rad", FieldKind::FloatArray).with_unit("rad"),
            ],
            // Its own axes rather than the scope's: a spectrum has no place on
            // a time axis (§10.1). Phase is on the second axis, because dB and
            // radians share an x axis and nothing else — which is what a Bode
            // plot is.
            ViewHint::Series {
                x: FieldRef::new("freq_hz"),
                y: vec![FieldRef::new("magnitude_db")],
                y2: vec![FieldRef::new("phase_rad")],
                x_log: false,
                y_log: false,
            },
        )
    }

    fn summary(&self) -> String {
        match self.peak() {
            Some((hz, db)) => format!("{}: peak {hz:.1} Hz at {db:.1} dB", self.signal),
            None => format!("{}: empty spectrum", self.signal),
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

/// Local extrema found in a signal (§9.8, Detection).
///
/// Instants rather than spans, so they draw as markers on the scope's own
/// time axis beside the detections a threshold produced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Peaks {
    /// Where each peak is, on the absolute timeline.
    pub time_s: Vec<f64>,
    /// The value reached at the peak.
    pub amplitude: Vec<f64>,
    /// How far the signal has to descend from the peak before it can climb to
    /// a higher one — the measure that separates a feature from a ripple.
    pub prominence: Vec<f64>,
    /// Which signal of the group each peak came from.
    pub signal: Vec<String>,
}

impl Peaks {
    pub fn push(
        &mut self,
        signal: impl Into<String>,
        time_s: f64,
        amplitude: f64,
        prominence: f64,
    ) {
        self.time_s.push(time_s);
        self.amplitude.push(amplitude);
        self.prominence.push(prominence);
        self.signal.push(signal.into());
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.time_s.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.time_s.is_empty()
    }

    /// The most prominent peak.
    ///
    /// Prominence rather than amplitude, because that is the one measure that
    /// means the same thing for a maximum and for a minimum: the tallest of a
    /// set of troughs is the shallowest one.
    #[must_use]
    pub fn strongest(&self) -> Option<usize> {
        let (index, _) = self.prominence.iter().copied().enumerate().fold(
            None,
            |best: Option<(usize, f64)>, (index, value)| match best {
                Some((_, top)) if top >= value => best,
                _ => Some((index, value)),
            },
        )?;
        Some(index)
    }
}

impl Artifact for Peaks {
    const KIND: &'static str = "peaks.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("time_s", FieldKind::TimeS).with_unit("s"),
                FieldSpec::new("amplitude", FieldKind::Float),
                FieldSpec::new("prominence", FieldKind::Float)
                    .describe("Descent required before a higher peak is reached."),
                FieldSpec::new("signal", FieldKind::Text),
            ],
            ViewHint::Overlay {
                form: OverlayForm::Markers,
            },
        )
    }

    fn summary(&self) -> String {
        match self.strongest() {
            Some(at) => format!(
                "{} peak(s), strongest {:.3} at {:.4} s, prominence {:.3}",
                self.len(),
                self.amplitude[at],
                self.time_s[at],
                self.prominence[at]
            ),
            None => "no peaks".to_owned(),
        }
    }
}

/// Decisions taken at symbol instants (§9.8, Digital).
///
/// One row per symbol: when it was sampled, what the waveform read there,
/// which level that was decided as, and how much room the decision had. The
/// margin is what turns "it decoded" into "it decoded with 0.02 to spare",
/// which is the number an eye-closure question is really asking.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Symbols {
    /// The signal the symbols were sliced from.
    pub signal: String,
    /// How many levels the decision was between; 2 for NRZ, 4 for PAM-4.
    pub levels: u32,
    /// Where each symbol was sampled, on the absolute timeline.
    pub time_s: Vec<f64>,
    /// The waveform value at the decision instant.
    pub level: Vec<f64>,
    /// The decided level, 0 being the lowest.
    pub symbol: Vec<i64>,
    /// Distance from the nearest decision boundary. A symbol decided on the
    /// boundary has a margin of zero.
    pub margin: Vec<f64>,
}

impl Symbols {
    #[must_use]
    pub fn new(signal: impl Into<String>, levels: u32) -> Self {
        Self {
            signal: signal.into(),
            levels,
            ..Self::default()
        }
    }

    pub fn push(&mut self, time_s: f64, level: f64, symbol: i64, margin: f64) {
        self.time_s.push(time_s);
        self.level.push(level);
        self.symbol.push(symbol);
        self.margin.push(margin);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.time_s.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.time_s.is_empty()
    }

    /// How many bits each symbol carries: 1 for two levels, 2 for four.
    #[must_use]
    pub fn bits_per_symbol(&self) -> u32 {
        self.levels.max(2).next_power_of_two().trailing_zeros()
    }

    /// The tightest decision in the group — the one that would fail first.
    #[must_use]
    pub fn worst_margin(&self) -> Option<f64> {
        self.margin.iter().copied().reduce(f64::min)
    }
}

impl Artifact for Symbols {
    const KIND: &'static str = "symbols.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("time_s", FieldKind::TimeS).with_unit("s"),
                // Ahead of `symbol` so the overlay draws stems at the
                // amplitude the decision was taken at (§10.1).
                FieldSpec::new("level", FieldKind::Float),
                FieldSpec::new("symbol", FieldKind::Int),
                FieldSpec::new("margin", FieldKind::Float)
                    .describe("Distance from the nearest decision boundary."),
            ],
            ViewHint::Overlay {
                form: OverlayForm::Stems,
            },
        )
    }

    fn summary(&self) -> String {
        match (self.len(), self.worst_margin()) {
            (0, _) => format!("{}: no symbols", self.signal),
            (n, Some(margin)) => format!(
                "{}: {n} symbol(s), {}-level, worst margin {margin:.3}",
                self.signal, self.levels
            ),
            (n, None) => format!("{}: {n} symbol(s)", self.signal),
        }
    }
}

/// A decoded bitstream, packed into words (§9.8, Digital).
///
/// The rows are the words; the header — width, count and the signal it came
/// from — is in the summary, because a bit ribbon wants one line above it
/// rather than four more columns beside it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bits {
    /// The signal the symbols this packs came from.
    pub signal: String,
    /// Bits per word.
    pub width: u32,
    /// How many bits were packed, which the last word may not fill.
    pub count: u64,
    pub index: Vec<i64>,
    /// The packed word.
    pub value: Vec<i64>,
    pub hex: Vec<String>,
    /// The word written out, most significant bit first.
    pub bits: Vec<String>,
}

impl Bits {
    #[must_use]
    pub fn new(signal: impl Into<String>, width: u32) -> Self {
        Self {
            signal: signal.into(),
            width,
            ..Self::default()
        }
    }

    /// Appends one word, written out in the two forms a reader wants.
    pub fn push(&mut self, value: u64, bits_in_word: u32) {
        let digits = (0..bits_in_word)
            .rev()
            .map(|bit| if value >> bit & 1 == 1 { '1' } else { '0' })
            .collect::<String>();
        self.index.push(self.index.len() as i64);
        self.value.push(value as i64);
        self.hex
            .push(format!("{value:0width$X}", width = hex_digits(self.width)));
        self.bits.push(digits);
        self.count += u64::from(bits_in_word);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The whole stream as one string of digits, which is what a comparison
    /// against an expected pattern reads.
    #[must_use]
    pub fn digits(&self) -> String {
        self.bits.concat()
    }
}

/// Hex digits needed to write a word of `width` bits.
fn hex_digits(width: u32) -> usize {
    (width as usize).max(1).div_ceil(4)
}

impl Artifact for Bits {
    const KIND: &'static str = "bits.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("index", FieldKind::Int),
                FieldSpec::new("value", FieldKind::Int),
                FieldSpec::new("hex", FieldKind::Text),
                FieldSpec::new("bits", FieldKind::Text),
            ],
            ViewHint::Table {
                columns: vec![
                    ColumnSpec::new("index", "#"),
                    ColumnSpec::new("hex", "Hex"),
                    ColumnSpec::new("bits", "Bits"),
                    ColumnSpec::new("value", "Value"),
                ],
            },
        )
    }

    fn summary(&self) -> String {
        if self.is_empty() {
            return format!("{}: no bits", self.signal);
        }
        format!(
            "{}: {} bit(s) in {} × {}-bit word(s)",
            self.signal,
            self.count,
            self.len(),
            self.width
        )
    }
}

/// A named scalar table — what a measurement stage emits when its answer is a
/// handful of numbers rather than a trace (§10.1).
///
/// Every row carries its own unit, because a pulse measurement reports
/// seconds, hertz and a bare ratio in the same breath and a column header
/// cannot say all three.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub name: Vec<String>,
    pub value: Vec<f64>,
    pub unit: Vec<String>,
}

impl Metrics {
    pub fn push(&mut self, name: impl Into<String>, value: f64, unit: impl Into<String>) {
        self.name.push(name.into());
        self.value.push(value);
        self.unit.push(unit.into());
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.name.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<f64> {
        let at = self.name.iter().position(|n| n == name)?;
        self.value.get(at).copied()
    }
}

impl Artifact for Metrics {
    const KIND: &'static str = "metrics.v1";
    const VERSION: u32 = 1;

    fn schema() -> ArtifactSchema {
        ArtifactSchema::new(
            vec![
                FieldSpec::new("name", FieldKind::Text),
                FieldSpec::new("value", FieldKind::Float),
                FieldSpec::new("unit", FieldKind::Text),
            ],
            ViewHint::Table {
                columns: vec![
                    ColumnSpec::new("name", "Measurement"),
                    ColumnSpec::new("value", "Value").with_precision(6),
                    ColumnSpec::new("unit", "Unit"),
                ],
            },
        )
    }

    fn summary(&self) -> String {
        match self.len() {
            0 => "nothing measured".to_owned(),
            1 => format!("{}: {:.4} {}", self.name[0], self.value[0], self.unit[0]),
            n => format!("{n} measurements"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_self_consistent() {
        assert!(Statistics::schema().is_consistent());
        assert!(Detections::schema().is_consistent());
        assert!(Spectrum::schema().is_consistent());
        assert!(Peaks::schema().is_consistent());
        assert!(Symbols::schema().is_consistent());
        assert!(Bits::schema().is_consistent());
        assert!(Metrics::schema().is_consistent());
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
    fn a_spectrum_decodes_into_the_columns_the_chart_draws() {
        // The viewer never sees the struct — it decodes the payload against
        // the schema, so that path is what the test has to exercise (§10.1).
        let spectrum = Spectrum::new("rf", vec![0.0, 10.0, 20.0], vec![-3.0, 0.0, -40.0]);
        let payload = serde_json::to_string(&spectrum).unwrap();
        let data = sp_core::artifact::ArtifactData::decode(Spectrum::schema(), &payload).unwrap();
        assert_eq!(data.rows(), 3);
        assert_eq!(
            data.column("magnitude_db")
                .map(sp_core::artifact::Column::len),
            Some(3)
        );
    }

    #[test]
    fn the_peak_is_the_loudest_bin() {
        let spectrum = Spectrum::new("rf", vec![0.0, 10.0, 20.0], vec![-3.0, 0.0, -40.0]);
        assert_eq!(spectrum.peak(), Some((10.0, 0.0)));
        assert_eq!(spectrum.summary(), "rf: peak 10.0 Hz at 0.0 dB");
        assert_eq!(Spectrum::default().peak(), None);
    }

    #[test]
    fn a_peak_is_an_instant_so_it_draws_as_a_marker() {
        let schema = Peaks::schema();
        assert!(schema.field("time_s").unwrap().kind.is_temporal());
        assert!(schema.view.is_overlay());
        assert_eq!(
            schema.view,
            ViewHint::Overlay {
                form: OverlayForm::Markers
            }
        );
    }

    #[test]
    fn the_strongest_peak_is_the_most_prominent_one_not_the_tallest() {
        // A ripple riding high on a plateau is taller than a lone spike and
        // means less; prominence is what says so, and it is the one measure
        // that reads the same for a trough.
        let mut peaks = Peaks::default();
        peaks.push("rf", 0.1, 9.0, 0.2);
        peaks.push("rf", 0.2, 3.0, 2.5);
        assert_eq!(peaks.strongest(), Some(1));
        assert_eq!(
            peaks.summary(),
            "2 peak(s), strongest 3.000 at 0.2000 s, prominence 2.500"
        );
        assert_eq!(Peaks::default().summary(), "no peaks");
    }

    #[test]
    fn symbols_draw_as_stems_at_the_amplitude_they_were_decided_at() {
        // `magnitude_field` takes the first numeric field, so `level` sits
        // ahead of `symbol` in the schema on purpose (§10.1).
        let mut symbols = Symbols::new("rf", 2);
        symbols.push(0.005, -1.0, 0, 1.0);
        symbols.push(0.015, 1.0, 1, 1.0);
        let payload = serde_json::to_string(&symbols).unwrap();
        let data = sp_core::artifact::ArtifactData::decode(Symbols::schema(), &payload).unwrap();
        assert_eq!(data.rows(), 2);
        assert_eq!(
            data.magnitude_field().and_then(|c| c.number_at(1)),
            Some(1.0)
        );
        assert_eq!(
            symbols.summary(),
            "rf: 2 symbol(s), 2-level, worst margin 1.000"
        );
    }

    #[test]
    fn a_symbol_carries_one_bit_at_two_levels_and_two_at_four() {
        assert_eq!(Symbols::new("rf", 2).bits_per_symbol(), 1);
        assert_eq!(Symbols::new("rf", 4).bits_per_symbol(), 2);
        assert_eq!(Symbols::new("rf", 2).worst_margin(), None);
        assert_eq!(Symbols::new("rf", 2).summary(), "rf: no symbols");
    }

    #[test]
    fn a_word_is_written_out_in_both_the_forms_a_reader_wants() {
        let mut bits = Bits::new("rf", 8);
        bits.push(0b1011_0101, 8);
        bits.push(0b101, 3);
        assert_eq!(bits.hex, ["B5", "05"]);
        assert_eq!(bits.bits, ["10110101", "101"]);
        assert_eq!(bits.digits(), "10110101101");
        assert_eq!(bits.count, 11);
        assert_eq!(bits.summary(), "rf: 11 bit(s) in 2 × 8-bit word(s)");
        assert_eq!(Bits::new("rf", 8).summary(), "rf: no bits");
    }

    #[test]
    fn a_measurement_table_is_read_by_name_and_carries_a_unit_per_row() {
        // Seconds, hertz and a bare ratio in the same table: a column header
        // could not have said all three.
        let mut metrics = Metrics::default();
        metrics.push("width_mean", 0.02, "s");
        metrics.push("prf", 10.0, "Hz");
        metrics.push("duty", 0.2, "");
        assert_eq!(metrics.get("prf"), Some(10.0));
        assert_eq!(metrics.get("nothing"), None);
        assert_eq!(metrics.summary(), "3 measurements");
        assert_eq!(Metrics::default().summary(), "nothing measured");

        let payload = serde_json::to_string(&metrics).unwrap();
        let data = sp_core::artifact::ArtifactData::decode(Metrics::schema(), &payload).unwrap();
        assert_eq!(data.rows(), 3);
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
