//! Generation into the library (`docs/DESIGN.md` §8.3, §8.4, §8.5).
//!
//! One action produces one dataset holding one **train**, because a train is
//! what an import produces too (§6.6) and the two have to be interchangeable
//! as pipeline input.
//!
//! [`generate`] fills that train with one group of sampled signals: a single
//! signal, or one per rung of a sweep with the swept value stored as a
//! property on each. That is the shape a pipeline wants as test input (§8.3) —
//! a run can group by the property and the degradation curve falls out of one
//! run. [`generate_train`] fills it with groups of pulse records instead.
//!
//! The whole batch is one SQLite transaction, so a failure or a cancellation
//! rolls back the rows *and* the blobs, exactly as import does (§14).

use sp_core::group::DatasetId;
use sp_core::props::PropertyValue;
use sp_core::{
    Attributes, GroupId, PropKind, PropScope, PropertyDef, Provenance, SampleBuffer, SampleRange,
    SourceKind, TrainId,
};
use sp_store::{
    library, props, pulses, trains, Connection, NewDataset, NewGroup, NewPulseField, NewPulseGroup,
    NewSignal, NewTrain,
};

use crate::control::{GenControl, GenProgress};
use crate::error::{GenError, Result};
use crate::render::{self, SourceSignal, Sources};
use crate::spec::GenSpec;
use crate::sweep::{self, ParamSweep};
use crate::train::TrainSpec;
use crate::validate::{self, validate, validate_train};

/// The attribute every generated signal carries, naming the seed that produced
/// it. The spec itself lives in `signal.gen_spec`.
pub const SEED_KEY: &str = "gen_seed";

/// The attribute a generated train carries its `TrainSpec` in, the way a
/// generated signal carries its `GenSpec` in `signal.gen_spec`.
pub const TRAIN_SPEC_KEY: &str = "gen_train_spec";

/// What to generate, and how to describe it in the library.
#[derive(Debug, Clone)]
pub struct GenRequest {
    /// Dataset name. Empty falls back to the signal name.
    pub name: String,
    /// Group name; defaults to the dataset name.
    pub group_name: Option<String>,
    /// Base name for the signals. A sweep appends each rung's value.
    pub signal_name: String,
    pub spec: GenSpec,
    /// The sweep, when one signal is not enough (§8.3).
    pub sweep: Option<ParamSweep>,
    pub notes: Option<String>,
}

impl GenRequest {
    #[must_use]
    pub fn new(spec: GenSpec) -> Self {
        Self {
            name: String::new(),
            group_name: None,
            signal_name: "Generated".to_owned(),
            spec,
            sweep: None,
            notes: None,
        }
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    #[must_use]
    pub fn with_signal_name(mut self, name: impl Into<String>) -> Self {
        self.signal_name = name.into();
        self
    }

    #[must_use]
    pub fn with_sweep(mut self, sweep: ParamSweep) -> Self {
        self.sweep = Some(sweep);
        self
    }

    /// The dataset name actually used.
    #[must_use]
    pub fn dataset_name(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            let fallback = self.signal_name.trim();
            if fallback.is_empty() {
                "Generated".to_owned()
            } else {
                fallback.to_owned()
            }
        } else {
            name.to_owned()
        }
    }

    /// The specs to render, one per signal, each with the swept value that
    /// produced it.
    pub fn rungs(&self) -> Result<Vec<(Option<f64>, GenSpec)>> {
        match &self.sweep {
            None => Ok(vec![(None, self.spec.clone())]),
            Some(sweep) => sweep::expand(&self.spec, sweep)
                .map_err(|reason| {
                    GenError::Invalid(validate::sweep_issue(&sweep.json_pointer(), &reason))
                })
                .map(|rungs| {
                    rungs
                        .into_iter()
                        .map(|(value, spec)| (Some(value), spec))
                        .collect()
                }),
        }
    }
}

