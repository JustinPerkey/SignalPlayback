//! Content-hash keyed reuse of stage output (`docs/DESIGN.md` §9.5).
//!
//! The key folds four things: the stage's kind, its version, its resolved
//! parameters, and the content hashes of everything it will read. Change any
//! of them and the key changes; change nothing and it does not. That is what
//! makes editing stage 4's parameters re-run stages 4…n and no more — on a
//! long pipeline, the difference between iterating on an algorithm and waiting
//! on one.
//!
//! The entries themselves live in the library, hung off the run that recorded
//! them, so a deleted run takes its cache with it and a key never points at
//! rows that are gone.

use crate::frame::GroupFrame;

/// The hash of everything a stage is about to read: its signals' samples and
/// the artifacts on its inbound ports.
///
/// Names and ordering are folded in as well as content, because a stage that
/// addresses its inputs by name would behave differently if they were renamed.
#[must_use]
pub fn input_hash(frame: &GroupFrame) -> String {
    let mut hasher = blake3::Hasher::new();
    for signal in &frame.signals {
        hasher.update(signal.name().as_bytes());
        hasher.update(b"\x1f");
        hasher.update(signal.content_hash().as_bytes());
        hasher.update(b"\x1e");
    }
    hasher.update(b"\x1d");
    for value in frame.inbound.iter() {
        hasher.update(value.port.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(value.kind.as_bytes());
        hasher.update(&value.kind_version.to_le_bytes());
        hasher.update(value.payload_json.as_bytes());
        hasher.update(b"\x1e");
    }
    hasher.finalize().to_hex().to_string()
}

/// The cache key for one stage over one frame.
///
/// The group is deliberately *not* part of the key: two groups whose signals
/// hash the same genuinely produce the same output, and reusing it is the
/// point of content addressing.
///
/// `salt` is whatever identity the stage instance adds beyond its declaration
/// — for a compiled stage nothing, for an external one the hash of the
/// library file behind it (`Stage::cache_salt`, §9.9).
#[must_use]
pub fn stage_key(
    kind: &str,
    version: u32,
    params_json: &str,
    salt: Option<&str>,
    input_hash: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    hasher.update(&version.to_le_bytes());
    hasher.update(params_json.as_bytes());
    hasher.update(salt.unwrap_or_default().as_bytes());
    hasher.update(input_hash.as_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::{
        Attributes, DType, Domain, GroupId, GroupMeta, RunId, SampleBuffer, Timebase, TrainId,
    };

    use crate::frame::{PortValue, SignalRef};

    fn frame(values: &[f64]) -> GroupFrame {
        GroupFrame::new(
            GroupMeta {
                id: GroupId::new(1),
                train_id: TrainId::new(1),
                ordinal: 0,
                name: None,
                toa_unit: None,
                attributes: Attributes::new(),
            },
            vec![SignalRef::in_memory(
                "a",
                Domain::Analog,
                Timebase::regular(1000.0, 0.0),
                SampleBuffer::from_f64(DType::F64, values),
                Attributes::new(),
            )],
            RunId::new(1),
        )
    }

    fn spectrum(payload: &str) -> PortValue {
        PortValue {
            port: "spectrum".into(),
            kind: "spectrum.v1".into(),
            kind_version: 1,
            payload_json: payload.into(),
            summary: None,
            stage_ordinal: 0,
        }
    }

    #[test]
    fn the_same_inputs_hash_the_same() {
        assert_eq!(
            input_hash(&frame(&[1.0, 2.0])),
            input_hash(&frame(&[1.0, 2.0]))
        );
    }

    #[test]
    fn different_samples_change_the_input_hash() {
        assert_ne!(
            input_hash(&frame(&[1.0, 2.0])),
            input_hash(&frame(&[1.0, 3.0]))
        );
    }

    #[test]
    fn an_inbound_artifact_is_part_of_the_input() {
        // Stage 4 may read stage 3's spectrum; re-running stage 3 with new
        // parameters has to invalidate stage 4 (§9.5).
        let mut with_artifact = frame(&[1.0, 2.0]);
        with_artifact.inbound.publish(spectrum(r#"{"peak":3.0}"#));
        let mut changed = frame(&[1.0, 2.0]);
        changed.inbound.publish(spectrum(r#"{"peak":4.0}"#));

        assert_ne!(input_hash(&with_artifact), input_hash(&frame(&[1.0, 2.0])));
        assert_ne!(input_hash(&with_artifact), input_hash(&changed));
    }

    #[test]
    fn renaming_a_signal_changes_the_input_hash() {
        let mut renamed = frame(&[1.0, 2.0]);
        renamed.rename(0, "b").unwrap();
        assert_ne!(input_hash(&renamed), input_hash(&frame(&[1.0, 2.0])));
    }

    #[test]
    fn the_stage_version_is_part_of_the_key() {
        // A bumped version means new behaviour, so the old output must not be
        // reused (§9.2).
        let inputs = input_hash(&frame(&[1.0]));
        assert_ne!(
            stage_key("dsp.gain", 1, "{}", None, &inputs),
            stage_key("dsp.gain", 2, "{}", None, &inputs)
        );
    }

    #[test]
    fn parameters_are_part_of_the_key() {
        let inputs = input_hash(&frame(&[1.0]));
        assert_ne!(
            stage_key("dsp.gain", 1, r#"{"gain":2.0}"#, None, &inputs),
            stage_key("dsp.gain", 1, r#"{"gain":3.0}"#, None, &inputs)
        );
        assert_eq!(
            stage_key("dsp.gain", 1, r#"{"gain":2.0}"#, None, &inputs),
            stage_key("dsp.gain", 1, r#"{"gain":2.0}"#, None, &inputs)
        );
    }

    #[test]
    fn a_rebuilt_external_library_gets_a_different_key() {
        // The stage kind, version and parameters are all unchanged when a DLL
        // is recompiled; only the file hash moves (§9.9).
        let inputs = input_hash(&frame(&[1.0]));
        assert_ne!(
            stage_key("ext.vendor.eq", 1, "{}", Some("aaaa"), &inputs),
            stage_key("ext.vendor.eq", 1, "{}", Some("bbbb"), &inputs)
        );
        assert_eq!(
            stage_key("ext.vendor.eq", 1, "{}", Some("aaaa"), &inputs),
            stage_key("ext.vendor.eq", 1, "{}", Some("aaaa"), &inputs)
        );
    }

    #[test]
    fn two_groups_with_identical_signals_share_a_key() {
        // Content addressing, not group identity: the same samples through the
        // same stage are the same result.
        let mut other_group = frame(&[1.0, 2.0]);
        other_group.group.id = GroupId::new(99);
        other_group.group.ordinal = 7;
        assert_eq!(input_hash(&other_group), input_hash(&frame(&[1.0, 2.0])));
    }
}
