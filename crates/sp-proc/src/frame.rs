//! What flows between stages: the [`GroupFrame`] (`docs/DESIGN.md` §9.3).
//!
//! A frame is one group's metadata plus lazy handles to its signals and the
//! artifacts upstream stages published. Signals are handles rather than
//! buffers on purpose: a group may hold a hundred million samples, and most
//! stages read them a span at a time. A handle that points at a stored column
//! reads it back through the blob store; one produced by a stage this run
//! holds the buffer until it is recorded.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use sp_core::time::SampleRange;
use sp_core::{
    Attributes, Disposition, Domain, GroupMeta, RunId, SampleBuffer, Signal, SignalStats, Timebase,
};
use sp_store::blob::{self, BlobId};
use sp_store::runs::RunSignalRow;
use sp_store::Store;

use crate::error::StageError;
use crate::stage::{SignalOut, StageOutput};

/// Where a [`SignalRef`]'s samples live.
#[derive(Debug, Clone)]
enum Source {
    /// A column in the library or in a run's records, read by span.
    Stored { store: Store, blob: BlobId },
    /// Produced by a stage in this run and not yet recorded.
    Memory(Arc<SampleBuffer>),
}

/// A lazy handle to one signal of a frame.
///
/// The content hash is what the cache key folds in (§9.5), so it must address
/// the samples and nothing else: for a stored column it is the blob's
/// checksum, which the store already computed.
#[derive(Debug, Clone)]
pub struct SignalRef {
    name: String,
    domain: Domain,
    timebase: Timebase,
    sample_count: u64,
    attributes: Attributes,
    stats: SignalStats,
    content_hash: String,
    source: Source,
}

impl SignalRef {
    /// A handle to a signal in the library.
    pub fn from_library(store: &Store, signal: &Signal, blob: BlobId) -> Result<Self, StageError> {
        let checksum = store.read(|conn| blob::info(conn, blob))?.checksum;
        Ok(Self {
            name: signal.name.clone(),
            domain: signal.domain,
            timebase: signal.timebase,
            sample_count: signal.sample_count,
            attributes: signal.attributes.clone(),
            stats: signal.stats.unwrap_or_default(),
            content_hash: checksum,
            source: Source::Stored {
                store: store.clone(),
                blob,
            },
        })
    }

    /// A handle to a signal recorded at the output of some stage — what a
    /// cache hit hands the next stage.
    pub fn from_recorded(store: &Store, row: &RunSignalRow) -> Result<Self, StageError> {
        let blob = row.blob_id.ok_or_else(|| {
            StageError::rejected(format!("'{}' was recorded without samples", row.name))
        })?;
        let checksum = store.read(|conn| blob::info(conn, blob))?.checksum;
        Ok(Self {
            name: row.name.clone(),
            domain: row.domain,
            timebase: row.timebase,
            sample_count: row.sample_count,
            attributes: row.attributes.clone(),
            stats: row.stats,
            content_hash: checksum,
            source: Source::Stored {
                store: store.clone(),
                blob,
            },
        })
    }

