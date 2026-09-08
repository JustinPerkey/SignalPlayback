//! The stage registry the application runs with: the built-ins, plus every
//! external library the user has allowed (`docs/DESIGN.md` §9.9, §12.4).
//!
//! Loading happens here rather than in each screen for two reasons. A library
//! is loaded **once per process** — its descriptor is `'static` data and its
//! code is mapped into this address space, so opening the same file twice
//! would map it twice and leak a second descriptor. And a failure to load is a
//! thing to *show* the user on the Settings screen, next to the path they
//! added, rather than a startup panic: the rest of the app works perfectly
//! well without a vendor DLL that has gone missing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use sp_ext::{AllowList, ExtLibrary};
use sp_proc::StageRegistry;

/// Libraries already loaded in this process, by the path they were loaded
/// from.
fn loaded() -> &'static Mutex<BTreeMap<PathBuf, Arc<ExtLibrary>>> {
    static LOADED: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ExtLibrary>>>> = OnceLock::new();
    LOADED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Opens one allowed library, or hands back the one already open for it.
///
/// The allow-list *is* the settings list: a path is here because the user put
/// it here, which is the consent §9.9 requires before code is mapped in.
pub fn open(path: &Path, allowed: &[PathBuf]) -> Result<Arc<ExtLibrary>, String> {
    let mut open = loaded()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(library) = open.get(path) {
        return Ok(Arc::clone(library));
    }
    let list: AllowList = allowed.iter().cloned().collect();
    let library = ExtLibrary::open(path, &list).map_err(|error| error.to_string())?;
    open.insert(path.to_path_buf(), Arc::clone(&library));
    Ok(library)
}

/// The registry every screen and every run works from, and one message per
/// library that could not be loaded.
///
/// A library that fails is skipped, not fatal: a pipeline that names its stage
/// then fails validation with "no stage is registered under that kind", which
/// is the truthful report.
#[must_use]
pub fn registry(libraries: &[PathBuf]) -> (StageRegistry, Vec<String>) {
    let mut errors = Vec::new();
    let mut registry = sp_dsp::registry().unwrap_or_else(|error| {
        tracing::error!(%error, "a built-in stage is mis-declared");
        errors.push(format!("A built-in stage is mis-declared: {error}"));
        StageRegistry::new()
    });

    for path in libraries {
        match open(path, libraries).and_then(|library| {
            sp_ext::register(&mut registry, library).map_err(|error| error.to_string())
        }) {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "an external stage library was not loaded");
                errors.push(format!("{}: {error}", path.display()));
            }
        }
    }
    (registry, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_libraries_the_registry_is_the_built_ins() {
        let (registry, errors) = registry(&[]);
        assert!(errors.is_empty());
        assert!(registry.contains("dsp.condition.gain"));
    }

    #[test]
    fn a_library_that_is_not_there_is_reported_rather_than_fatal() {
        let missing = PathBuf::from("no-such-vendor-library.dll");
        let (registry, errors) = registry(std::slice::from_ref(&missing));
        assert!(
            registry.contains("dsp.condition.gain"),
            "the built-ins are still registered"
        );
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("no-such-vendor-library.dll"),
            "{}",
            errors[0]
        );
    }
}
