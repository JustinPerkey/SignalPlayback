//! A conforming external stage, written the way a vendor would write one
//! (`docs/DESIGN.md` §9.9).
//!
//! It exists to be loaded: `sp-ext`'s tests run it both as a linked-in symbol
//! source and as the built `cdylib`, which is what keeps the ABI honest — a
//! change to the host's marshalling that this library does not agree with
//! fails the build rather than a user's run.
//!
//! Nothing here depends on the rest of the workspace. The structs below are
//! declared from the header in §9.9, exactly as a C or C++ library would
//! declare them, so this file also serves as the reference for what a
//! conforming implementation has to do:
//!
//! * describe yourself once, in JSON, including a parameter schema;
//! * keep per-instance state behind the opaque handle, never in a global;
//! * account for every input signal in `dispositions`;
//! * hand back buffers you allocated, and free them in `sp_free_output`.
//!
//! The algorithm itself is deliberately trivial — a gain, and an optional
//! envelope alongside it — because what is being demonstrated is the
//! boundary, not the DSP.

use std::ffi::{c_char, c_void, CStr, CString};

pub const ABI_VERSION: u32 = 1;

const OK: i32 = 0;
const FAILED: i32 = 1;

const REPLACED: u8 = 0;

/// One signal, as §9.9 declares it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Signal {
    pub name: *const c_char,
    pub dtype: u8,
    pub domain: u8,
    pub data: *const c_void,
    pub len: u64,
    pub sample_rate_hz: f64,
    pub t0_s: f64,
    pub attrs_json: *const c_char,
}

/// One group: the unit of work.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Group {
    pub group_id: u64,
    pub signals: *const Signal,
    pub signal_count: u32,
    pub group_json: *const c_char,
    pub inbound_json: *const c_char,
}

/// What one call produced.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Output {
    pub signals: *mut Signal,
    pub signal_count: u32,
    pub dispositions: *const u8,
    pub disposition_count: u32,
    pub artifacts_json: *const c_char,
    pub metrics_json: *const c_char,
    pub diagnostics_json: *const c_char,
}

/// The f64 dtype code, which is what the host lends and what this library
/// hands back.
const F64: u8 = 1;

const DESCRIPTOR: &str = r#"{
    "kind": "ext.sample.gain",
    "version": 1,
    "label": "Sample gain (external)",
    "summary": "The reference external stage: a flat gain, with an optional envelope",
    "concurrency": "thread_safe",
    "params": [
        {"name": "gain", "label": "Gain", "kind": {"type": "float", "min": -1000, "max": 1000},
         "default": 2.0, "help": "Every sample is multiplied by this"},
        {"name": "add_envelope", "label": "Add envelope", "kind": {"type": "bool"},
         "default": false, "help": "Publish |x| alongside each signal as a new signal"}
    ],
    "inputs": [{"name": "signals", "kind": "signals"}],
    "outputs": [{"name": "signals", "kind": "signals"}]
}"#;

/// Everything one instance owns. The host may open several against the same
/// library and call them from separate threads, which is what
/// `"concurrency": "thread_safe"` above promises — so none of this is global.
#[derive(Debug)]
struct Instance {
    gain: f64,
    add_envelope: bool,
    /// Groups this instance has seen, published as a metric. Per-instance
    /// state exists here to prove the handle really is the state's home.
    groups_seen: u64,
    /// Buffers handed to the host and not yet freed. Moving an entry moves
    /// the `Vec` headers, never the heap buffers the host holds pointers
    /// into, so growing this list cannot invalidate a live output.
    live: Vec<Owned>,
}

/// The allocations behind one [`Output`], kept until `sp_free_output`.
#[derive(Debug)]
struct Owned {
    signals: Vec<Signal>,
    samples: Vec<Vec<f64>>,
    dispositions: Vec<u8>,
    strings: Vec<CString>,
}

