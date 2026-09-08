//! The stage a loaded library becomes (`docs/DESIGN.md` §9.9).
//!
//! [`ExtStage`] is the whole of the translation: a [`GroupFrame`] goes out as
//! the flat structs of [`crate::abi`], and what comes back becomes an ordinary
//! [`StageOutput`]. Nothing downstream — the recorder, the results screen, the
//! run diff — can tell the difference, which is the property the crate exists
//! to provide.
//!
//! Two rules keep the memory story simple enough to hold in your head:
//!
//! * **Input buffers are borrowed.** The host owns them and they are valid for
//!   exactly the duration of one call, so everything lent is kept alive in
//!   local variables until `sp_process` returns.
//! * **Output buffers are the library's.** They are read, copied into owned
//!   Rust values, and handed straight back through `sp_free_output` — in a
//!   scope guard, so a mis-shaped output still frees rather than leaking.

use std::collections::BTreeMap;
use std::ffi::{c_char, CStr, CString};
use std::sync::Arc;

use serde::Deserialize;
use sp_core::{Attributes, DType, Diagnostic, Domain, SampleBuffer};
use sp_proc::error::{ConfigError, StageError};
use sp_proc::frame::GroupFrame;
use sp_proc::param::ParamSet;
use sp_proc::stage::{
    ArtifactOut, RunCtx, SignalOut, Stage, StageCtx, StageDescriptor, StageOutput,
};

use crate::abi::{self, Disposition, Handle};
use crate::library::{c_string, ExtLibrary};

/// One instance of an external stage: one library, one open handle.
///
/// Every group gets its own instance, as it does for a built-in stage, so a
/// library may keep state across the groups *it* sees without the host having
/// to reason about which groups those were.
#[derive(Debug)]
pub struct ExtStage {
    library: Arc<ExtLibrary>,
    /// Null until `configure`, and again after `close`.
    handle: Handle,
}

// SAFETY: the handle is opened, used and closed by this value alone, and it is
// only reachable through `&mut self`. A library that declared itself anything
// but `thread_safe` is additionally serialised by `ExtLibrary::lock`, so at
// most one call is ever in flight in it.
unsafe impl Send for ExtStage {}
// SAFETY: nothing is reachable through a shared reference except the library,
// which is itself `Sync`; every call into the library needs `&mut self`.
unsafe impl Sync for ExtStage {}

impl ExtStage {
    #[must_use]
    pub fn new(library: Arc<ExtLibrary>) -> Self {
        Self {
            library,
            handle: std::ptr::null_mut(),
        }
    }

    #[must_use]
    pub fn library(&self) -> &Arc<ExtLibrary> {
        &self.library
    }

    fn close(&mut self) {
        if self.handle.is_null() {
            return;
        }
        let handle = std::mem::replace(&mut self.handle, std::ptr::null_mut());
        // SAFETY: the handle came from this library's `sp_open` and has not
        // been closed; after this it is unreachable.
        unsafe { (self.library.vtable().close)(handle) };
    }

    fn opened(&self) -> Result<Handle, StageError> {
        if self.handle.is_null() {
            return Err(StageError::rejected(
                "the external stage was never configured, so no library instance is open",
            ));
        }
        Ok(self.handle)
    }

    /// What this run looks like to the library: enough to name the run in a
    /// log of its own, and no more.
    fn run_json(ctx: &RunCtx) -> String {
        serde_json::json!({
            "run_id": ctx.run.get(),
            "pipeline_hash": ctx.pipeline_hash,
            "stage_ordinal": ctx.stage_ordinal,
        })
        .to_string()
    }

    /// Reads an [`abi::Output`] the library filled in, then frees it.
    fn take_output(
        &self,
        handle: Handle,
        mut output: abi::Output,
        inputs: usize,
    ) -> Result<StageOutput, StageError> {
        let read = read_output(&output, inputs);
        // SAFETY: `output` was filled in by this library's `sp_process` or
        // `sp_end_run` and is freed exactly once, whether or not reading it
        // succeeded — a mis-shaped output is still the library's to free.
        unsafe { (self.library.vtable().free_output)(handle, &raw mut output) };
        read
    }
}

