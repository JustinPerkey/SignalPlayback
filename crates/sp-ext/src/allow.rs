//! The list of libraries this installation is willing to load
//! (`docs/DESIGN.md` §9.9, §12.4).
//!
//! Loading a native library is running arbitrary code in the app's address
//! space. There is no sandbox to hide behind in M9, so the mitigation is
//! consent: a library runs because the user added *that file* on the settings
//! screen, never because it happened to be in a directory that got scanned.
//!
//! The list holds absolute paths, canonicalised, so a symlink or a `..` cannot
//! smuggle in a different file than the one that was approved.

use std::path::{Path, PathBuf};

/// The library paths a user has approved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    paths: Vec<PathBuf>,
}

impl AllowList {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A list that permits everything, for a headless run whose configuration
    /// *is* the consent — the CLI naming a library on the command line, and
    /// the tests in this crate.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            paths: vec![PathBuf::new()],
        }
    }

    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.paths.iter().any(|path| path.as_os_str().is_empty())
    }

    /// Approves one library. Canonicalising here rather than at check time
    /// means the entry is stored as the file the user actually picked.
    pub fn allow(&mut self, path: impl AsRef<Path>) {
        let path = canonical(path.as_ref());
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }

    #[must_use]
    pub fn allowing(mut self, path: impl AsRef<Path>) -> Self {
        self.allow(path);
        self
    }

    pub fn remove(&mut self, path: impl AsRef<Path>) {
        let path = canonical(path.as_ref());
        self.paths.retain(|entry| entry != &path);
    }

    #[must_use]
    pub fn permits(&self, path: impl AsRef<Path>) -> bool {
        if self.is_unrestricted() {
            return true;
        }
        let path = canonical(path.as_ref());
        self.paths.contains(&path)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Path> {
        self.paths.iter().map(PathBuf::as_path)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

impl FromIterator<PathBuf> for AllowList {
    fn from_iter<T: IntoIterator<Item = PathBuf>>(iter: T) -> Self {
        let mut list = Self::new();
        for path in iter {
            list.allow(path);
        }
        list
    }
}

/// The path as the filesystem sees it, or the path as given when it cannot be
/// resolved — a listed library that has since been deleted still shows in
/// settings, and fails at load time with a message about the file rather than
/// about the list.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_permitted_by_default() {
        assert!(!AllowList::new().permits("vendor.dll"));
    }

    #[test]
    fn an_approved_library_is_permitted_and_can_be_withdrawn() {
        let mut list = AllowList::new();
        list.allow("vendor.dll");
        assert!(list.permits("vendor.dll"));
        list.remove("vendor.dll");
        assert!(!list.permits("vendor.dll"));
        assert!(list.is_empty());
    }

    #[test]
    fn approving_the_same_library_twice_lists_it_once() {
        let list = AllowList::new()
            .allowing("vendor.dll")
            .allowing("vendor.dll");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn a_relative_detour_resolves_to_the_same_approved_file() {
        // './x/../vendor.dll' is 'vendor.dll'; the list must not be fooled by
        // the spelling.
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("vendor.dll");
        std::fs::write(&library, b"not really a library").unwrap();

        let list = AllowList::new().allowing(&library);
        let detour = dir.path().join("sub").join("..").join("vendor.dll");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(list.permits(detour));
    }

    #[test]
    fn an_unrestricted_list_permits_anything() {
        assert!(AllowList::unrestricted().permits("anything.dll"));
    }
}