impl Instance {
    fn from_params(json: &str) -> Result<Self, String> {
        let params: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| format!("parameters are not JSON: {error}"))?;
        let gain = params
            .get("gain")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(2.0);
        if !gain.is_finite() {
            return Err("gain must be a finite number".to_owned());
        }
        Ok(Self {
            gain,
            add_envelope: params
                .get("add_envelope")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            groups_seen: 0,
            live: Vec::new(),
        })
    }

    /// The gain applied to one group, with everything the host needs to read
    /// the result back.
    ///
    /// # Safety
    ///
    /// The group's signal array and every buffer it points at must be
    /// readable for the duration of the call, which is what the host promises
    /// when it lends them (§9.9).
    unsafe fn process(&mut self, group: &Group) -> Owned {
        self.groups_seen += 1;
        let inputs = borrowed(group);

        let mut owned = Owned {
            signals: Vec::new(),
            samples: Vec::new(),
            dispositions: Vec::new(),
            strings: Vec::new(),
        };

        // Every input is replaced, so the disposition list is one byte per
        // input and the replacement buffers come first, in input order.
        for signal in &inputs {
            let values: Vec<f64> = samples_of(signal).iter().map(|x| x * self.gain).collect();
            owned.dispositions.push(REPLACED);
            owned.samples.push(values);
            owned.strings.push(name_of(signal));
        }
        if self.add_envelope {
            for (at, signal) in inputs.iter().enumerate() {
                owned
                    .samples
                    .push(samples_of(signal).iter().map(|x| x.abs()).collect());
                owned.strings.push(
                    CString::new(format!("envelope {at}")).unwrap_or_else(|_| CString::default()),
                );
            }
        }

        for (at, values) in owned.samples.iter().enumerate() {
            let template = inputs.get(at.min(inputs.len().saturating_sub(1)));
            owned.signals.push(Signal {
                name: owned.strings[at].as_ptr(),
                dtype: F64,
                domain: template.map_or(0, |signal| signal.domain),
                data: values.as_ptr().cast(),
                len: values.len() as u64,
                sample_rate_hz: template.map_or(0.0, |signal| signal.sample_rate_hz),
                t0_s: template.map_or(0.0, |signal| signal.t0_s),
                attrs_json: std::ptr::null(),
            });
        }
        owned
    }

    /// The metrics for a group: the gain that was applied, the peak that came
    /// out of it, and how many groups this instance has now seen.
    fn metrics(&self, owned: &Owned) -> String {
        let peak = owned
            .samples
            .iter()
            .flatten()
            .fold(0.0_f64, |peak, value| peak.max(value.abs()));
        format!(
            r#"{{"gain":{},"peak_abs":{peak},"groups_seen":{}}}"#,
            self.gain, self.groups_seen
        )
    }
}

/// The version of the ABI this library was built against.
///
/// # Safety
///
/// Callable at any time; it reads nothing.
#[no_mangle]
pub unsafe extern "C" fn sp_abi_version() -> u32 {
    ABI_VERSION
}

/// The stage this library implements, as JSON.
///
/// # Safety
///
/// The returned pointer is a static string and is valid for the life of the
/// library; the host must not free it.
#[no_mangle]
pub unsafe extern "C" fn sp_describe() -> *const c_char {
    // A NUL-terminated copy of DESCRIPTOR, made once.
    static DESCRIPTOR_C: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    DESCRIPTOR_C
        .get_or_init(|| CString::new(DESCRIPTOR).unwrap_or_default())
        .as_ptr()
}

/// Opens one instance for the given parameters.
///
/// # Safety
///
/// `params_json` must be a NUL-terminated string, and `err` must point at
/// `err_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn sp_open(
    params_json: *const c_char,
    err: *mut c_char,
    err_len: usize,
) -> *mut c_void {
    let json = read_string(params_json).unwrap_or_else(|| "{}".to_owned());
    match Instance::from_params(&json) {
        Ok(instance) => Box::into_raw(Box::new(instance)).cast(),
        Err(reason) => {
            write_error(&reason, err, err_len);
            std::ptr::null_mut()
        }
    }
}

