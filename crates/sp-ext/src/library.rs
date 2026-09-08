//! Loading a native library and holding it for as long as runs refer to it
//! (`docs/DESIGN.md` §9.9).
//!
//! An [`ExtLibrary`] is one loaded file: its resolved entry points, the
//! descriptor it published, and the BLAKE3 hash of the bytes on disk that
//! produced them. The hash is what makes a run answerable — "which build made
//! this result?" — and what invalidates cached output when the library is
//! recompiled without its declared version moving (§9.5, G8).
//!
//! Symbols come through [`SymbolSource`] rather than straight from
//! `libloading`, for one reason worth the indirection: the sample library in
//! `sp-ext-sample` can be linked into a test binary and offered through the
//! same interface, so every line of marshalling below is exercised without a
//! DLL and a build step in between.

use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use sp_proc::registry::{RegistryError, StageRegistry};
use sp_proc::stage::StageDescriptor;

use crate::abi::{self, Vtable, ABI_VERSION};
use crate::allow::AllowList;
use crate::descriptor::{Concurrency, LibraryDescriptor};
use crate::error::{ExtError, Result};
use crate::stage::ExtStage;

/// Somewhere to look up an exported symbol by name.
///
/// # Safety
///
/// An implementation must return either null-or-`None` or the address of a
/// symbol that stays valid for as long as the source itself is alive, and it
/// must keep the underlying library loaded for that whole time. Everything
/// built on top of a [`Vtable`] trusts that.
pub unsafe trait SymbolSource: std::fmt::Debug + Send + Sync {
    fn symbol(&self, name: &str) -> Option<*const c_void>;
}

/// A dynamic library on disk.
#[derive(Debug)]
struct NativeLibrary(libloading::Library);

// SAFETY: the library is kept loaded for the lifetime of this value, and
// `libloading::Library::get` returns addresses that stay valid for that long.
unsafe impl SymbolSource for NativeLibrary {
    fn symbol(&self, name: &str) -> Option<*const c_void> {
        let mut symbol = name.as_bytes().to_vec();
        symbol.push(0);
        // SAFETY: the name is NUL-terminated, and the returned pointer is
        // immediately turned into an address rather than being called here.
        unsafe {
            self.0
                .get::<*const c_void>(&symbol)
                .ok()
                .map(|found| *found)
        }
    }
}

/// One loaded external stage library.
#[derive(Debug)]
pub struct ExtLibrary {
    path: PathBuf,
    /// BLAKE3 of the file, or empty for a library linked into this binary.
    hash: String,
    descriptor: &'static StageDescriptor,
    concurrency: Concurrency,
    vtable: Vtable,
    /// Held so the library stays loaded; every call goes through `vtable`.
    _source: Box<dyn SymbolSource>,
    /// Taken around every call into a library that did not declare itself
    /// thread-safe. Groups run in parallel (§9.5), so without this a
    /// `sequential` library would be called from several threads at once.
    exclusive: Mutex<()>,
}

