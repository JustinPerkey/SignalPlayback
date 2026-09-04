//! Platform-correct default locations (`docs/DESIGN.md` §5.1).

use std::path::PathBuf;

use directories::BaseDirs;

/// Directory holding all application data, e.g.
/// `%LOCALAPPDATA%\SignalPlayback` on Windows.
///
/// `None` when the platform has no home directory to derive it from, in which
/// case the app runs with no library and logs to stderr only.
#[must_use]
pub fn app_data_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| dirs.data_local_dir().join("SignalPlayback"))
}

/// Default library root. The root is user-selectable; this is only where a
/// first run looks.
#[must_use]
pub fn default_library_root() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join("library"))
}

/// Directory for the rolling application log.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join("logs"))
}