/// Told about the run before its first group. This library has nothing to set
/// up, and says so by accepting.
///
/// # Safety
///
/// `handle` must be one `sp_open` returned and has not been closed.
#[no_mangle]
pub unsafe extern "C" fn sp_begin_run(handle: *mut c_void, _run_json: *const c_char) -> i32 {
    if handle.is_null() {
        return FAILED;
    }
    OK
}

/// One group in, one output out.
///
/// # Safety
///
/// `handle` must be open, `input` must point at a valid group whose buffers
/// are readable for the duration of the call, and `out` must point at an
/// output struct the host owns.
#[no_mangle]
pub unsafe extern "C" fn sp_process(
    handle: *mut c_void,
    input: *const Group,
    out: *mut Output,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let (Some(instance), false) = (
        handle.cast::<Instance>().as_mut(),
        input.is_null() || out.is_null(),
    ) else {
        write_error(
            "the call is missing its handle, group or output",
            err,
            err_len,
        );
        return FAILED;
    };
    let group = &*input;

    let owned = instance.process(group);

    let metrics = CString::new(instance.metrics(&owned)).unwrap_or_default();
    publish(instance, owned, Some(metrics), out);
    OK
}

/// The end of the run. This library publishes nothing run-level.
///
/// # Safety
///
/// As for `sp_process`.
#[no_mangle]
pub unsafe extern "C" fn sp_end_run(handle: *mut c_void, out: *mut Output) -> i32 {
    let (Some(instance), false) = (handle.cast::<Instance>().as_mut(), out.is_null()) else {
        return FAILED;
    };
    let owned = Owned {
        signals: Vec::new(),
        samples: Vec::new(),
        dispositions: Vec::new(),
        strings: Vec::new(),
    };
    publish(instance, owned, None, out);
    OK
}

/// Frees what `sp_process` or `sp_end_run` allocated.
///
/// # Safety
///
/// `out` must be an output this library filled in, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn sp_free_output(handle: *mut c_void, out: *mut Output) {
    let (Some(instance), false) = (handle.cast::<Instance>().as_mut(), out.is_null()) else {
        return;
    };
    let output = &mut *out;
    // Match the allocation by the pointer the host was given, so freeing one
    // output never takes another's buffers with it.
    let found = instance.live.iter().position(|owned| {
        std::ptr::eq(owned.signals.as_ptr(), output.signals)
            || std::ptr::eq(owned.dispositions.as_ptr(), output.dispositions)
    });
    if let Some(at) = found {
        instance.live.remove(at);
    }
    *output = Output {
        signals: std::ptr::null_mut(),
        signal_count: 0,
        dispositions: std::ptr::null(),
        disposition_count: 0,
        artifacts_json: std::ptr::null(),
        metrics_json: std::ptr::null(),
        diagnostics_json: std::ptr::null(),
    };
}

/// Closes an instance.
///
/// # Safety
///
/// `handle` must be one `sp_open` returned, closed at most once.
#[no_mangle]
pub unsafe extern "C" fn sp_close(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    drop(Box::from_raw(handle.cast::<Instance>()));
}

/// The address of one exported symbol, for a host that has this library linked
/// in rather than loaded from a file — which is how `sp-ext` exercises the ABI
/// in its own tests.
#[must_use]
pub fn symbol(name: &str) -> Option<*const c_void> {
    Some(match name {
        "sp_abi_version" => sp_abi_version as *const c_void,
        "sp_describe" => sp_describe as *const c_void,
        "sp_open" => sp_open as *const c_void,
        "sp_begin_run" => sp_begin_run as *const c_void,
        "sp_process" => sp_process as *const c_void,
        "sp_end_run" => sp_end_run as *const c_void,
        "sp_free_output" => sp_free_output as *const c_void,
        "sp_close" => sp_close as *const c_void,
        _ => return None,
    })
}

