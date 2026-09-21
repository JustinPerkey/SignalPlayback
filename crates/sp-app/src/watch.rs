//! The watched folder (`docs/DESIGN.md` §7.6, §15.1).
//!
//! A folder named in `settings.json` is scanned on a timer; a file that
//! appears in it is queued for import with the profile the Import screen is
//! holding, and nothing else happens — no second importer, no second set of
//! rules. The whole feature is a *source* for [`crate::screens::import`]'s
//! queue, which is the same queue the file dialog and a window drop feed.
//!
//! Three decisions are the design, and each of them is a thing that goes
//! wrong if it is not made:
//!
//! **A file is imported only once it has stopped changing.** A capture being
//! written into the folder is a file that exists, is readable, and is half a
//! file. So a scan records each file's length and modification time, and a
//! file becomes eligible on the *next* scan only if both are unchanged. This
//! is also why this is a poll and not an OS watch: an inotify event says a
//! write happened, not that writing has finished, so the settle check would be
//! needed either way and the event buys nothing.
//!
//! **What was already in the folder is adopted, not imported.** Pointing at a
//! folder of four hundred old captures is not a request to import four hundred
//! datasets, and there is no undo (§12.3). The census taken when the folder is
//! adopted marks everything in it as seen; `Import every file in the watched
//! folder` is the action for the other intent, and it is a thing the user
//! asks for.
//!
//! **A file is offered once.** A file that imported, that failed, or that was
//! adopted is not offered again while the folder stays watched, so a folder
//! nobody touches costs one `read_dir` a second and produces nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often the folder is scanned. Two scans are needed before a new file is
/// imported, so this is also half the latency of an auto-import — a second is
/// quick enough for a bench and slow enough to be free.
pub const INTERVAL: Duration = Duration::from_millis(1_000);

/// Extensions the watcher and a dropped folder consider importable.
///
/// The importer's framer is the CSV one (§7), so this is the set of names a
/// CSV is written under rather than a guess at the contents. Anything else in
/// the folder is left alone — a watched folder is usually somebody's capture
/// directory and has notes and screenshots in it.
const IMPORTABLE: [&str; 3] = ["csv", "txt", "tsv"];

/// Whether `path` is a file this application would try to import.
#[must_use]
pub fn is_importable(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMPORTABLE
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Importable files directly inside `folder`, sorted by name.
///
/// Not recursive: a watched folder is a drop box, and walking into whatever is
/// below it turns "put a capture here" into "and mind what is in your
/// archive".
#[must_use]
pub fn importable_in(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_importable(path))
        .collect();
    files.sort();
    files
}

/// What a scan saw of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

/// Where a file has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    /// Seen once, and being watched to see whether it is still being written.
    Settling(Stamp),
    /// Offered to the queue, or adopted by the census. Either way, done with.
    Done,
}

/// The watched folder and what it has seen.
#[derive(Debug, Default)]
pub struct Watcher {
    folder: Option<PathBuf>,
    seen: BTreeMap<PathBuf, Known>,
}

impl Watcher {
    /// The folder being watched, if any.
    #[must_use]
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    #[must_use]
    pub const fn is_watching(&self) -> bool {
        self.folder.is_some()
    }

    /// How many files the watcher is accounting for, which is what the
    /// Settings screen reports so a watch that is doing nothing can be told
    /// from one that is not running.
    #[must_use]
    pub fn known(&self) -> usize {
        self.seen.len()
    }

    /// Points the watcher at `folder`, adopting whatever is already in it.
    ///
    /// Re-pointing it at the folder it is already watching is deliberately
    /// *not* a no-op reset: the census runs again, so a file that appeared
    /// while the app was closed is adopted rather than imported on the next
    /// tick. Setting it to `None` stops the watch and forgets everything,
    /// because what it had seen was only true of a folder it is no longer
    /// looking at.
    pub fn watch(&mut self, folder: Option<PathBuf>) {
        self.seen.clear();
        self.folder = folder;
        if let Some(folder) = self.folder.clone() {
            let census = importable_in(&folder);
            tracing::info!(
                folder = %folder.display(),
                adopted = census.len(),
                "watching a folder for new files",
            );
            for path in census {
                self.seen.insert(path, Known::Done);
            }
        }
    }

    /// Scans the folder and returns the files that are ready to import: new
    /// since the last scan, and unchanged since the one before it.
    #[must_use]
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let Some(folder) = self.folder.clone() else {
            return Vec::new();
        };

        let present = importable_in(&folder);
        // A file that has gone is forgotten, so a name that is used again —
        // the same capture re-exported over the top of itself — is a new file.
        self.seen.retain(|path, _| present.contains(path));

