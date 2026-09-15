//! Every built-in stage, through the contract every stage is held to
//! (`docs/DESIGN.md` §14, §16.1 M11).
//!
//! The harness lives in `sp-proc` and knows nothing about DSP, so what it
//! asks of a biquad is exactly what it would ask of a stage a user wrote:
//! a coherent descriptor, parameters that are validated, every input signal
//! accounted for, the same answer twice, a cancellation noticed, and a
//! diagnostic rather than a panic on a group with nothing in it.

use sp_core::artifact::Artifact;
use sp_dsp::{Detections, Symbols};
use sp_proc::conform::{Check, Conformance};
use sp_proc::registry::StageFactory;
use sp_proc::{ParamSet, PortValue};

/// Parameters each built-in needs; the rest run on their declared defaults.
fn params(kind: &str) -> ParamSet {
    match kind {
        "dsp.condition.gain" => ParamSet::new().with("gain", 2.0),
        "dsp.filter.biquad" => ParamSet::new().with("cutoff_hz", 100.0),
        "dsp.detect.threshold" => ParamSet::new().with("level", 0.5),
        "dsp.digital.symbols" => ParamSet::new().with("symbol_rate_hz", 50.0),
        _ => ParamSet::new(),
    }
}

/// An artifact on a port, as an upstream stage would have published it.
fn published<A: Artifact>(port: &str, value: &A) -> PortValue {
    PortValue {
        port: port.to_owned(),
        kind: A::KIND.to_owned(),
        kind_version: A::VERSION,
        payload_json: serde_json::to_string(value).unwrap(),
        summary: Some(value.summary()),
        stage_ordinal: 0,
    }
}

/// What a stage that reads an artifact needs on its inbound ports (§9.3).
fn inbound(kind: &str) -> Vec<PortValue> {
    match kind {
        "dsp.digital.bits" => {
            let mut symbols = Symbols::new("rf", 2);
            for (index, symbol) in [1i64, 0, 1, 1, 0, 0, 1, 0, 1].into_iter().enumerate() {
                let level = symbol as f64 * 2.0 - 1.0;
                symbols.push(index as f64 * 0.02, level, symbol, 1.0);
            }
            vec![published("symbols", &symbols)]
        }
        "dsp.measure.pulse" => {
            let mut detections = Detections::default();
            for pulse in 0..4 {
                let start = f64::from(pulse) * 0.1;
                detections.push("rf", (start, start + 0.02), 1.0);
            }
            vec![published("detections", &detections)]
        }
        _ => Vec::new(),
    }
}

fn conformance(factory: StageFactory) -> Conformance {
    let kind = factory().descriptor().kind;
    inbound(kind)
        .into_iter()
        .fold(Conformance::of(factory), Conformance::with_inbound)
        .with_params(params(kind))
}

#[test]
fn every_builtin_conforms() {
    // Every stage is reported, not just the first that fails: one run of the
    // harness should say everything that is wrong with the crate.
    let failures: Vec<String> = sp_dsp::builtins()
        .into_iter()
        .map(|factory| conformance(factory).run())
        .filter(|report| !report.is_pass())
        .map(|report| report.describe())
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_harness_covers_every_check_for_every_builtin() {
    // A milestone's worth of value is in what is *not* waived: a report that
    // skipped half its checks would pass just as loudly.
    for factory in sp_dsp::builtins() {
        let report = conformance(factory).run();
        assert!(
            report.skipped().is_empty(),
            "{} waived {:?}",
            report.kind(),
            report.skipped()
        );
        assert_eq!(report.cases().len(), 7, "{}", report.kind());
    }
    assert_eq!(Check::ALL.len(), 7);
}