/// Hands `owned` to the host and keeps it alive until it is freed.
///
/// # Safety
///
/// `out` must be a writable output struct.
unsafe fn publish(
    instance: &mut Instance,
    mut owned: Owned,
    metrics: Option<CString>,
    out: *mut Output,
) {
    let metrics_at = metrics.map(|metrics| {
        owned.strings.push(metrics);
        owned.strings.len() - 1
    });
    instance.live.push(owned);
    let stored = instance.live.last_mut().expect("it was just pushed");
    *out = Output {
        signals: stored.signals.as_mut_ptr(),
        signal_count: stored.signals.len() as u32,
        dispositions: stored.dispositions.as_ptr(),
        disposition_count: stored.dispositions.len() as u32,
        artifacts_json: std::ptr::null(),
        metrics_json: metrics_at.map_or(std::ptr::null(), |at| stored.strings[at].as_ptr()),
        diagnostics_json: std::ptr::null(),
    };
}

/// # Safety
///
/// The group's signal array must hold `signal_count` entries.
unsafe fn borrowed(group: &Group) -> Vec<Signal> {
    if group.signals.is_null() || group.signal_count == 0 {
        return Vec::new();
    }
    std::slice::from_raw_parts(group.signals, group.signal_count as usize).to_vec()
}

/// # Safety
///
/// The signal's buffer must hold `len` f64 samples, as the host promises.
unsafe fn samples_of(signal: &Signal) -> &[f64] {
    if signal.data.is_null() || signal.len == 0 {
        return &[];
    }
    std::slice::from_raw_parts(signal.data.cast::<f64>(), signal.len as usize)
}

/// # Safety
///
/// `signal.name` must be NUL-terminated or null.
unsafe fn name_of(signal: &Signal) -> CString {
    read_string(signal.name)
        .and_then(|name| CString::new(name).ok())
        .unwrap_or_default()
}

/// # Safety
///
/// `text` must be NUL-terminated or null.
unsafe fn read_string(text: *const c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    Some(CStr::from_ptr(text).to_string_lossy().into_owned())
}

/// Writes a message into the host's buffer, truncated to fit and always
/// NUL-terminated.
///
/// # Safety
///
/// `err` must point at `err_len` writable bytes, or be null.
unsafe fn write_error(message: &str, err: *mut c_char, err_len: usize) {
    if err.is_null() || err_len == 0 {
        return;
    }
    let bytes = message.as_bytes();
    let room = err_len - 1;
    let len = bytes.len().min(room);
    std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), err, len);
    *err.add(len) = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_the_json_the_host_will_parse() {
        let json: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
        assert_eq!(json["kind"], "ext.sample.gain");
        assert_eq!(json["params"][0]["name"], "gain");
    }

    #[test]
    fn parameters_that_are_not_usable_are_refused_at_open_rather_than_per_group() {
        // Both roads lead to `sp_open` returning null with a message: the
        // host reports it against the parameter form, before a run starts.
        let malformed = Instance::from_params("{gain: 2}").unwrap_err();
        assert!(malformed.contains("not JSON"), "{malformed}");
        let unusable = Instance::from_params(r#"{"gain":1e999}"#).unwrap_err();
        assert!(!unusable.is_empty(), "a refusal always says something");
    }

    #[test]
    fn an_instance_takes_the_parameters_it_was_opened_with() {
        let instance = Instance::from_params(r#"{"gain":4.0,"add_envelope":true}"#).unwrap();
        assert_eq!(instance.gain, 4.0);
        assert!(instance.add_envelope);
        assert_eq!(instance.groups_seen, 0);
    }

    #[test]
    fn every_named_symbol_resolves() {
        for name in [
            "sp_abi_version",
            "sp_describe",
            "sp_open",
            "sp_begin_run",
            "sp_process",
            "sp_end_run",
            "sp_free_output",
            "sp_close",
        ] {
            assert!(symbol(name).is_some(), "{name} is exported");
        }
        assert!(symbol("sp_nothing").is_none());
    }
}