impl Drop for ExtStage {
    fn drop(&mut self) {
        self.close();
    }
}

impl Stage for ExtStage {
    fn descriptor(&self) -> &'static StageDescriptor {
        self.library.descriptor()
    }

    /// Opens a library instance for these parameters.
    ///
    /// The host validates against the declared schema first, so the common
    /// mistakes — a typo, a value out of range — are reported by the same code
    /// that reports them for a built-in stage, and the library only ever sees
    /// parameters that already satisfy what it published.
    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError> {
        let resolved = params.resolved(self.descriptor().params)?;
        self.close();

        let json = c_string(&resolved.to_json());
        let mut err = vec![0 as c_char; abi::ERR_LEN];
        let _lock = self.library.lock();
        // SAFETY: both pointers are valid for the call — the JSON outlives it,
        // and the error buffer is exactly the length declared alongside it.
        let handle =
            unsafe { (self.library.vtable().open)(json.as_ptr(), err.as_mut_ptr(), err.len()) };
        if handle.is_null() {
            return Err(ConfigError::rejected(message(&err).unwrap_or_else(|| {
                "the library refused these parameters without saying why".to_owned()
            })));
        }
        self.handle = handle;
        Ok(())
    }

    /// The library file's hash, so recompiling it invalidates its cached
    /// output and nothing else's (§9.5).
    fn cache_salt(&self) -> Option<String> {
        let hash = self.library.hash();
        (!hash.is_empty()).then(|| hash.to_owned())
    }

    fn begin_run(&mut self, ctx: &RunCtx) -> Result<(), StageError> {
        let handle = self.opened()?;
        let json = c_string(&Self::run_json(ctx));
        let _lock = self.library.lock();
        // SAFETY: the handle is open and the JSON outlives the call.
        let status = unsafe { (self.library.vtable().begin_run)(handle, json.as_ptr()) };
        if status != abi::OK {
            return Err(StageError::rejected(format!(
                "the library refused to start the run (status {status})"
            )));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame) -> Result<StageOutput, StageError> {
        ctx.check()?;
        let handle = self.opened()?;

        // Everything lent to the library is kept alive in `lent` until the
        // call returns; the signal structs only point into it.
        let lent = Lent::of(input)?;
        let group = lent.group();
        let mut output = abi::Output::empty();
        let mut err = vec![0 as c_char; abi::ERR_LEN];

        let status = {
            let _lock = self.library.lock();
            // SAFETY: `group` borrows `lent`, which outlives the call; the
            // output struct is zeroed and is the library's to fill in; the
            // error buffer is as long as the length passed with it.
            unsafe {
                (self.library.vtable().process)(
                    handle,
                    &raw const group,
                    &raw mut output,
                    err.as_mut_ptr(),
                    err.len(),
                )
            }
        };
        if status != abi::OK {
            return Err(StageError::rejected(message(&err).unwrap_or_else(|| {
                format!("the library failed this group with status {status}")
            })));
        }

        let mut result = self.take_output(handle, output, input.signals.len())?;
        result.diagnose(Diagnostic::info(self.library.provenance()));
        Ok(result)
    }

    fn end_run(&mut self, _ctx: &RunCtx) -> Result<StageOutput, StageError> {
        let handle = self.opened()?;
        let mut output = abi::Output::empty();
        let status = {
            let _lock = self.library.lock();
            // SAFETY: as for `process`, with no group to lend.
            unsafe { (self.library.vtable().end_run)(handle, &raw mut output) }
        };
        if status != abi::OK {
            return Err(StageError::rejected(format!(
                "the library failed at the end of the run (status {status})"
            )));
        }
        // No group, so no input signals to account for: a run-level call may
        // only publish artifacts, metrics and diagnostics.
        self.take_output(handle, output, 0)
    }
}

