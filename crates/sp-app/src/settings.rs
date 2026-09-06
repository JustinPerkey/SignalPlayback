//! User settings and the file they live in (`docs/DESIGN.md` §12.1).
//!
//! Settings are *not* in the library: one of them is which library to open, and
//! the theme should follow the user rather than the file they happen to have
//! open. They live in `settings.json` beside the log directory, in the same
//! application data directory the default library sits under.
//!
//! Every field is optional on the way in ([`serde(default)`]) and validated on
//! the way out of the file, so a hand-edited or older settings file loads with
//! the fields it does understand and defaults for the rest rather than
//! refusing to open. Nothing here is fatal: a settings file that cannot be
//! read or written is logged and the defaults are used, because losing a
//! preference must never cost the user their application.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sp_csv::profile::CountMode;
use sp_engine::reduce::Quality;

/// Bins the Inspector's histogram is drawn with.
pub const MIN_BINS: usize = 8;
pub const MAX_BINS: usize = 256;
pub const DEFAULT_BINS: usize = 64;

/// Sample rate a new generator spec starts at.
pub const DEFAULT_SAMPLE_RATE_HZ: f64 = 48_000.0;

/// Which theme the window opens in. Iced's `Theme` is not serialisable and
/// carries far more variants than the two the app offers (§12.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

impl ThemeChoice {
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }

    #[must_use]
    pub fn theme(self) -> iced::Theme {
        match self {
            Self::Dark => iced::Theme::Dark,
            Self::Light => iced::Theme::Light,
        }
    }

    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

/// How many runs of a pipeline are kept when it finishes another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Never delete a run. Runs are the record of what the algorithm did, and
    /// the default is to keep that record.
    #[default]
    KeepAll,
    /// Keep this many finished runs per pipeline, oldest deleted first. A run
    /// a baseline names is always kept (§10.4).
    Keep(usize),
}

impl Retention {
    /// The run limit, or `None` when everything is kept.
    #[must_use]
    pub const fn limit(self) -> Option<usize> {
        match self {
            Self::KeepAll => None,
            Self::Keep(n) => Some(n),
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::KeepAll => "Keep every run".to_owned(),
            Self::Keep(1) => "Keep the newest run".to_owned(),
            Self::Keep(n) => format!("Keep the newest {n} runs"),
        }
    }

    /// The choices the Settings screen offers.
    #[must_use]
    pub fn choices() -> Vec<Self> {
        vec![Self::KeepAll, Self::Keep(50), Self::Keep(20), Self::Keep(5)]
    }
}

/// Everything the Settings screen edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The library to open, or `None` for the default location.
    pub library_file: Option<PathBuf>,
    pub theme: ThemeChoice,
    /// Sample rate a new generator spec starts at (§8.1).
    pub default_sample_rate_hz: f64,
    /// Whether an import refuses a file whose counts do not add up (§7.3).
    pub import_mode: CountMode,
    /// What happens to old runs when a new one finishes (§9.6).
    pub retention: Retention,
    /// How hard the scope's reducer works per frame (§11.4).
    pub decimation: Quality,
    /// Bins in the Inspector's histogram.
    pub histogram_bins: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            library_file: None,
            theme: ThemeChoice::default(),
            default_sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            import_mode: CountMode::default(),
            retention: Retention::default(),
            decimation: Quality::default(),
            histogram_bins: DEFAULT_BINS,
        }
    }
}

impl Settings {
    /// `settings.json` inside `dir`.
    #[must_use]
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    /// Reads the settings file, falling back to the defaults for anything it
    /// does not hold — including the file not existing at all, which is what a
    /// first run sees.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(settings) => settings.validated(),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "settings file is not readable; using defaults");
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not read the settings file");
                Self::default()
            }
        }
    }

    /// Writes the settings to `path`, creating the directory if needed.
    ///
    /// The write goes to a temporary file first and is then renamed over the
    /// old one: a crash mid-write leaves the previous settings intact rather
    /// than a truncated file the next run cannot parse.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.clone().validated())
            .map_err(std::io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, json.as_bytes())?;
        std::fs::rename(&temporary, path)
    }

    /// Clamps anything a hand-edited file could have put out of range. A bad
    /// value is corrected rather than rejected: the point is to open.
    #[must_use]
    pub fn validated(mut self) -> Self {
        if !self.default_sample_rate_hz.is_finite() || self.default_sample_rate_hz <= 0.0 {
            self.default_sample_rate_hz = DEFAULT_SAMPLE_RATE_HZ;
        }
        self.histogram_bins = self.histogram_bins.clamp(MIN_BINS, MAX_BINS);
        if let Retention::Keep(0) = self.retention {
            // Keeping nothing would delete a run the moment it finished, which
            // no user means by "retention".
            self.retention = Retention::Keep(1);
        }
        self
    }

    /// The library file to open: the chosen one, or the default location.
    #[must_use]
    pub fn library_file(&self, default: Option<PathBuf>) -> Option<PathBuf> {
        self.library_file.clone().or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_settings_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(&Settings::path_in(dir.path()));
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.histogram_bins, DEFAULT_BINS);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = Settings::path_in(dir.path());
        let settings = Settings {
            library_file: Some(PathBuf::from("/tmp/other.db")),
            theme: ThemeChoice::Light,
            default_sample_rate_hz: 1_000_000.0,
            import_mode: CountMode::Strict,
            retention: Retention::Keep(5),
            decimation: Quality::Fine,
            histogram_bins: 128,
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        // And the temporary file did not survive the rename.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn an_older_file_loads_with_defaults_for_what_it_lacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = Settings::path_in(dir.path());
        std::fs::write(&path, r#"{"theme":"light"}"#).unwrap();
        let settings = Settings::load(&path);
        assert_eq!(settings.theme, ThemeChoice::Light);
        assert_eq!(settings.decimation, Quality::Balanced);
        assert_eq!(settings.default_sample_rate_hz, DEFAULT_SAMPLE_RATE_HZ);
    }

    #[test]
    fn an_unreadable_file_does_not_stop_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = Settings::path_in(dir.path());
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn out_of_range_values_are_clamped_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = Settings::path_in(dir.path());
        std::fs::write(
            &path,
            r#"{"default_sample_rate_hz":-5.0,"histogram_bins":100000,
                "retention":{"keep":0}}"#,
        )
        .unwrap();
        let settings = Settings::load(&path);
        assert_eq!(settings.default_sample_rate_hz, DEFAULT_SAMPLE_RATE_HZ);
        assert_eq!(settings.histogram_bins, MAX_BINS);
        assert_eq!(settings.retention, Retention::Keep(1));
    }

    #[test]
    fn the_library_path_falls_back_to_the_default_location() {
        let default = Some(PathBuf::from("/data/library.db"));
        assert_eq!(
            Settings::default().library_file(default.clone()),
            default.clone()
        );
        let chosen = Settings {
            library_file: Some(PathBuf::from("/elsewhere/lib.db")),
            ..Settings::default()
        };
        assert_eq!(
            chosen.library_file(default),
            Some(PathBuf::from("/elsewhere/lib.db"))
        );
    }

    #[test]
    fn retention_labels_read_as_sentences() {
        assert_eq!(Retention::KeepAll.limit(), None);
        assert_eq!(Retention::Keep(20).limit(), Some(20));
        assert_eq!(Retention::Keep(1).label(), "Keep the newest run");
        assert_eq!(Retention::Keep(5).label(), "Keep the newest 5 runs");
        assert!(Retention::choices().contains(&Retention::KeepAll));
    }
}