/// What a generation did.
#[derive(Debug, Clone, PartialEq)]
pub struct GenReport {
    pub dataset_id: DatasetId,
    /// The train everything was written under. Both modes produce one.
    pub train_id: TrainId,
    /// The first group written, which is the only one in waveform mode.
    pub group_id: GroupId,
    pub name: String,
    pub groups: u32,
    /// Sampled signals written (waveform mode).
    pub signals: u32,
    pub samples: u64,
    /// Pulse records written (pulse-train mode).
    pub pulses: u64,
    /// The property the swept value was stored under, when there was a sweep.
    pub swept_property: Option<String>,
}

impl GenReport {
    /// A one-line summary for the status bar.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.pulses > 0 {
            return format!(
                "{}: {} group{}, {} pulse{}",
                self.name,
                self.groups,
                plural(u64::from(self.groups)),
                self.pulses,
                plural(self.pulses),
            );
        }
        let mut summary = format!(
            "{}: {} signal{}, {} sample{}",
            self.name,
            self.signals,
            plural(u64::from(self.signals)),
            self.samples,
            plural(self.samples),
        );
        if let Some(key) = &self.swept_property {
            summary.push_str(&format!(", swept by {key}"));
        }
        summary
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Renders a request and writes it to the library.
///
/// `conn` is the store's writer connection; the caller owns the transaction
/// boundary, so a failure here leaves nothing behind.
pub fn generate(
    conn: &mut Connection,
    request: &GenRequest,
    control: &GenControl,
) -> Result<GenReport> {
    let rungs = request.rungs()?;
    for (_, spec) in &rungs {
        let issues = validate(spec);
        if issues.blocks() {
            return Err(GenError::Invalid(issues));
        }
    }

    let sources = resolve_sources(conn, &rungs)?;
    let signals_total = rungs.len() as u32;
    let samples_total: u64 = rungs.iter().map(|(_, spec)| spec.sample_count()).sum();
    let name = request.dataset_name();

    let tx = conn.transaction().map_err(sp_store::StoreError::from)?;

    let dataset_id = library::insert_dataset(
        &tx,
        &NewDataset {
            name: name.clone(),
            source_kind: SourceKind::Generated,
            source_uri: None,
            notes: request.notes.clone(),
            attributes: Attributes::new(),
        },
    )?;

    // A generated batch is a train of one group, so it lands in the library
    // the same shape an imported file does (§6.6).
    let train_id = trains::insert_train(
        &tx,
        &NewTrain::new(dataset_id, 0)
            .named(request.group_name.clone().unwrap_or_else(|| name.clone())),
    )?;

    let mut group_attributes = Attributes::new();
    if let Some(sweep) = &request.sweep {
        group_attributes.insert("gen_sweep_parameter", sweep.json_pointer());
        group_attributes.insert("gen_sweep_property", sweep.property_key.clone());
    }
    let group_id = library::insert_group(
        &tx,
        &NewGroup {
            train_id,
            ordinal: 0,
            name: Some(request.group_name.clone().unwrap_or_else(|| name.clone())),
            declared_count: signals_total,
            actual_count: signals_total,
            attributes: group_attributes,
        },
    )?;

    if let Some(sweep) = &request.sweep {
        declare_swept_property(&tx, sweep)?;
    }

    let mut samples_done = 0;
    for (ordinal, (value, spec)) in rungs.iter().enumerate() {
        control.check()?;

        let buffer = render_rung(spec, &sources, control)?;
        let mut attributes = Attributes::new();
        attributes.insert(SEED_KEY, spec.seed);
        if let (Some(sweep), Some(value)) = (&request.sweep, value) {
            attributes.insert(sweep.property_key.clone(), PropertyValue::from(*value));
        }

        let signal = NewSignal {
            group_id,
            ordinal: ordinal as u32,
            name: signal_name(request, *value),
            units: None,
            domain: spec.domain,
            provenance: Provenance::Generated,
            timebase: spec.timebase,
            samples: buffer,
            attributes,
            gen_spec: Some(spec.to_json()?),
        };
        samples_done += signal.samples.len() as u64;
        library::insert_signal(&tx, &signal)?;

        control.report(GenProgress {
            items_done: ordinal as u32 + 1,
            items_total: signals_total,
            values_done: samples_done,
            values_total: samples_total,
        });
    }

    control.check()?;
    tx.commit().map_err(sp_store::StoreError::from)?;

    Ok(GenReport {
        dataset_id,
        train_id,
        group_id,
        name,
        groups: 1,
        signals: signals_total,
        samples: samples_done,
        pulses: 0,
        swept_property: request.sweep.as_ref().map(|s| s.property_key.clone()),
    })
}