/// The host-owned side of one call: the buffers and strings the library
/// borrows, held together so they outlive it.
struct Lent {
    group_id: u64,
    signals: Vec<abi::Signal>,
    /// Kept alive because `signals` points into them.
    _samples: Vec<Vec<f64>>,
    _names: Vec<CString>,
    _attrs: Vec<CString>,
    group_json: CString,
    inbound_json: CString,
}

impl Lent {
    fn of(frame: &GroupFrame) -> Result<Self, StageError> {
        let count = frame.signals.len();
        let mut samples = Vec::with_capacity(count);
        let mut names = Vec::with_capacity(count);
        let mut attrs = Vec::with_capacity(count);
        let mut signals = Vec::with_capacity(count);

        for signal in &frame.signals {
            // f64 for now: §17.10 has every stage doing its arithmetic in
            // f64, and the dtype byte is in the struct so lending native
            // buffers later does not change the ABI's shape.
            samples.push(signal.read_values()?);
            names.push(c_string(signal.name()));
            attrs.push(c_string(
                &serde_json::to_string(signal.attributes()).unwrap_or_else(|_| "{}".to_owned()),
            ));
        }

        for (at, signal) in frame.signals.iter().enumerate() {
            let timebase = signal.timebase();
            signals.push(abi::Signal {
                name: names[at].as_ptr(),
                dtype: DType::F64.code(),
                domain: domain_code(signal.domain()),
                data: samples[at].as_ptr().cast(),
                len: samples[at].len() as u64,
                sample_rate_hz: timebase.sample_rate_hz.unwrap_or(0.0),
                t0_s: timebase.t0_s,
                attrs_json: attrs[at].as_ptr(),
            });
        }

        let inbound: Vec<serde_json::Value> = frame
            .inbound
            .iter()
            .map(|value| {
                serde_json::json!({
                    "port": value.port,
                    "kind": value.kind,
                    "kind_version": value.kind_version,
                    "stage_ordinal": value.stage_ordinal,
                    "payload": serde_json::from_str::<serde_json::Value>(&value.payload_json)
                        .unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();

        Ok(Self {
            group_id: frame.group.id.get() as u64,
            signals,
            _samples: samples,
            _names: names,
            _attrs: attrs,
            group_json: c_string(
                &serde_json::to_string(&frame.group).unwrap_or_else(|_| "{}".to_owned()),
            ),
            inbound_json: c_string(
                &serde_json::to_string(&inbound).unwrap_or_else(|_| "[]".to_owned()),
            ),
        })
    }

    fn group(&self) -> abi::Group {
        abi::Group {
            group_id: self.group_id,
            signals: self.signals.as_ptr(),
            signal_count: self.signals.len() as u32,
            group_json: self.group_json.as_ptr(),
            inbound_json: self.inbound_json.as_ptr(),
        }
    }
}

/// One artifact as a library publishes it.
#[derive(Debug, Deserialize)]
struct ArtifactJson {
    port: String,
    kind: String,
    #[serde(default = "one")]
    kind_version: u32,
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    summary: Option<String>,
}

const fn one() -> u32 {
    1
}

/// Turns what the library filled in into a [`StageOutput`], checking
/// everything the host is in a position to check (§9.9).
///
/// The disposition array is per *input* signal, so "account for every input"
/// (§9.4) holds by construction rather than by trusting the library to say so.
fn read_output(output: &abi::Output, inputs: usize) -> Result<StageOutput, StageError> {
    let dispositions = read_dispositions(output, inputs)?;
    let replaced = dispositions
        .iter()
        .filter(|d| **d == Disposition::Replaced)
        .count();

    let count = output.signal_count as usize;
    if count < replaced {
        return Err(StageError::rejected(format!(
            "the library said it replaced {replaced} signal(s) but returned {count}"
        )));
    }
    if count > 0 && output.signals.is_null() {
        return Err(StageError::rejected(
            "the library returned signals but no buffer to read them from",
        ));
    }
    // SAFETY: `signals` is non-null whenever `count > 0`, and the library
    // promises `signal_count` entries behind it.
    let returned: &[abi::Signal] = if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(output.signals, count) }
    };

    let mut result = StageOutput::new();
    let mut next = 0;
    for (ordinal, disposition) in dispositions.iter().enumerate() {
        match disposition {
            Disposition::Replaced => {
                let samples = read_samples(&returned[next], ordinal)?;
                next += 1;
                result.signals.push(SignalOut::replace(ordinal, samples));
            }
            Disposition::Passthrough => {
                result.signals.push(SignalOut::Passthrough { ordinal });
            }
            Disposition::Dropped => result.signals.push(SignalOut::Drop { ordinal }),
        }
    }

    for (at, signal) in returned.iter().enumerate().skip(replaced) {
        let samples = read_samples(signal, at)?;
        result.signals.push(SignalOut::Add {
            name: read_string(signal.name).unwrap_or_else(|| format!("added {}", at - replaced)),
            domain: domain_from_code(signal.domain).ok_or_else(|| {
                StageError::rejected(format!(
                    "an added signal has domain code {}, which is not a domain",
                    signal.domain
                ))
            })?,
            samples,
            attrs: read_attributes(signal.attrs_json)?,
        });
    }

    result.artifacts = read_artifacts(output.artifacts_json)?;
    result.metrics = read_json(output.metrics_json, "metrics")?.unwrap_or_default();
    result.diagnostics = read_json(output.diagnostics_json, "diagnostics")?.unwrap_or_default();
    Ok(result)
}

fn read_dispositions(output: &abi::Output, inputs: usize) -> Result<Vec<Disposition>, StageError> {
    let declared = output.disposition_count as usize;
    if declared != inputs {
        return Err(StageError::rejected(format!(
            "the library accounted for {declared} input signal(s); the group has {inputs}"
        )));
    }
    if inputs == 0 {
        return Ok(Vec::new());
    }
    if output.dispositions.is_null() {
        return Err(StageError::rejected(
            "the library said what it did to each input signal but returned no list",
        ));
    }
    // SAFETY: non-null with `disposition_count` bytes behind it, which was
    // just checked against the input count.
    let codes = unsafe { std::slice::from_raw_parts(output.dispositions, inputs) };
    codes
        .iter()
        .enumerate()
        .map(|(ordinal, code)| {
            Disposition::from_code(*code).ok_or_else(|| {
                StageError::rejected(format!(
                    "input signal {ordinal} came back with disposition {code}, which means nothing"
                ))
            })
        })
        .collect()
}

/// Copies one returned buffer into an owned [`SampleBuffer`].
///
/// `f32` and `f64` are what a library may hand back; anything else is refused
/// by name rather than reinterpreted, because guessing at a buffer's element
/// type is how a stage silently produces noise.
fn read_samples(signal: &abi::Signal, at: usize) -> Result<SampleBuffer, StageError> {
    let len = usize::try_from(signal.len).map_err(|_| {
        StageError::rejected(format!(
            "returned signal {at} declares {} samples",
            signal.len
        ))
    })?;
    if len == 0 {
        return Ok(SampleBuffer::from_f64(DType::F64, &[]));
    }
    if signal.data.is_null() {
        return Err(StageError::rejected(format!(
            "returned signal {at} declares {len} samples but has no buffer"
        )));
    }
    let dtype = DType::from_code(signal.dtype).ok_or_else(|| {
        StageError::rejected(format!(
            "returned signal {at} has dtype code {}, which is not a dtype",
            signal.dtype
        ))
    })?;
    let values: Vec<f64> = match dtype {
        // SAFETY: non-null with `len` elements of the declared type behind it,
        // which is the contract `sp_signal` states.
        DType::F64 => {
            unsafe { std::slice::from_raw_parts(signal.data.cast::<f64>(), len) }.to_vec()
        }
        DType::F32 => unsafe { std::slice::from_raw_parts(signal.data.cast::<f32>(), len) }
            .iter()
            .map(|value| f64::from(*value))
            .collect(),
        other => {
            return Err(StageError::rejected(format!(
                "returned signal {at} is {other:?}; a library returns f32 or f64 (§9.9)"
            )))
        }
    };
    Ok(SampleBuffer::from_f64(dtype, &values))
}

fn read_artifacts(json: *const c_char) -> Result<Vec<ArtifactOut>, StageError> {
    let published: Vec<ArtifactJson> = read_json(json, "artifacts")?.unwrap_or_default();
    published
        .into_iter()
        .map(|artifact| {
            Ok(ArtifactOut {
                port: artifact.port,
                kind: artifact.kind,
                kind_version: artifact.kind_version,
                payload_json: serde_json::to_string(&artifact.payload)?,
                summary: artifact.summary,
            })
        })
        .collect()
}

fn read_attributes(json: *const c_char) -> Result<Attributes, StageError> {
    Ok(read_json(json, "signal attributes")?.unwrap_or_default())
}

/// Parses one of the JSON fields of an output, naming the field when it does
/// not parse — "the library's metrics are not JSON" is a far better report
/// than a bare serde error against a document nobody can see.
fn read_json<T: serde::de::DeserializeOwned>(
    json: *const c_char,
    what: &str,
) -> Result<Option<T>, StageError> {
    let Some(text) = read_string(json) else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text).map(Some).map_err(|error| {
        StageError::rejected(format!("the library's {what} are unreadable: {error}"))
    })
}

fn read_string(text: *const c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null, and the ABI says every string it hands back is
    // NUL-terminated and valid for the call.
    Some(
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// The message a library wrote into the error buffer, if it wrote one.
fn message(err: &[c_char]) -> Option<String> {
    let text = read_string(err.as_ptr())?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn domain_code(domain: Domain) -> u8 {
    Domain::ALL
        .iter()
        .position(|candidate| *candidate == domain)
        .unwrap_or(0) as u8
}

fn domain_from_code(code: u8) -> Option<Domain> {
    Domain::ALL.get(code as usize).copied()
}

/// Metrics come back as a plain JSON object; this is the type they land in.
pub type Metrics = BTreeMap<String, f64>;

#[cfg(test)]
mod tests {
    use super::*;

    /// An output as a library would fill it in, with the owned buffers behind
    /// it kept alive alongside.
    struct Filled {
        output: abi::Output,
        _dispositions: Vec<u8>,
        _signals: Vec<abi::Signal>,
        _samples: Vec<Vec<f64>>,
        _strings: Vec<CString>,
    }

    fn filled(dispositions: &[Disposition], returned: &[&[f64]]) -> Filled {
        let codes: Vec<u8> = dispositions.iter().map(|d| d.code()).collect();
        let samples: Vec<Vec<f64>> = returned.iter().map(|values| values.to_vec()).collect();
        let strings: Vec<CString> = (0..returned.len())
            .map(|at| c_string(&format!("out{at}")))
            .collect();
        let signals: Vec<abi::Signal> = samples
            .iter()
            .enumerate()
            .map(|(at, values)| abi::Signal {
                name: strings[at].as_ptr(),
                dtype: DType::F64.code(),
                domain: 0,
                data: values.as_ptr().cast(),
                len: values.len() as u64,
                sample_rate_hz: 1000.0,
                t0_s: 0.0,
                attrs_json: std::ptr::null(),
            })
            .collect();

        let output = abi::Output {
            signals: signals.as_ptr().cast_mut(),
            signal_count: signals.len() as u32,
            dispositions: codes.as_ptr(),
            disposition_count: codes.len() as u32,
            ..abi::Output::empty()
        };
        Filled {
            output,
            _dispositions: codes,
            _signals: signals,
            _samples: samples,
            _strings: strings,
        }
    }

    #[test]
    fn a_replacement_lands_on_the_input_it_replaced() {
        let filled = filled(
            &[Disposition::Replaced, Disposition::Passthrough],
            &[&[1.0, 2.0]],
        );
        let output = read_output(&filled.output, 2).unwrap();
        assert_eq!(output.signals.len(), 2);
        let SignalOut::Replace {
            ordinal, samples, ..
        } = &output.signals[0]
        else {
            panic!("expected a replacement, got {:?}", output.signals[0]);
        };
        assert_eq!(*ordinal, 0);
        assert_eq!(samples.values().collect::<Vec<_>>(), [1.0, 2.0]);
        assert_eq!(output.signals[1], SignalOut::Passthrough { ordinal: 1 });
    }

    #[test]
    fn a_signal_beyond_the_replacements_is_an_addition() {
        let filled = filled(&[Disposition::Passthrough], &[&[3.0]]);
        let output = read_output(&filled.output, 1).unwrap();
        let SignalOut::Add { name, samples, .. } = &output.signals[1] else {
            panic!("expected an addition, got {:?}", output.signals[1]);
        };
        assert_eq!(name, "out0");
        assert_eq!(samples.len(), 1);
    }

    #[test]
    fn a_disposition_list_that_does_not_match_the_group_is_refused() {
        // The library was handed two signals and accounted for one; taking it
        // at its word would silently drop the other.
        let filled = filled(&[Disposition::Passthrough], &[]);
        let error = read_output(&filled.output, 2).unwrap_err();
        assert!(error.to_string().contains("the group has 2"), "{error}");
    }

    #[test]
    fn fewer_buffers_than_replacements_is_refused_rather_than_read() {
        let filled = filled(&[Disposition::Replaced, Disposition::Replaced], &[&[1.0]]);
        let error = read_output(&filled.output, 2).unwrap_err();
        assert!(error.to_string().contains("replaced 2"), "{error}");
    }

    #[test]
    fn a_disposition_the_abi_does_not_define_is_named() {
        let codes = [7u8];
        let output = abi::Output {
            dispositions: codes.as_ptr(),
            disposition_count: 1,
            ..abi::Output::empty()
        };
        let error = read_output(&output, 1).unwrap_err();
        assert!(error.to_string().contains("disposition 7"), "{error}");
    }

    #[test]
    fn unreadable_metrics_name_the_field_rather_than_the_parser() {
        let json = c_string("{not json");
        let output = abi::Output {
            metrics_json: json.as_ptr(),
            ..abi::Output::empty()
        };
        let error = read_output(&output, 0).unwrap_err();
        assert!(
            error.to_string().contains("metrics are unreadable"),
            "{error}"
        );
    }

    #[test]
    fn an_output_with_nothing_in_it_is_a_valid_empty_result() {
        let output = read_output(&abi::Output::empty(), 0).unwrap();
        assert!(output.signals.is_empty());
        assert!(output.metrics.is_empty());
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn artifacts_and_metrics_come_through_as_published() {
        let artifacts = c_string(
            r#"[{"port":"spectrum","kind":"spectrum.v1","payload":{"peak":3.0},"summary":"one line"}]"#,
        );
        let metrics = c_string(r#"{"snr_db":12.5}"#);
        let diagnostics = c_string(r#"[{"severity":"warn","message":"clipped"}]"#);
        let output = abi::Output {
            artifacts_json: artifacts.as_ptr(),
            metrics_json: metrics.as_ptr(),
            diagnostics_json: diagnostics.as_ptr(),
            ..abi::Output::empty()
        };
        let read = read_output(&output, 0).unwrap();
        assert_eq!(read.artifacts[0].kind, "spectrum.v1");
        assert_eq!(read.artifacts[0].kind_version, 1, "unstated version is 1");
        assert_eq!(read.artifacts[0].payload_json, r#"{"peak":3.0}"#);
        assert_eq!(read.metrics["snr_db"], 12.5);
        assert_eq!(read.diagnostics[0].message, "clipped");
    }

    #[test]
    fn every_domain_survives_the_round_trip_through_its_code() {
        for domain in Domain::ALL {
            assert_eq!(domain_from_code(domain_code(domain)), Some(domain));
        }
        assert_eq!(domain_from_code(200), None);
    }
}
