//! The flat C ABI an external stage speaks (`docs/DESIGN.md` §9.9).
//!
//! Nothing Rust-specific crosses the boundary: every struct is `repr(C)` and
//! every string is a NUL-terminated UTF-8 `char*`, so a library built by any
//! toolchain can conform. The first thing the host calls is
//! [`Vtable::abi_version`]; a library that answers with anything but
//! [`ABI_VERSION`] is refused before a single other symbol is touched, which
//! is what keeps a stale DLL from being read with the wrong struct layout.

use std::ffi::{c_char, c_void};

/// The ABI revision this host speaks. Bumped whenever a struct below changes
/// shape; a library reports its own from `sp_abi_version`.
pub const ABI_VERSION: u32 = 1;

/// A successful call. Anything else is an error, and the message the library
/// wrote into the error buffer says what went wrong.
pub const OK: i32 = 0;

/// How much room the host gives a library to explain itself.
pub const ERR_LEN: usize = 1024;

/// The library's per-instance state, opaque to the host.
pub type Handle = *mut c_void;

/// One signal, borrowed from the host for the duration of a call.
///
/// `data` is host-owned when it arrives in a [`Group`] and library-owned when
/// it comes back in an [`Output`]; neither side ever frees the other's memory.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Signal {
    pub name: *const c_char,
    /// A [`sp_core::DType`] code. The host lends `f64` and takes `f32` or
    /// `f64` back (settled during M9, §9.9).
    pub dtype: u8,
    /// A [`sp_core::Domain`], in declaration order.
    pub domain: u8,
    pub data: *const c_void,
    pub len: u64,
    /// Zero when the timebase is irregular.
    pub sample_rate_hz: f64,
    pub t0_s: f64,
    /// The signal's attributes as a JSON object.
    pub attrs_json: *const c_char,
}

impl Signal {
    /// The zeroed struct a library fills in, and what an empty output holds.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            name: std::ptr::null(),
            dtype: 0,
            domain: 0,
            data: std::ptr::null(),
            len: 0,
            sample_rate_hz: 0.0,
            t0_s: 0.0,
            attrs_json: std::ptr::null(),
        }
    }
}

/// One group: the unit of work, exactly as in §9.1.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Group {
    pub group_id: u64,
    pub signals: *const Signal,
    pub signal_count: u32,
    /// Group metadata and its pulse records (§6.6), as JSON.
    pub group_json: *const c_char,
    /// The artifacts upstream stages published, as JSON.
    pub inbound_json: *const c_char,
}

/// What one call produced. Every pointer in here is library-owned and freed
/// through `sp_free_output` once the host has copied what it needs.
///
/// `dispositions` has one byte per *input* signal, in input order, so the
/// "account for every input" rule of §9.4 holds by construction. `signals`
/// carries the replacement buffers first, in input order, and then any added
/// signals — which is why the host can pair them up without an ordinal field.
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

impl Output {
    /// The zeroed struct the host hands to `sp_process`.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            signals: std::ptr::null_mut(),
            signal_count: 0,
            dispositions: std::ptr::null(),
            disposition_count: 0,
            artifacts_json: std::ptr::null(),
            metrics_json: std::ptr::null(),
            diagnostics_json: std::ptr::null(),
        }
    }
}

/// What a library did to one input signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Replaced,
    Passthrough,
    Dropped,
}

impl Disposition {
    /// The byte written into [`Output::dispositions`].
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Replaced => 0,
            Self::Passthrough => 1,
            Self::Dropped => 2,
        }
    }

    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Replaced),
            1 => Some(Self::Passthrough),
            2 => Some(Self::Dropped),
            _ => None,
        }
    }
}

/// The name of every symbol a conforming library exports.
pub const SYMBOLS: [&str; 8] = [
    "sp_abi_version",
    "sp_describe",
    "sp_open",
    "sp_begin_run",
    "sp_process",
    "sp_end_run",
    "sp_free_output",
    "sp_close",
];

pub type AbiVersionFn = unsafe extern "C" fn() -> u32;
pub type DescribeFn = unsafe extern "C" fn() -> *const c_char;
pub type OpenFn = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> Handle;
pub type BeginRunFn = unsafe extern "C" fn(Handle, *const c_char) -> i32;
pub type ProcessFn =
    unsafe extern "C" fn(Handle, *const Group, *mut Output, *mut c_char, usize) -> i32;
pub type EndRunFn = unsafe extern "C" fn(Handle, *mut Output) -> i32;
pub type FreeOutputFn = unsafe extern "C" fn(Handle, *mut Output);
pub type CloseFn = unsafe extern "C" fn(Handle);

/// The resolved entry points of one library.
#[derive(Debug, Clone, Copy)]
pub struct Vtable {
    pub abi_version: AbiVersionFn,
    pub describe: DescribeFn,
    pub open: OpenFn,
    pub begin_run: BeginRunFn,
    pub process: ProcessFn,
    pub end_run: EndRunFn,
    pub free_output: FreeOutputFn,
    pub close: CloseFn,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_disposition_round_trips_through_its_code() {
        for disposition in [
            Disposition::Replaced,
            Disposition::Passthrough,
            Disposition::Dropped,
        ] {
            assert_eq!(
                Disposition::from_code(disposition.code()),
                Some(disposition)
            );
        }
        assert_eq!(Disposition::from_code(9), None);
    }

    #[test]
    fn the_structs_are_pointer_aligned_and_c_shaped() {
        // A library built by another toolchain lays these out from the header
        // in §9.9; a Rust-side reordering would be silent corruption.
        use std::mem::align_of;
        assert_eq!(align_of::<Signal>(), align_of::<u64>());
        assert_eq!(align_of::<Output>(), align_of::<usize>());
    }
}