    /// A signal a stage just produced.
    #[must_use]
    pub fn in_memory(
        name: impl Into<String>,
        domain: Domain,
        timebase: Timebase,
        samples: SampleBuffer,
        attributes: Attributes,
    ) -> Self {
        let stats = sp_core::stats::summarise(&samples);
        Self {
            name: name.into(),
            domain,
            timebase,
            sample_count: samples.len() as u64,
            attributes,
            stats,
            content_hash: hash_samples(&samples),
            source: Source::Memory(Arc::new(samples)),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn domain(&self) -> Domain {
        self.domain
    }

    #[must_use]
    pub fn timebase(&self) -> Timebase {
        self.timebase
    }

    #[must_use]
    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sample_count == 0
    }

    #[must_use]
    pub fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    #[must_use]
    pub fn stats(&self) -> &SignalStats {
        &self.stats
    }

    /// The address of these samples: a blake3 hex digest.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// The blob backing this signal, for a handle that points at stored
    /// samples. `None` for one a stage produced.
    #[must_use]
    pub fn blob_id(&self) -> Option<BlobId> {
        match &self.source {
            Source::Stored { blob, .. } => Some(*blob),
            Source::Memory(_) => None,
        }
    }

    /// The buffer a stage produced, for the recorder to write.
    #[must_use]
    pub fn buffer(&self) -> Option<&SampleBuffer> {
        match &self.source {
            Source::Memory(buffer) => Some(buffer),
            Source::Stored { .. } => None,
        }
    }

    /// Reads `range` of the samples, clamped to the signal.
    pub fn read(&self, range: SampleRange) -> Result<SampleBuffer, StageError> {
        match &self.source {
            Source::Stored { store, blob } => {
                Ok(store.read(|conn| blob::read_column(conn, *blob, range))?)
            }
            Source::Memory(buffer) => {
                let start = usize::try_from(range.start.min(self.sample_count)).unwrap_or(0);
                let end = usize::try_from(range.end.min(self.sample_count)).unwrap_or(0);
                let values: Vec<f64> = (start..end)
                    .filter_map(|i| buffer.value(i as u64))
                    .collect();
                Ok(SampleBuffer::from_f64(buffer.dtype(), &values))
            }
        }
    }

    /// Reads every sample. Convenient, and the right thing for the group
    /// sizes a stage usually sees; a stage over a very long signal should read
    /// by span instead.
    pub fn read_all(&self) -> Result<SampleBuffer, StageError> {
        match &self.source {
            Source::Memory(buffer) => Ok((**buffer).clone()),
            Source::Stored { .. } => self.read(SampleRange::first(self.sample_count)),
        }
    }

    /// Every sample as an `f64`, which is how a stage does its arithmetic
    /// (§17.10).
    pub fn read_values(&self) -> Result<Vec<f64>, StageError> {
        Ok(self.read_all()?.values().collect())
    }

    fn renamed(&self, name: impl Into<String>) -> Self {
        let mut next = self.clone();
        next.name = name.into();
        next
    }
}

/// blake3 over the dtype and the packed little-endian samples: the same
/// address the blob store would give these bytes.
fn hash_samples(samples: &SampleBuffer) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[samples.dtype().code()]);
    const PIECE: usize = 1 << 16;
    let len = samples.len();
    let mut at = 0;
    while at < len {
        let end = (at + PIECE).min(len);
        hasher.update(&blob::encode_samples(samples.samples(), at..end));
        at = end;
    }
    hasher.finalize().to_hex().to_string()
}

/// One artifact on a port, as the next stage sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortValue {
    pub port: String,
    pub kind: String,
    pub kind_version: u32,
    pub payload_json: String,
    pub summary: Option<String>,
    /// Which stage published it, so "nearest upstream producer" is decidable.
    pub stage_ordinal: i32,
}

impl PortValue {
    /// Deserialises the payload into the artifact type it was published as.
    pub fn read<A: DeserializeOwned>(&self) -> Result<A, StageError> {
        Ok(serde_json::from_str(&self.payload_json)?)
    }
}

/// The frame-scoped, typed blackboard of §9.3.
///
/// Publishing to a port name that is already taken replaces it, which is what
/// makes a required input resolve to the *nearest* upstream producer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortMap(BTreeMap<String, PortValue>);

impl PortMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&mut self, value: PortValue) {
        self.0.insert(value.port.clone(), value);
    }

    #[must_use]
    pub fn get(&self, port: &str) -> Option<&PortValue> {
        self.0.get(port)
    }

    /// The most recent artifact of a kind, whatever port it was published on.
    /// A stage that wants "the spectrum" rather than "port `spectrum`" reads
    /// it this way.
    #[must_use]
    pub fn latest_of_kind(&self, kind: &str) -> Option<&PortValue> {
        self.0
            .values()
            .filter(|value| value.kind == kind)
            .max_by_key(|value| value.stage_ordinal)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &PortValue> {
        self.0.values()
    }
}

/// One group as it enters a stage (§9.3).
#[derive(Debug, Clone)]
pub struct GroupFrame {
    /// Name, properties and position — and the train the group belongs to, for
    /// a stage that needs the rest of the capture.
    pub group: GroupMeta,
    /// Lazy column handles; read by span, never copied whole.
    pub signals: Vec<SignalRef>,
    /// Artifacts published by upstream stages.
    pub inbound: PortMap,
    pub run: RunId,
}

