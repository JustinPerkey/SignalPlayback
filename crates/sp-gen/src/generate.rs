//! Generation into the library (`docs/DESIGN.md` §8.3, §8.4).
//!
//! One action produces one dataset holding one group: a single signal, or one
//! signal per rung of a sweep with the swept value stored as a property on
//! each. That is the shape a pipeline wants as test input (§8.3) — a run can
//! group by the property and the degradation curve falls out of one run.
//!
//! The whole batch is one SQLite transaction, so a failure or a cancellation
//! rolls back the rows *and* the blobs, exactly as import does (§14).

use sp_core::group::DatasetId;
use sp_core::props::PropertyValue;
use sp_core::{
    Attributes, GroupId, PropKind, PropScope, PropertyDef, Provenance, SampleBuffer, SampleRange,
    SourceKind,
};
use sp_store::{library, props, Connection, NewDataset, NewGroup, NewSignal};

use crate::control::{GenControl, GenProgress};
use crate::error::{GenError, Result};
use crate::render::{self, SourceSignal, Sources};
use crate::spec::GenSpec;
use crate::sweep::{self, ParamSweep};
use crate::validate::{self, validate};

/// The attribute every generated signal carries, naming the seed that produced
/// it. The spec itself lives in `signal.gen_spec`.
pub const SEED_KEY: &str = "gen_seed";

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
    pub group_id: GroupId,
    pub name: String,
    pub signals: u32,
    pub samples: u64,
    /// The property the swept value was stored under, when there was a sweep.
    pub swept_property: Option<String>,
}

impl GenReport {
    /// A one-line summary for the status bar.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut summary = format!(
            "{}: {} signal{}, {} sample{}",
            self.name,
            self.signals,
            if self.signals == 1 { "" } else { "s" },
            self.samples,
            if self.samples == 1 { "" } else { "s" },
        );
        if let Some(key) = &self.swept_property {
            summary.push_str(&format!(", swept by {key}"));
        }
        summary
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

    let mut group_attributes = Attributes::new();
    if let Some(sweep) = &request.sweep {
        group_attributes.insert("gen_sweep_parameter", sweep.json_pointer());
        group_attributes.insert("gen_sweep_property", sweep.property_key.clone());
    }
    let group_id = library::insert_group(
        &tx,
        &NewGroup {
            dataset_id,
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
            signals_done: ordinal as u32 + 1,
            signals_total,
            samples_done,
            samples_total,
        });
    }

    control.check()?;
    tx.commit().map_err(sp_store::StoreError::from)?;

    Ok(GenReport {
        dataset_id,
        group_id,
        name,
        signals: signals_total,
        samples: samples_done,
        swept_property: request.sweep.as_ref().map(|s| s.property_key.clone()),
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
    use sp_store::Store;

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