/// What pulse train to generate, and how to describe it in the library.
#[derive(Debug, Clone)]
pub struct TrainRequest {
    /// Dataset name. Empty falls back to the train name.
    pub name: String,
    /// Train name; defaults to the dataset name.
    pub train_name: Option<String>,
    pub spec: TrainSpec,
    pub notes: Option<String>,
}

impl TrainRequest {
    #[must_use]
    pub fn new(spec: TrainSpec) -> Self {
        Self {
            name: String::new(),
            train_name: None,
            spec,
            notes: None,
        }
    }

    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The dataset name actually used.
    #[must_use]
    pub fn dataset_name(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            return name.to_owned();
        }
        match self.train_name.as_deref().map(str::trim) {
            Some(train) if !train.is_empty() => train.to_owned(),
            _ => "Generated train".to_owned(),
        }
    }
}

/// Renders a pulse train and writes it to the library (§8.5).
///
/// The result is indistinguishable in shape from an import: one train, its
/// groups, a time-of-arrival column and one column per field.
pub fn generate_train(
    conn: &mut Connection,
    request: &TrainRequest,
    control: &GenControl,
) -> Result<GenReport> {
    let spec = &request.spec;
    let issues = validate_train(spec);
    if issues.blocks() {
        return Err(GenError::Invalid(issues));
    }

    let name = request.dataset_name();
    let train_name = request.train_name.clone().unwrap_or_else(|| name.clone());
    let pulses_total = spec.pulses();

    let tx = conn.transaction().map_err(sp_store::StoreError::from)?;

    let dataset_id = library::insert_dataset(
        &tx,
        &NewDataset {
            name: name.clone(),
            source_kind: SourceKind::Generated,
            source_uri: None,
            notes: request.notes.clone(),
            attributes: Attributes::new(),
        },
    )?;

    let mut train_attributes = Attributes::new();
    train_attributes.insert(SEED_KEY, spec.seed);
    train_attributes.insert(TRAIN_SPEC_KEY, serde_json::to_value(spec)?);
    let train_id = trains::insert_train(
        &tx,
        &NewTrain::new(dataset_id, 0)
            .named(train_name)
            .with_toa_unit(spec.toa_unit)
            .with_attributes(train_attributes),
    )?;

    let mut first_group = None;
    let mut pulses_done = 0u64;
    for index in 0..spec.groups {
        control.check()?;
        let data = spec.group(index);
        let count = data.toa_s.len() as u64;

        let mut group = NewPulseGroup::new(train_id, index, data.toa_s);
        group.name = Some(format!("{}", index + 1));
        group.toa_unit = spec.toa_unit;
        for (field, values) in spec.fields.iter().zip(data.fields) {
            let mut column = NewPulseField::new(field.name.clone(), values);
            if let Some(unit) = &field.unit {
                column = column.with_unit(unit.clone());
            }
            group.fields.push(column);
        }

        let group_id = pulses::insert_pulse_group(&tx, &group)?;
        first_group.get_or_insert(group_id);
        pulses_done += count;
        control.report(GenProgress {
            items_done: index + 1,
            items_total: spec.groups,
            values_done: pulses_done,
            values_total: pulses_total,
        });
    }

    control.check()?;
    let group_id = first_group.ok_or_else(|| {
        GenError::Invalid(validate::sweep_issue("", "the train produced no groups"))
    })?;
    tx.commit().map_err(sp_store::StoreError::from)?;

    Ok(GenReport {
        dataset_id,
        train_id,
        group_id,
        name,
        groups: spec.groups,
        signals: 0,
        samples: 0,
        pulses: pulses_done,
        swept_property: None,
    })
}