impl GroupFrame {
    #[must_use]
    pub fn new(group: GroupMeta, signals: Vec<SignalRef>, run: RunId) -> Self {
        Self {
            group,
            signals,
            inbound: PortMap::new(),
            run,
        }
    }

    /// The signal at a position, or a `NoSuchSignal` error naming the group's
    /// actual count.
    pub fn signal(&self, ordinal: usize) -> Result<&SignalRef, StageError> {
        self.signals
            .get(ordinal)
            .ok_or_else(|| StageError::NoSuchSignal {
                ordinal,
                count: self.signals.len(),
            })
    }

    /// The first signal whose name matches, for a stage that addresses one by
    /// name rather than position.
    #[must_use]
    pub fn signal_named(&self, name: &str) -> Option<&SignalRef> {
        self.signals.iter().find(|s| s.name() == name)
    }

    /// The frame the next stage sees, given what this one produced (§9.5).
    ///
    /// Replacements keep their slot, additions go on the end, drops are gone.
    /// Ordering is by input position, so a stage cannot silently shuffle a
    /// group's signals by the order it happened to emit them in.
    pub fn apply(&self, output: &StageOutput, stage_ordinal: i32) -> Result<Self, StageError> {
        output.check_covers(self)?;

        let mut kept: Vec<(usize, SignalRef)> = Vec::new();
        let mut added: Vec<SignalRef> = Vec::new();

        for out in &output.signals {
            match out {
                SignalOut::Passthrough { ordinal } => {
                    kept.push((*ordinal, self.signal(*ordinal)?.clone()));
                }
                SignalOut::Drop { .. } => {}
                SignalOut::Replace {
                    ordinal,
                    samples,
                    patch,
                } => {
                    let input = self.signal(*ordinal)?;
                    let mut attributes = input.attributes().clone();
                    patch.apply_to(&mut attributes);
                    kept.push((
                        *ordinal,
                        SignalRef::in_memory(
                            input.name(),
                            input.domain(),
                            input.timebase(),
                            samples.clone(),
                            attributes,
                        ),
                    ));
                }
                SignalOut::Add {
                    name,
                    domain,
                    samples,
                    attrs,
                } => {
                    // A new signal shares the group's timebase; a stage that
                    // resamples says so by replacing instead.
                    let timebase = self
                        .signals
                        .first()
                        .map_or(Timebase::irregular(0.0), SignalRef::timebase);
                    added.push(SignalRef::in_memory(
                        name.clone(),
                        *domain,
                        timebase,
                        samples.clone(),
                        attrs.clone(),
                    ));
                }
            }
        }

        kept.sort_by_key(|(ordinal, _)| *ordinal);
        let mut signals: Vec<SignalRef> = kept.into_iter().map(|(_, signal)| signal).collect();
        signals.extend(added);

        let mut inbound = self.inbound.clone();
        for artifact in &output.artifacts {
            inbound.publish(PortValue {
                port: artifact.port.clone(),
                kind: artifact.kind.clone(),
                kind_version: artifact.kind_version,
                payload_json: artifact.payload_json.clone(),
                summary: artifact.summary.clone(),
                stage_ordinal,
            });
        }

        let mut group = self.group.clone();
        for patch in &output.properties {
            if matches!(patch.target, crate::stage::PatchTarget::Group) {
                group
                    .attributes
                    .insert(patch.key.clone(), patch.value.clone());
            }
        }
        for patch in &output.properties {
            if let crate::stage::PatchTarget::Signal { ordinal } = patch.target {
                if let Some(signal) = signals.get_mut(ordinal) {
                    signal
                        .attributes
                        .insert(patch.key.clone(), patch.value.clone());
                }
            }
        }

        Ok(Self {
            group,
            signals,
            inbound,
            run: self.run,
        })
    }