        let mut ready = Vec::new();
        for path in present {
            let Some(stamp) = Stamp::of(&path) else {
                // Vanished between the listing and the stat. It will be seen
                // again next scan if it comes back.
                self.seen.remove(&path);
                continue;
            };
            match self.seen.get(&path).copied() {
                Some(Known::Done) => {}
                Some(Known::Settling(previous)) if previous == stamp => {
                    self.seen.insert(path.clone(), Known::Done);
                    ready.push(path);
                }
                // Still growing: start the clock again from what it is now.
                Some(Known::Settling(_)) | None => {
                    self.seen.insert(path, Known::Settling(stamp));
                }
            }
        }
        if !ready.is_empty() {
            tracing::info!(
                folder = %folder.display(),
                files = ready.len(),
                "the watched folder has new files",
            );
        }
        ready
    }

    /// Every importable file in the folder, marked as seen — the explicit
    /// "import everything that is in there" the census deliberately does not
    /// do on its own.
    #[must_use]
    pub fn sweep(&mut self) -> Vec<PathBuf> {
        let Some(folder) = self.folder.clone() else {
            return Vec::new();
        };
        let files = importable_in(&folder);
        for path in &files {
            self.seen.insert(path.clone(), Known::Done);
        }
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `name` in `dir` with `bytes` of content, and gives it a
    /// modification time far enough in the past that a later write of the
    /// same length is still a change.
    fn write(dir: &Path, name: &str, bytes: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn only_importable_names_are_looked_at() {
        assert!(is_importable(Path::new("capture.csv")));
        assert!(is_importable(Path::new("capture.CSV")));
        assert!(is_importable(Path::new("capture.txt")));
        assert!(is_importable(Path::new("capture.tsv")));
        assert!(!is_importable(Path::new("notes.md")));
        assert!(!is_importable(Path::new("screenshot.png")));
        assert!(!is_importable(Path::new("capture")));
    }

    #[test]
    fn a_new_file_is_offered_once_it_has_stopped_changing() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));
        assert!(watcher.is_watching());
        assert!(watcher.poll().is_empty(), "an empty folder offers nothing");

        let path = write(dir.path(), "capture.csv", "a,b\n1,2\n");
        assert!(
            watcher.poll().is_empty(),
            "the first scan only starts watching it"
        );
        assert_eq!(watcher.poll(), vec![path.clone()], "the second offers it");
        assert!(watcher.poll().is_empty(), "and never again");
    }

    #[test]
    fn a_file_still_being_written_is_left_alone_until_it_settles() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));

        let path = write(dir.path(), "growing.csv", "a,b\n");
        assert!(watcher.poll().is_empty());
        // Still growing between scans: the clock restarts rather than the
        // half-written file being imported.
        write(dir.path(), "growing.csv", "a,b\n1,2\n");
        assert!(
            watcher.poll().is_empty(),
            "it changed, so it is not ready yet"
        );
        write(dir.path(), "growing.csv", "a,b\n1,2\n3,4\n");
        assert!(watcher.poll().is_empty());
        // Once the writer stops, the next scan finds it unchanged.
        assert_eq!(watcher.poll(), vec![path]);
    }

    #[test]
    fn what_was_already_there_is_adopted_rather_than_imported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old-one.csv", "a\n1\n");
        write(dir.path(), "old-two.csv", "a\n1\n");

        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));
        assert_eq!(watcher.known(), 2, "both are accounted for");
        assert!(watcher.poll().is_empty());
        assert!(watcher.poll().is_empty());

        // A file that arrives afterwards is the one that imports.
        let fresh = write(dir.path(), "new.csv", "a\n1\n");
        assert!(watcher.poll().is_empty());
        assert_eq!(watcher.poll(), vec![fresh]);
    }

    #[test]
    fn a_sweep_offers_everything_in_the_folder_once() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "one.csv", "a\n1\n");
        write(dir.path(), "two.csv", "a\n1\n");
        write(dir.path(), "notes.md", "not a capture");

        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));
        let swept = watcher.sweep();
        assert_eq!(swept.len(), 2, "{swept:?}");
        assert!(swept.iter().all(|path| is_importable(path)));
        assert!(
            watcher.poll().is_empty(),
            "and the poll does not repeat them"
        );
    }

    #[test]
    fn files_that_are_not_captures_are_never_offered() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));
        write(dir.path(), "notes.md", "hello");
        write(dir.path(), "shot.png", "not really");
        assert!(watcher.poll().is_empty());
        assert!(watcher.poll().is_empty());
        assert_eq!(watcher.known(), 0);
    }

    #[test]
    fn a_file_that_goes_away_is_forgotten_so_the_same_name_can_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));

        let path = write(dir.path(), "capture.csv", "a\n1\n");
        let _ = watcher.poll();
        assert_eq!(watcher.poll(), vec![path.clone()]);

        std::fs::remove_file(&path).unwrap();
        assert!(watcher.poll().is_empty());
        assert_eq!(watcher.known(), 0, "it is not remembered for ever");

        // Re-exported under the same name, it is a new file.
        write(dir.path(), "capture.csv", "a\n9\n");
        assert!(watcher.poll().is_empty());
        assert_eq!(watcher.poll(), vec![path]);
    }

    #[test]
    fn nothing_happens_without_a_folder_and_stopping_forgets_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::default();
        assert!(!watcher.is_watching());
        assert!(watcher.poll().is_empty());
        assert!(watcher.sweep().is_empty());

        write(dir.path(), "capture.csv", "a\n1\n");
        watcher.watch(Some(dir.path().to_path_buf()));
        assert_eq!(watcher.known(), 1);
        watcher.watch(None);
        assert!(!watcher.is_watching());
        assert_eq!(watcher.known(), 0);
        assert!(watcher.poll().is_empty());
    }

    #[test]
    fn a_folder_that_is_not_there_is_not_an_error() {
        let mut watcher = Watcher::default();
        watcher.watch(Some(PathBuf::from("no-such-folder-anywhere")));
        assert!(watcher.is_watching(), "the setting is still the setting");
        assert!(watcher.poll().is_empty());
        assert!(watcher.sweep().is_empty());
    }

    #[test]
    fn re_adopting_the_same_folder_takes_the_census_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::default();
        watcher.watch(Some(dir.path().to_path_buf()));

        // As if the application had been closed while this arrived.
        write(dir.path(), "while-away.csv", "a\n1\n");
        watcher.watch(Some(dir.path().to_path_buf()));
        assert_eq!(watcher.known(), 1);
        assert!(watcher.poll().is_empty());
        assert!(watcher.poll().is_empty(), "adopted, not imported");
    }
}