impl ExtLibrary {
    /// Loads the library at `path`, if the user has approved it.
    ///
    /// The order matters: consent, then existence, then ABI version, then the
    /// descriptor. Each step is a thing to tell the user about the file they
    /// picked, and none of them runs the library's own code except the last
    /// two, which are the two calls the ABI promises are safe on any
    /// conforming build.
    pub fn open(path: impl AsRef<Path>, allowed: &AllowList) -> Result<Arc<Self>> {
        let path = path.as_ref();
        if !allowed.permits(path) {
            return Err(ExtError::NotAllowed(path.to_path_buf()));
        }
        if !path.exists() {
            return Err(ExtError::NotFound(path.to_path_buf()));
        }
        let hash = hash_file(path)?;
        // SAFETY: loading runs the library's initialisers, which is exactly
        // what the allow-list above is consent for. There is no way to make
        // this safe in M9; process isolation is M14.
        let library =
            unsafe { libloading::Library::new(path) }.map_err(|source| ExtError::Load {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_source(Box::new(NativeLibrary(library)), path.to_path_buf(), hash)
    }

    /// Builds a library from an already-resolved symbol source: the sample
    /// library linked into a test, or — later — the out-of-process host.
    pub fn from_source(
        source: Box<dyn SymbolSource>,
        path: PathBuf,
        hash: String,
    ) -> Result<Arc<Self>> {
        let vtable = resolve(source.as_ref(), &path)?;

        // SAFETY: the symbols resolved above are the ones the ABI names, and
        // `abi_version` takes no arguments and returns a plain integer, so it
        // is callable before anything else has been agreed.
        let found = unsafe { (vtable.abi_version)() };
        if found != ABI_VERSION {
            return Err(ExtError::AbiMismatch {
                path,
                found,
                expected: ABI_VERSION,
            });
        }

        // SAFETY: the ABI version matched, so `describe` returns a
        // NUL-terminated string that lives as long as the library.
        let json = unsafe {
            let text = (vtable.describe)();
            if text.is_null() {
                return Err(ExtError::Rejected {
                    path,
                    reason: "the library described itself as nothing at all".to_owned(),
                });
            }
            CStr::from_ptr(text).to_string_lossy().into_owned()
        };

        let published = LibraryDescriptor::parse(&json).map_err(|source| ExtError::Descriptor {
            path: path.clone(),
            source,
        })?;
        let concurrency = published.concurrency;
        let descriptor =
            published
                .into_stage_descriptor()
                .map_err(|reason| ExtError::Rejected {
                    path: path.clone(),
                    reason,
                })?;

        tracing::info!(
            path = %path.display(),
            kind = descriptor.kind,
            version = descriptor.version,
            concurrency = concurrency.as_str(),
            "external stage library loaded"
        );

        Ok(Arc::new(Self {
            path,
            hash,
            descriptor,
            concurrency,
            vtable,
            _source: source,
            exclusive: Mutex::new(()),
        }))
    }

    #[must_use]
    pub fn descriptor(&self) -> &'static StageDescriptor {
        self.descriptor
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// BLAKE3 of the file the stage was loaded from. Empty for a library
    /// linked into this binary, which has no file of its own.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    #[must_use]
    pub fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    #[must_use]
    pub(crate) fn vtable(&self) -> &Vtable {
        &self.vtable
    }

    /// Held for the duration of a call into a library that is not thread-safe.
    ///
    /// This gives mutual exclusion, not ordering: a `sequential` library sees
    /// one group at a time, but not necessarily in group order. Ordered
    /// delivery is the cross-group state question still open at §17.7.
    pub(crate) fn lock(&self) -> Option<MutexGuard<'_, ()>> {
        if !self.concurrency.is_exclusive() {
            return None;
        }
        Some(self.exclusive.lock().unwrap_or_else(|poisoned| {
            // A previous call panicked on the way out. The library's own state
            // is its business; the lock is only here to serialise calls.
            poisoned.into_inner()
        }))
    }

    /// One line naming the build behind a result, recorded on every stage row
    /// so a run always answers "which library produced this?" (G8).
    #[must_use]
    pub fn provenance(&self) -> String {
        let hash = if self.hash.is_empty() {
            "linked in".to_owned()
        } else {
            format!("blake3 {}", &self.hash[..16.min(self.hash.len())])
        };
        format!(
            "{} v{} from {} ({hash})",
            self.descriptor.kind,
            self.descriptor.version,
            self.path.display()
        )
    }
}

/// Registers a loaded library's stage, so the palette lists it and a saved
/// pipeline naming its kind can be run.
pub fn register(
    registry: &mut StageRegistry,
    library: Arc<ExtLibrary>,
) -> Result<(), RegistryError> {
    let descriptor = library.descriptor();
    registry.register_loaded(
        descriptor,
        Arc::new(move || Box::new(ExtStage::new(Arc::clone(&library)))),
    )
}

/// Resolves every symbol the ABI names, failing on the first one missing —
/// which is the useful message: a library missing `sp_process` is not an
/// external stage at all, whatever else it exports.
fn resolve(source: &dyn SymbolSource, path: &Path) -> Result<Vtable> {
    let mut found = Vec::with_capacity(abi::SYMBOLS.len());
    for symbol in abi::SYMBOLS {
        let address = source
            .symbol(symbol)
            .filter(|address| !address.is_null())
            .ok_or_else(|| ExtError::MissingSymbol {
                path: path.to_path_buf(),
                symbol: (*symbol).to_owned(),
            })?;
        found.push(address);
    }

    // SAFETY: each address is the entry point the library exported under the
    // matching name in `abi::SYMBOLS`, and the signatures are the ones the
    // header in §9.9 declares. A library that exports the name with another
    // signature is undefined behaviour and cannot be detected from here —
    // which is why loading one is a deliberate, allow-listed act.
    unsafe {
        Ok(Vtable {
            abi_version: std::mem::transmute::<*const c_void, abi::AbiVersionFn>(found[0]),
            describe: std::mem::transmute::<*const c_void, abi::DescribeFn>(found[1]),
            open: std::mem::transmute::<*const c_void, abi::OpenFn>(found[2]),
            begin_run: std::mem::transmute::<*const c_void, abi::BeginRunFn>(found[3]),
            process: std::mem::transmute::<*const c_void, abi::ProcessFn>(found[4]),
            end_run: std::mem::transmute::<*const c_void, abi::EndRunFn>(found[5]),
            free_output: std::mem::transmute::<*const c_void, abi::FreeOutputFn>(found[6]),
            close: std::mem::transmute::<*const c_void, abi::CloseFn>(found[7]),
        })
    }
}

/// The address of the library file's contents, streamed rather than read whole
/// — a vendor DLL with its debug information can be hundreds of megabytes.
fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path).map_err(|source| ExtError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    std::io::copy(&mut file, &mut hasher).map_err(|source| ExtError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// A NUL-terminated copy of `text`, with any interior NUL dropped: a JSON
/// document cannot contain one, and refusing the whole call over it would be
/// a worse answer than sending the string.
pub(crate) fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Empty;

    // SAFETY: it never hands back an address at all.
    unsafe impl SymbolSource for Empty {
        fn symbol(&self, _name: &str) -> Option<*const c_void> {
            None
        }
    }

    #[test]
    fn a_library_that_is_not_on_the_list_is_not_loaded() {
        let error = ExtLibrary::open("vendor.dll", &AllowList::new()).unwrap_err();
        assert!(matches!(error, ExtError::NotAllowed(_)), "{error}");
    }

    #[test]
    fn an_approved_library_that_is_missing_says_so_rather_than_failing_to_load() {
        let error = ExtLibrary::open("absent.dll", &AllowList::unrestricted()).unwrap_err();
        assert!(matches!(error, ExtError::NotFound(_)), "{error}");
    }

    #[test]
    fn a_library_without_the_abi_symbols_is_refused_by_name() {
        let error =
            ExtLibrary::from_source(Box::new(Empty), PathBuf::from("empty.dll"), String::new())
                .unwrap_err();
        let ExtError::MissingSymbol { symbol, .. } = &error else {
            panic!("expected a missing symbol, got {error}");
        };
        assert_eq!(symbol, "sp_abi_version");
    }

    #[test]
    fn a_file_that_is_not_a_library_fails_to_load_rather_than_being_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vendor.dll");
        std::fs::write(&path, b"MZ but not really").unwrap();
        let error = ExtLibrary::open(&path, &AllowList::unrestricted()).unwrap_err();
        assert!(matches!(error, ExtError::Load { .. }), "{error}");
    }

    #[test]
    fn an_interior_nul_does_not_sink_the_call() {
        assert_eq!(c_string("a\0b").to_str().unwrap(), "ab");
    }
}