    /// What happened to each signal of the frame [`GroupFrame::apply`]
    /// returns, in the same order.
    ///
    /// It lives beside `apply` because it encodes the same ordering rule —
    /// surviving signals by input position, then additions in the order they
    /// were emitted — and the two must never drift apart.
    #[must_use]
    pub fn dispositions(output: &StageOutput) -> Vec<Disposition> {
        let mut kept: Vec<(usize, Disposition)> = output
            .signals
            .iter()
            .filter_map(|out| match out {
                SignalOut::Passthrough { ordinal } => Some((*ordinal, Disposition::Passthrough)),
                SignalOut::Replace { ordinal, .. } => Some((*ordinal, Disposition::Replaced)),
                SignalOut::Add { .. } | SignalOut::Drop { .. } => None,
            })
            .collect();
        kept.sort_by_key(|(ordinal, _)| *ordinal);

        let mut out: Vec<Disposition> = kept.into_iter().map(|(_, kind)| kind).collect();
        out.extend(
            output
                .signals
                .iter()
                .filter(|s| matches!(s, SignalOut::Add { .. }))
                .map(|_| Disposition::Added),
        );
        out
    }

    /// Renames a signal in place, for a stage that relabels rather than
    /// rewrites.
    pub fn rename(&mut self, ordinal: usize, name: impl Into<String>) -> Result<(), StageError> {
        let renamed = self.signal(ordinal)?.renamed(name);
        self.signals[ordinal] = renamed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::{DType, GroupId, TrainId};

    use crate::stage::{AttrPatch, PropertyPatch, SignalOut};

    fn meta() -> GroupMeta {
        GroupMeta {
            id: GroupId::new(1),
            train_id: TrainId::new(1),
            ordinal: 0,
            name: Some("dwell 1".into()),
            toa_unit: None,
            attributes: Attributes::new(),
        }
    }

    fn signal(name: &str, values: &[f64]) -> SignalRef {
        SignalRef::in_memory(
            name,
            Domain::Analog,
            Timebase::regular(1000.0, 0.0),
            SampleBuffer::from_f64(DType::F64, values),
            Attributes::new(),
        )
    }

    fn frame() -> GroupFrame {
        GroupFrame::new(
            meta(),
            vec![signal("a", &[1.0, 2.0]), signal("b", &[3.0, 4.0])],
            RunId::new(7),
        )
    }

    fn buffer(values: &[f64]) -> SampleBuffer {
        SampleBuffer::from_f64(DType::F64, values)
    }

    #[test]
    fn identical_samples_hash_identically_and_different_ones_do_not() {
        assert_eq!(
            signal("a", &[1.0, 2.0]).content_hash(),
            signal("differently named", &[1.0, 2.0]).content_hash(),
            "the hash addresses the samples, not the name"
        );
        assert_ne!(
            signal("a", &[1.0, 2.0]).content_hash(),
            signal("a", &[1.0, 2.5]).content_hash()
        );
    }

    #[test]
    fn a_replacement_keeps_the_slot_and_an_addition_goes_on_the_end() {
        let frame = frame();
        let output = StageOutput::passthrough_of(&frame)
            .with_signal(SignalOut::add("envelope", buffer(&[9.0, 9.0])));
        let mut output = output;
        output.set_signal(0, SignalOut::replace(0, buffer(&[10.0, 20.0])));

        let next = frame.apply(&output, 0).unwrap();
        assert_eq!(next.signals.len(), 3);
        assert_eq!(next.signals[0].name(), "a");
        assert_eq!(next.signals[0].read_values().unwrap(), [10.0, 20.0]);
        assert_eq!(next.signals[1].name(), "b");
        assert_eq!(next.signals[2].name(), "envelope");
    }

    #[test]
    fn a_dropped_signal_is_gone_from_the_next_frame() {
        let frame = frame();
        let mut output = StageOutput::passthrough_of(&frame);
        output.set_signal(0, SignalOut::Drop { ordinal: 0 });
        let next = frame.apply(&output, 0).unwrap();
        assert_eq!(next.signals.len(), 1);
        assert_eq!(next.signals[0].name(), "b");
    }

    #[test]
    fn a_stage_that_forgets_a_signal_is_refused() {
        // Passthrough is explicit so that this is an error rather than a
        // silent guess (§9.4).
        let frame = frame();
        let output = StageOutput::new().with_signal(SignalOut::Passthrough { ordinal: 0 });
        let err = frame.apply(&output, 0).unwrap_err();
        assert!(
            matches!(err, StageError::UnhandledSignal { ordinal: 1, ref name } if name == "b"),
            "{err}"
        );
    }

    #[test]
    fn accounting_for_a_signal_twice_is_refused_too() {
        let frame = frame();
        let mut output = StageOutput::passthrough_of(&frame);
        output.signals.push(SignalOut::Drop { ordinal: 1 });
        let err = frame.apply(&output, 0).unwrap_err();
        assert!(
            matches!(err, StageError::UnhandledSignal { ordinal: 1, .. }),
            "{err}"
        );
    }

    #[test]
    fn addressing_a_signal_the_group_does_not_have_names_the_count() {
        let frame = frame();
        let mut output = StageOutput::passthrough_of(&frame);
        output.signals.push(SignalOut::Passthrough { ordinal: 5 });
        let err = frame.apply(&output, 0).unwrap_err();
        assert!(
            matches!(
                err,
                StageError::NoSuchSignal {
                    ordinal: 5,
                    count: 2
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn an_attribute_patch_rides_along_with_the_replacement() {
        let frame = frame();
        let mut output = StageOutput::passthrough_of(&frame);
        output.set_signal(
            0,
            SignalOut::Replace {
                ordinal: 0,
                samples: buffer(&[0.5, 1.0]),
                patch: AttrPatch::new().set("filtered", true),
            },
        );
        let next = frame.apply(&output, 0).unwrap();
        assert_eq!(
            next.signals[0].attributes().get_bool("filtered"),
            Some(true)
        );
    }

    #[test]
    fn property_patches_write_back_onto_the_group_and_its_signals() {
        let frame = frame();
        let mut output = StageOutput::passthrough_of(&frame);
        output.properties.push(PropertyPatch::group("snr_db", 12.5));
        output
            .properties
            .push(PropertyPatch::signal(1, "role", "reference"));

        let next = frame.apply(&output, 0).unwrap();
        assert_eq!(next.group.attributes.get_f64("snr_db"), Some(12.5));
        assert_eq!(
            next.signals[1].attributes().get_str("role"),
            Some("reference")
        );
    }

    #[test]
    fn a_port_resolves_to_the_nearest_upstream_producer() {
        let mut ports = PortMap::new();
        for stage_ordinal in [0, 3] {
            ports.publish(PortValue {
                port: "spectrum".into(),
                kind: "spectrum.v1".into(),
                kind_version: 1,
                payload_json: format!("{{\"from\":{stage_ordinal}}}"),
                summary: None,
                stage_ordinal,
            });
        }
        assert_eq!(ports.len(), 1);
        assert_eq!(ports.get("spectrum").unwrap().stage_ordinal, 3);
        assert_eq!(
            ports.latest_of_kind("spectrum.v1").unwrap().stage_ordinal,
            3
        );
        assert!(ports.latest_of_kind("detections.v1").is_none());
    }

    #[test]
    fn artifacts_a_stage_publishes_reach_the_next_frame() {
        let frame = frame();
        let mut output = StageOutput::passthrough_of(&frame);
        output.artifacts.push(crate::stage::ArtifactOut {
            port: "stats".into(),
            kind: "stats.v1".into(),
            kind_version: 1,
            payload_json: r#"{"rms":2.0}"#.into(),
            summary: Some("rms 2.0".into()),
        });
        let next = frame.apply(&output, 2).unwrap();
        let value = next.inbound.get("stats").unwrap();
        assert_eq!(value.stage_ordinal, 2);
        assert_eq!(value.summary.as_deref(), Some("rms 2.0"));
    }

    #[test]
    fn an_in_memory_signal_reads_back_by_span() {
        let signal = signal("a", &[0.0, 1.0, 2.0, 3.0]);
        let span = signal.read(SampleRange::new(1, 3)).unwrap();
        assert_eq!(span.values().collect::<Vec<_>>(), [1.0, 2.0]);
        assert_eq!(signal.stats().max(), Some(3.0));
    }
}