/// Renders one rung. Progress is reported per signal by the caller, which
/// knows how many there are, so the render itself only needs to cancel.
fn render_rung(spec: &GenSpec, sources: &Sources, control: &GenControl) -> Result<SampleBuffer> {
    render::render_with(
        spec,
        SampleRange::first(spec.sample_count()),
        sources,
        &control.cancel_only(),
    )
}

fn signal_name(request: &GenRequest, value: Option<f64>) -> String {
    let base = request.signal_name.trim();
    let base = if base.is_empty() { "Generated" } else { base };
    match value {
        Some(value) => format!("{base} {value}"),
        None => base.to_owned(),
    }
}

/// Reads every signal the rungs reference as a source.
fn resolve_sources(conn: &Connection, rungs: &[(Option<f64>, GenSpec)]) -> Result<Sources> {
    let mut sources = Sources::new();
    for (_, spec) in rungs {
        for id in render::referenced_signals(spec) {
            if sources.contains_key(&id) {
                continue;
            }
            let signal = library::get_signal(conn, id).map_err(|error| GenError::Source {
                id: id.get(),
                reason: error.to_string(),
            })?;
            let Some(sample_rate_hz) = signal.timebase.sample_rate_hz else {
                return Err(GenError::Source {
                    id: id.get(),
                    reason: "an irregularly-sampled signal has no rate to read it at".to_owned(),
                });
            };
            let count = usize::try_from(signal.sample_count).unwrap_or(usize::MAX);
            let buffer = library::read_samples(conn, id, SampleRange::first(count as u64))
                .map_err(|error| GenError::Source {
                    id: id.get(),
                    reason: error.to_string(),
                })?;
            sources.insert(
                id,
                SourceSignal {
                    values: buffer.to_f64(),
                    sample_rate_hz,
                    t0_s: signal.timebase.t0_s,
                },
            );
        }
    }
    Ok(sources)
}

/// Declares the swept value as a signal property, so the library shows it as a
/// typed column rather than as an unrecognised attribute (§6.4).
fn declare_swept_property(conn: &Connection, sweep: &ParamSweep) -> Result<()> {
    if props::get_property_def(conn, PropScope::Signal, &sweep.property_key)?.is_some() {
        return Ok(());
    }
    let def = PropertyDef::new(
        sweep.property_key.clone(),
        PropScope::Signal,
        PropKind::Float {
            min: None,
            max: None,
            step: None,
        },
    )
    .with_label(sweep.field.clone())
    .in_section("Generated");
    match props::insert_property_def(conn, &def) {
        Ok(_) => Ok(()),
        // A pointer like `parts/0/duration_s` does not make a legal property
        // key. The value still reaches the signal's attributes, so the sweep
        // works; it just has no typed column (§6.5).
        Err(sp_store::StoreError::Invalid(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Node;
    use crate::sweep::{ParamSweep, SweepValues};
    use sp_store::{pulses, trains, Store};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        (dir, store)
    }

    fn sine(freq_hz: f64) -> GenSpec {
        GenSpec::new(
            1_000.0,
            0.1,
            Node::Sine {
                freq_hz,
                amp: 1.0,
                phase_rad: 0.0,
                offset: 0.0,
            },
        )
    }

    #[test]
    fn one_spec_becomes_one_dataset_group_and_signal() {
        let (_dir, store) = store();
        let request = GenRequest::new(sine(100.0))
            .named("Tone")
            .with_signal_name("Tone");
        let report = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();

        assert_eq!(report.signals, 1);
        assert_eq!(report.samples, 100);
        assert_eq!(report.name, "Tone");
        assert!(report.swept_property.is_none());

        let group_id = report.group_id;
        let signals = store
            .read(move |conn| library::list_signals(conn, group_id))
            .unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].name, "Tone");
        assert_eq!(signals[0].provenance, Provenance::Generated);
        assert_eq!(signals[0].sample_count, 100);
        assert_eq!(signals[0].attributes.get_i64(SEED_KEY), Some(0));
    }

    #[test]
    fn a_sweep_becomes_one_group_of_signals_carrying_the_swept_value() {
        let (_dir, store) = store();
        let sweep = ParamSweep {
            pointer: crate::tree::ROOT.to_owned(),
            field: "freq_hz".to_owned(),
            property_key: "freq_hz".to_owned(),
            values: SweepValues::Range {
                start: 100.0,
                stop: 400.0,
                step: 100.0,
            },
        };
        let request = GenRequest::new(sine(100.0))
            .named("Sweep")
            .with_signal_name("Tone")
            .with_sweep(sweep);
        let report = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();

        assert_eq!(report.signals, 4);
        assert_eq!(report.swept_property.as_deref(), Some("freq_hz"));

        let group_id = report.group_id;
        let signals = store
            .read(move |conn| library::list_signals(conn, group_id))
            .unwrap();
        let swept: Vec<_> = signals
            .iter()
            .map(|s| s.attributes.get_f64("freq_hz").unwrap())
            .collect();
        assert_eq!(swept, [100.0, 200.0, 300.0, 400.0]);
        assert_eq!(signals[3].name, "Tone 400");

        // The swept value is a declared property, not a loose attribute.
        let defs = store
            .read(|conn| props::list_property_defs(conn, Some(PropScope::Signal)))
            .unwrap();
        assert!(defs.iter().any(|def| def.key == "freq_hz"));
    }

    #[test]
    fn the_spec_is_stored_beside_the_samples_so_they_can_be_regenerated() {
        let (_dir, store) = store();
        let request = GenRequest::new(sine(100.0).with_seed(99));
        let report = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();
        let group_id = report.group_id;
        let signal_id = store
            .read(move |conn| library::list_signals(conn, group_id))
            .unwrap()[0]
            .id;

        let json: String = store
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT gen_spec FROM signal WHERE id = ?1",
                    [signal_id.get()],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        let spec = GenSpec::from_json(&json).unwrap();
        assert_eq!(spec, sine(100.0).with_seed(99));
    }

    #[test]
    fn an_invalid_spec_writes_nothing() {
        let (_dir, store) = store();
        let request = GenRequest::new(sine(900.0));
        let outcome = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap();
        assert!(matches!(outcome, Err(GenError::Invalid(_))));

        let summary = store.read(library::summary).unwrap();
        assert_eq!(summary.datasets, 0);
        assert_eq!(summary.signals, 0);
    }

    #[test]
    fn a_cancelled_generation_rolls_the_whole_batch_back() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let (_dir, store) = store();
        let flag = Arc::new(AtomicBool::new(true));
        let control = GenControl::new().with_cancel(flag);
        let request = GenRequest::new(sine(100.0));
        let outcome = store
            .write(move |conn| Ok(generate(conn, &request, &control)))
            .unwrap();
        assert!(matches!(outcome, Err(GenError::Cancelled)));

        let summary = store.read(library::summary).unwrap();
        assert_eq!(summary.datasets, 0);
        assert_eq!(summary.blobs, 0);
    }

    #[test]
    fn a_generated_signal_can_be_read_back_as_a_source() {
        let (_dir, store) = store();
        let request = GenRequest::new(sine(100.0)).named("Source");
        let report = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();
        let group_id = report.group_id;
        let source_id = store
            .read(move |conn| library::list_signals(conn, group_id))
            .unwrap()[0]
            .id;

        let derived = GenRequest::new(GenSpec::new(
            1_000.0,
            0.1,
            Node::Gain {
                input: Box::new(Node::FromSignal {
                    signal_id: source_id,
                }),
                factor: 2.0,
            },
        ))
        .named("Doubled");
        let report = store
            .write(move |conn| Ok(generate(conn, &derived, &GenControl::new())))
            .unwrap()
            .unwrap();
        assert_eq!(report.signals, 1);

        let group_id = report.group_id;
        let doubled = store
            .read(move |conn| {
                let signals = library::list_signals(conn, group_id)?;
                library::read_samples(conn, signals[0].id, SampleRange::first(100))
            })
            .unwrap();
        let original = render::render_values(
            &sine(100.0),
            SampleRange::first(100),
            &Sources::new(),
            &GenControl::new(),
        )
        .unwrap();
        for (index, value) in doubled.to_f64().iter().enumerate() {
            assert!(
                (value - 2.0 * original[index]).abs() < 1e-5,
                "{index}: {value}"
            );
        }
    }

    #[test]
    fn a_source_that_is_not_in_the_library_is_reported_not_silently_skipped() {
        let (_dir, store) = store();
        let request = GenRequest::new(GenSpec::new(
            1_000.0,
            0.1,
            Node::FromSignal {
                signal_id: sp_core::SignalId::new(404),
            },
        ));
        let outcome = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap();
        assert!(matches!(outcome, Err(GenError::Source { id: 404, .. })));
    }

    #[test]
    fn a_generated_batch_lands_under_a_train_like_an_import_does() {
        let (_dir, store) = store();
        let request = GenRequest::new(sine(100.0)).named("Tone");
        let report = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();

        let dataset_id = report.dataset_id;
        let (trains, groups) = store
            .read(move |conn| {
                let trains = trains::list_trains(conn, dataset_id)?;
                let groups = library::list_groups(conn, report.train_id)?;
                Ok((trains, groups))
            })
            .unwrap();
        assert_eq!(trains.len(), 1);
        assert_eq!(trains[0].id, report.train_id);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, report.group_id);
        assert_eq!(groups[0].train_id, report.train_id);
    }

    fn train_spec() -> TrainSpec {
        TrainSpec {
            groups: 3,
            pulses_per_group: 16,
            ..TrainSpec::default()
        }
    }

    #[test]
    fn a_generated_train_is_the_same_shape_as_an_imported_one() {
        let (_dir, store) = store();
        let spec = train_spec();
        let request = TrainRequest::new(spec.clone()).named("Emitter A");
        let report = store
            .write(move |conn| Ok(generate_train(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();

        assert_eq!(report.groups, 3);
        assert_eq!(report.pulses, 48);
        assert_eq!(report.signals, 0);
        assert!(report.summary().contains("48 pulses"));

        let train_id = report.train_id;
        let (train, groups, fields, toa) = store
            .read(move |conn| {
                let train = trains::get_train(conn, train_id)?;
                let groups = library::list_groups(conn, train_id)?;
                let fields = pulses::list_fields(conn, groups[1].id)?;
                let toa = pulses::read_toa(conn, groups[1].id, SampleRange::first(16))?;
                Ok((train, groups, fields, toa))
            })
            .unwrap();

        // The train, not the group, is what says these are pulse records.
        assert!(train.is_pulse_train());
        assert_eq!(train.toa_unit, Some(sp_core::TimeUnit::Microseconds));
        assert_eq!(train.display_name(), "Emitter A");

        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|g| g.is_pulse_group()));
        assert!(groups.iter().all(|g| g.actual_count == 16));

        // One column per field, keyed the way an import keys them.
        assert_eq!(
            fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
            ["pulse_width", "power", "angle"]
        );
        assert_eq!(fields[0].unit.as_deref(), Some("us"));

        // The second group continues the first rather than restarting it.
        assert_eq!(toa.len(), 16);
        assert!((toa[0] - spec.toa_seconds(1)[0]).abs() < 1e-12);
        assert!(toa[0] > 0.015, "group 1 starts after group 0: {}", toa[0]);
    }

    #[test]
    fn a_generated_train_carries_the_spec_that_made_it() {
        let (_dir, store) = store();
        let spec = TrainSpec {
            seed: 42,
            ..train_spec()
        };
        let request = TrainRequest::new(spec.clone());
        let report = store
            .write(move |conn| Ok(generate_train(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();

        let train_id = report.train_id;
        let train = store
            .read(move |conn| trains::get_train(conn, train_id))
            .unwrap();
        assert_eq!(train.attributes.get_i64(SEED_KEY), Some(42));
        let stored: TrainSpec =
            serde_json::from_value(train.attributes.get(TRAIN_SPEC_KEY).cloned().unwrap()).unwrap();
        assert_eq!(stored, spec);
    }

    #[test]
    fn the_pulses_a_train_writes_are_the_ones_the_spec_renders() {
        let (_dir, store) = store();
        let spec = train_spec();
        let request = TrainRequest::new(spec.clone());
        let report = store
            .write(move |conn| Ok(generate_train(conn, &request, &GenControl::new())))
            .unwrap()
            .unwrap();

        let train_id = report.train_id;
        let stored = store
            .read(move |conn| {
                let groups = library::list_groups(conn, train_id)?;
                pulses::read_field(conn, groups[2].id, 2, SampleRange::first(16))
            })
            .unwrap();
        // `angle` is an f32 column, so compare in the stored precision.
        for (index, value) in spec.field_values(2, 2).iter().enumerate() {
            let read = stored.value(index as u64).unwrap();
            assert!((read - value).abs() < 1e-3, "{index}: {read} vs {value}");
        }
    }

    #[test]
    fn an_invalid_train_writes_nothing() {
        let (_dir, store) = store();
        let request = TrainRequest::new(TrainSpec {
            fields: Vec::new(),
            ..train_spec()
        });
        let outcome = store
            .write(move |conn| Ok(generate_train(conn, &request, &GenControl::new())))
            .unwrap();
        assert!(matches!(outcome, Err(GenError::Invalid(_))));
        assert_eq!(store.read(library::summary).unwrap().datasets, 0);
    }

    #[test]
    fn a_cancelled_train_rolls_the_whole_batch_back() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let (_dir, store) = store();
        let control = GenControl::new().with_cancel(Arc::new(AtomicBool::new(true)));
        let request = TrainRequest::new(train_spec());
        let outcome = store
            .write(move |conn| Ok(generate_train(conn, &request, &control)))
            .unwrap();
        assert!(matches!(outcome, Err(GenError::Cancelled)));

        let summary = store.read(library::summary).unwrap();
        assert_eq!(summary.datasets, 0);
        assert_eq!(summary.blobs, 0);
    }

    #[test]
    fn a_sweep_that_cannot_expand_is_reported_before_anything_is_written() {
        let (_dir, store) = store();
        let request = GenRequest::new(sine(100.0)).with_sweep(ParamSweep {
            pointer: crate::tree::ROOT.to_owned(),
            field: "freq_hz".to_owned(),
            property_key: "freq_hz".to_owned(),
            values: SweepValues::Range {
                start: 0.0,
                stop: 1.0,
                step: 0.0,
            },
        });
        let outcome = store
            .write(move |conn| Ok(generate(conn, &request, &GenControl::new())))
            .unwrap();
        assert!(matches!(outcome, Err(GenError::Invalid(_))));
        assert_eq!(store.read(library::summary).unwrap().datasets, 0);
    }
}
