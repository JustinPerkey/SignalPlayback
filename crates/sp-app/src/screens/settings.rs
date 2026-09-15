//! The Settings screen (`docs/DESIGN.md` §12.1): where the library lives, how
//! the window looks, and the defaults the other screens open with.
//!
//! Every control here writes [`crate::settings::Settings`] to disk as soon as
//! it changes — there is no Save button, because there is nothing here that is
//! only half-decided. The root application applies each change to the screen
//! that cares about it (§12.2), so switching decimation quality redraws the
//! scope now rather than after a restart.
//!
//! The screen also reports what the open library holds, which is the one place
//! in the application that answers "what is this file made of?" (§15.6).

use std::fmt;
use std::path::PathBuf;

use iced::widget::{
    button, column, container, pick_list, row, scrollable, text, text_input, Space,
};
use iced::{Alignment, Element, Length, Task};
use sp_csv::profile::CountMode;
use sp_engine::reduce::Quality;
use sp_store::stats::{self, StorageStats};
use sp_store::{LibrarySummary, Store};

use crate::actions::Action;
use crate::jobs;
use crate::keymap::Chord;
use crate::settings::{Retention, SampleCap, Settings, ThemeChoice, MAX_BINS, MIN_BINS};
use crate::typography;
use crate::ui;

/// A pick-list entry that knows how to print itself.
macro_rules! choice {
    ($name:ident, $inner:ty, $label:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let label: fn($inner) -> String = $label;
                f.write_str(&label(self.0))
            }
        }
    };
}

choice!(ThemeEntry, ThemeChoice, |theme: ThemeChoice| theme
    .label()
    .to_owned());
choice!(ModeEntry, CountMode, |mode: CountMode| match mode {
    CountMode::Tolerant => "Tolerant — warn and carry on".to_owned(),
    CountMode::Strict => "Strict — refuse the file".to_owned(),
});
choice!(QualityEntry, Quality, |quality: Quality| match quality {
    Quality::Fast => "Fast — half the detail, half the read".to_owned(),
    Quality::Balanced => "Balanced — about one cell per pixel".to_owned(),
    Quality::Fine => "Fine — twice the detail per pixel".to_owned(),
});
choice!(RetentionEntry, Retention, Retention::label);
choice!(CapEntry, SampleCap, SampleCap::label);

#[derive(Debug, Default)]
pub struct State {
    settings: Settings,
    /// Where the settings file is, when there is an application data
    /// directory to hold one.
    path: Option<PathBuf>,
    /// The default library path, shown when nothing else is chosen.
    default_library: Option<PathBuf>,
    /// Raw text of the sample-rate field, so a half-typed number is not
    /// rewritten under the cursor.
    rate_draft: String,
    rate_error: Option<String>,
    storage: Option<(LibrarySummary, StorageStats)>,
    storage_error: Option<String>,
    notice: Option<String>,
    error: Option<String>,
    /// Set when a control changed the settings; the root takes it, applies
    /// them and writes the file.
    changed: bool,
    /// A library the user picked, waiting for the root to open it. The inner
    /// `None` means "go back to the default location".
    library_request: Option<Option<PathBuf>>,
    /// The action whose key is being captured, if the user is holding the
    /// keyboard open waiting to press one.
    rebinding: Option<Action>,
}

#[derive(Debug, Clone)]
pub enum Message {
    ThemePicked(ThemeEntry),
    ModePicked(ModeEntry),
    QualityPicked(QualityEntry),
    RetentionPicked(RetentionEntry),
    CapPicked(CapEntry),
    RateChanged(String),
    RateCommitted,
    ChooseLibrary,
    LibraryChosen(Option<PathBuf>),
    UseDefaultLibrary,
    Refresh,
    Storage(Result<(LibrarySummary, StorageStats), String>),
    Sweep,
    Swept(Result<usize, String>),
    ClearCache,
    CacheCleared(Result<usize, String>),
    UseHistogramBins(usize),
    AddExternalLibrary,
    ExternalLibraryChosen(Option<PathBuf>),
    RemoveExternalLibrary(PathBuf),

    ChooseWatchFolder,
    WatchFolderChosen(Option<PathBuf>),
    StopWatching,

    /// Start listening for the chord to bind to this action.
    Rebind(Action),
    /// The chord that was pressed while listening; the root hands it over.
    BindCaptured(Chord),
    StopRebinding,
    Unbind(Action),
    ResetKeymap,
}

impl State {
    /// Adopts the settings the application booted with.
    pub fn adopt(
        &mut self,
        settings: Settings,
        path: Option<PathBuf>,
        default_library: Option<PathBuf>,
    ) {
        self.rate_draft = crate::screens::library::fmt_num(settings.default_sample_rate_hz);
        self.settings = settings;
        self.path = path;
        self.default_library = default_library;
    }

    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Reads the library's counts and storage figures.
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        Task::perform(
            jobs::read(store.clone(), |conn| {
                Ok((sp_store::library::summary(conn)?, stats::storage(conn)?))
            }),
            Message::Storage,
        )
    }

    /// Whether the last message changed the settings; the root reads this to
    /// apply them and write the file.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// The library the user has asked to open, once.
    pub fn take_library_request(&mut self) -> Option<Option<PathBuf>> {
        self.library_request.take()
    }

    /// The action waiting for a key, if any.
    ///
    /// The root reads it to know that the next key press is a *binding* and
    /// not a shortcut — which is the only way to bind `Ctrl`+`1` to something
    /// else without the press navigating away first.
    #[must_use]
    pub const fn rebinding(&self) -> Option<Action> {
        self.rebinding
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::ThemePicked(ThemeEntry(theme)) => {
                self.settings.theme = theme;
                self.changed = true;
                Task::none()
            }
            Message::ModePicked(ModeEntry(mode)) => {
                self.settings.import_mode = mode;
                self.changed = true;
                Task::none()
            }
            Message::QualityPicked(QualityEntry(quality)) => {
                self.settings.decimation = quality;
                self.changed = true;
                Task::none()
            }
            Message::RetentionPicked(RetentionEntry(retention)) => {
                self.settings.retention = retention;
                self.changed = true;
                Task::none()
            }
            Message::CapPicked(CapEntry(cap)) => {
                self.settings.sample_cap = cap;
                self.changed = true;
                Task::none()
            }
            Message::RateChanged(text) => {
                self.rate_draft = text;
                Task::none()
            }
            Message::RateCommitted => {
                match self.rate_draft.trim().parse::<f64>() {
                    Ok(hz) if hz.is_finite() && hz > 0.0 => {
                        self.settings.default_sample_rate_hz = hz;
                        self.rate_error = None;
                        self.changed = true;
                    }
                    _ => {
                        self.rate_error =
                            Some("A sample rate is a positive number of hertz.".to_owned());
                    }
                }
                Task::none()
            }
            Message::UseHistogramBins(bins) => {
                self.settings.histogram_bins = bins.clamp(MIN_BINS, MAX_BINS);
                self.changed = true;
                Task::none()
            }
            Message::ChooseLibrary => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Open a SignalPlayback library")
                        .add_filter("SignalPlayback library", &["db"])
                        .pick_file()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::LibraryChosen,
            ),
            Message::LibraryChosen(path) => {
                if let Some(path) = path {
                    self.settings.library_file = Some(path.clone());
                    self.changed = true;
                    self.library_request = Some(Some(path));
                }
                Task::none()
            }
            Message::UseDefaultLibrary => {
                self.settings.library_file = None;
                self.changed = true;
                self.library_request = Some(None);
                Task::none()
            }
            Message::AddExternalLibrary => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Allow a native stage library")
                        .add_filter("Native library", &["dll", "so", "dylib"])
                        .pick_file()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::ExternalLibraryChosen,
            ),
            Message::ExternalLibraryChosen(path) => {
                let Some(path) = path else {
                    return Task::none();
                };
                if self.settings.external_libraries.contains(&path) {
                    self.notice = Some("That library is already allowed.".to_owned());
                    return Task::none();
                }
                // Loading is attempted now, in front of the user who chose the
                // file, rather than silently at the next run (§9.9).
                let mut allowed = self.settings.external_libraries.clone();
                allowed.push(path.clone());
                match crate::stages::open(&path, &allowed) {
                    Ok(library) => {
                        self.settings.external_libraries = allowed;
                        self.changed = true;
                        self.error = None;
                        self.notice = Some(format!(
                            "Loaded {} — it is now in the stage palette.",
                            library.descriptor().label
                        ));
                    }
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::RemoveExternalLibrary(path) => {
                self.settings
                    .external_libraries
                    .retain(|entry| entry != &path);
                self.changed = true;
                // The library stays mapped into this process until it exits;
                // what changes now is that nothing new will use it, and the
                // next start will not load it at all.
                self.notice = Some(
                    "Removed. The stage disappears from the palette when the app restarts."
                        .to_owned(),
                );
                Task::none()
            }
            Message::ChooseWatchFolder => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Watch a folder for new capture files")
                        .pick_folder()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::WatchFolderChosen,
            ),
            Message::WatchFolderChosen(folder) => {
                let Some(folder) = folder else {
                    return Task::none();
                };
                let already = crate::watch::importable_in(&folder).len();
                self.settings.watch_folder = Some(folder);
                self.changed = true;
                self.error = None;
                // Saying what will *not* happen is the point: the files
                // already in the folder are adopted, and a user who expected
                // four hundred imports should find that out here rather than
                // by watching them not happen (§7.6).
                self.notice = Some(if already == 0 {
                    "Watching. A capture written into this folder imports on its own.".to_owned()
                } else {
                    format!(
                        "Watching. The {already} file{} already there {} left alone; \
                         'Import every file in the watched folder' takes them.",
                        if already == 1 { "" } else { "s" },
                        if already == 1 { "is" } else { "are" },
                    )
                });
                Task::none()
            }
            Message::StopWatching => {
                if self.settings.watch_folder.take().is_some() {
                    self.changed = true;
                    self.notice = Some("Stopped watching.".to_owned());
                }
                Task::none()
            }

            Message::Rebind(action) => {
                self.rebinding = Some(action);
                self.notice = Some(format!(
                    "Press the keys for '{}'. Escape cancels.",
                    action.label()
                ));
                Task::none()
            }
            Message::StopRebinding => {
                self.rebinding = None;
                self.notice = None;
                Task::none()
            }
            Message::BindCaptured(chord) => {
                let Some(action) = self.rebinding.take() else {
                    return Task::none();
                };
                self.settings.keymap.bind(action, Some(chord));
                self.changed = true;
                let conflicts = self.settings.keymap.conflicts();
                // A chord bound twice is resolved, not refused: refusing it
                // would mean the user has to remember which of fifty actions
                // is holding the key they want. What they get instead is the
                // key and a line saying what it no longer reaches.
                self.notice = Some(
                    match conflicts
                        .iter()
                        .find(|(kept, shadowed)| *kept == action || *shadowed == action)
                    {
                        Some((kept, shadowed)) => format!(
                            "{} is now {} — which no longer reaches '{}'.",
                            chord.label(),
                            kept.label(),
                            shadowed.label(),
                        ),
                        None => format!("'{}' is now {}.", action.label(), chord.label()),
                    },
                );
                Task::none()
            }
            Message::Unbind(action) => {
                self.settings.keymap.bind(action, None);
                self.rebinding = None;
                self.changed = true;
                self.notice = Some(format!(
                    "'{}' has no key; it is still in the command palette.",
                    action.label()
                ));
                Task::none()
            }
            Message::ResetKeymap => {
                self.settings.keymap.reset();
                self.rebinding = None;
                self.changed = true;
                self.notice = Some("Every shortcut is back to its default.".to_owned());
                Task::none()
            }

            Message::Refresh => store.map_or_else(Task::none, |store| self.load(store)),
            Message::Storage(result) => {
                match result {
                    Ok(figures) => {
                        self.storage = Some(figures);
                        self.storage_error = None;
                    }
                    Err(error) => self.storage_error = Some(error),
                }
                Task::none()
            }
            Message::Sweep => {
                let Some(store) = store else {
                    self.error = Some("No library is open.".to_owned());
                    return Task::none();
                };
                Task::perform(
                    jobs::write(store.clone(), |conn| {
                        sp_store::blob::sweep_unreferenced(conn)
                    }),
                    Message::Swept,
                )
            }
            Message::ClearCache => {
                let Some(store) = store else {
                    self.error = Some("No library is open.".to_owned());
                    return Task::none();
                };
                // Only the keys go. The output they named belongs to the run
                // that recorded it and stays exactly where it was (§9.5).
                Task::perform(
                    jobs::write(store.clone(), |conn| sp_store::runs::clear_cache(conn)),
                    Message::CacheCleared,
                )
            }
            Message::CacheCleared(result) => {
                match result {
                    Ok(0) => self.notice = Some("The stage cache was already empty.".to_owned()),
                    Ok(n) => {
                        self.notice = Some(format!(
                            "Unpublished {n} stage cache {}. The next run recomputes; nothing \
                             recorded was deleted.",
                            if n == 1 { "key" } else { "keys" }
                        ));
                    }
                    Err(error) => self.error = Some(error),
                }
                store.map_or_else(Task::none, |store| self.load(store))
            }
            Message::Swept(result) => {
                match result {
                    Ok(0) => self.notice = Some("Nothing to reclaim.".to_owned()),
                    Ok(n) => {
                        self.notice = Some(format!(
                            "Reclaimed {n} unreferenced blob{}.",
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    Err(error) => self.error = Some(error),
                }
                store.map_or_else(Task::none, |store| self.load(store))
            }
        }
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let body = column![
            text("Settings")
                .size(typography::TITLE_SIZE)
                .font(typography::TITLE),
            text(match &self.path {
                Some(path) => format!("Saved to {}", path.display()),
                None =>
                    "This machine has no application data directory, so settings last only for \
                     this session."
                        .to_owned(),
            })
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT)
            .style(ui::dim),
            Space::with_height(Length::Fixed(18.0)),
            self.library_section(),
            section_rule(),
            self.appearance_section(),
            section_rule(),
            self.defaults_section(),
            section_rule(),
            self.ingest_section(),
            section_rule(),
            self.storage_section(),
            section_rule(),
            self.external_section(),
            section_rule(),
            self.keyboard_section(),
        ]
        .spacing(10)
        .max_width(760);

        container(scrollable(body))
            .padding([16, 20])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn library_section(&self) -> Element<'_, Message> {
        let current = self
            .settings
            .library_file
            .clone()
            .or_else(|| self.default_library.clone())
            .map_or_else(|| "none".to_owned(), |path| path.display().to_string());

        // Where the library is, is a path, so it is set as one: a path in the
        // prose face wraps in the wrong places and cannot be compared by eye
        // against another.
        let mut section = column![
            heading("Library"),
            text(current)
                .size(typography::BODY_SIZE)
                .font(typography::READOUT),
            text(if self.settings.library_file.is_some() {
                "Chosen library."
            } else {
                "The default location for this machine."
            })
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
        ]
        .spacing(4);

        let mut actions = row![command("Open a library…", Message::ChooseLibrary)].spacing(6);
        if self.settings.library_file.is_some() {
            actions = actions.push(command("Use the default", Message::UseDefaultLibrary));
        }
        section = section.push(actions);
        section.into()
    }

    /// The libraries this installation may load as external stages (§9.9).
    ///
    /// Worded as consent rather than configuration, because that is what it
    /// is: a library listed here runs its own code inside this process.
    fn external_section(&self) -> Element<'_, Message> {
        let mut section = column![
            heading("External stages"),
            text(
                "A native library listed here is loaded as a pipeline stage. Its code runs                  inside SignalPlayback, so add only libraries you trust."
            )
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
        ]
        .spacing(6);

        if self.settings.external_libraries.is_empty() {
            section = section.push(
                text("No external libraries are allowed.")
                    .size(typography::BODY_SIZE)
                    .style(ui::dim),
            );
        } else {
            for path in &self.settings.external_libraries {
                section = section.push(
                    row![
                        text(path.display().to_string())
                            .size(typography::BODY_SIZE)
                            .font(typography::READOUT)
                            .width(Length::Fill),
                        command("Remove", Message::RemoveExternalLibrary(path.clone())),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
            }
        }

        section
            .push(command("Allow a library…", Message::AddExternalLibrary))
            .into()
    }

    fn appearance_section(&self) -> Element<'_, Message> {
        column![
            heading("Appearance"),
            labelled(
                "Theme",
                pick_list(
                    ThemeChoice::ALL.map(ThemeEntry).to_vec(),
                    Some(ThemeEntry(self.settings.theme)),
                    Message::ThemePicked,
                )
                .text_size(typography::BODY_SIZE)
                .width(Length::Fixed(220.0))
                .into(),
            ),
            text("Ctrl+T switches theme from any screen.")
                .size(typography::LABEL_SIZE)
                .style(ui::dim),
        ]
        .spacing(6)
        .into()
    }

    fn defaults_section(&self) -> Element<'_, Message> {
        let mut rate_field = column![text_input("48000", &self.rate_draft)
            .on_input(Message::RateChanged)
            .on_submit(Message::RateCommitted)
            .size(typography::BODY_SIZE)
            .width(Length::Fixed(220.0))]
        .spacing(3);
        if let Some(error) = &self.rate_error {
            rate_field =
                rate_field.push(text(error).size(typography::LABEL_SIZE).style(text::danger));
        }

        let bins = self.settings.histogram_bins;
        let bin_buttons = row([16_usize, 32, 64, 128]
            .into_iter()
            .map(|choice| {
                // Four values of one setting, one of which is on. That is a
                // selection, so it is drawn as one — not as four filled
                // buttons with the chosen one filled louder.
                button(
                    text(choice.to_string())
                        .size(typography::BODY_SIZE)
                        .font(typography::READOUT),
                )
                .padding([4, 12])
                .style(ui::selectable(choice == bins))
                .on_press(Message::UseHistogramBins(choice))
                .into()
            })
            .collect::<Vec<Element<'_, Message>>>())
        .spacing(6);

        column![
            heading("Defaults"),
            labelled("New generator sample rate (Hz)", rate_field.into()),
            labelled(
                "CSV import when a count does not add up",
                pick_list(
                    CountMode::ALL.map(ModeEntry).to_vec(),
                    Some(ModeEntry(self.settings.import_mode)),
                    Message::ModePicked,
                )
                .text_size(typography::BODY_SIZE)
                .width(Length::Fixed(300.0))
                .into(),
            ),
            labelled(
                "Scope decimation",
                pick_list(
                    Quality::ALL.map(QualityEntry).to_vec(),
                    Some(QualityEntry(self.settings.decimation)),
                    Message::QualityPicked,
                )
                .text_size(typography::BODY_SIZE)
                .width(Length::Fixed(300.0))
                .into(),
            ),
            labelled(
                "Run retention",
                pick_list(
                    Retention::choices()
                        .into_iter()
                        .map(RetentionEntry)
                        .collect::<Vec<_>>(),
                    Some(RetentionEntry(self.settings.retention)),
                    Message::RetentionPicked,
                )
                .text_size(typography::BODY_SIZE)
                .width(Length::Fixed(300.0))
                .into(),
            ),
            text("A run a baseline names is never deleted, whatever the limit.")
                .size(typography::LABEL_SIZE)
                .style(ui::dim),
            labelled(
                "Samples one run may record",
                pick_list(
                    SampleCap::choices()
                        .into_iter()
                        .map(CapEntry)
                        .collect::<Vec<_>>(),
                    Some(CapEntry(self.settings.sample_cap)),
                    Message::CapPicked,
                )
                .text_size(typography::BODY_SIZE)
                .width(Length::Fixed(300.0))
                .into(),
            ),
            text(
                "Past the cap a stage still records what it did and what it measured; only the \
                 samples are left out, and it says so.",
            )
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
            labelled("Inspector histogram bins", bin_buttons.into()),
        ]
        .spacing(8)
        .into()
    }

    /// The watched folder (§7.6).
    ///
    /// Like the external-library list above it, this is consent rather than
    /// configuration: a folder is here because the user pointed at it, and
    /// that pointing is what makes an unattended import theirs.
    fn ingest_section(&self) -> Element<'_, Message> {
        let mut section = column![
            heading("Watched folder"),
            text(
                "A capture written into this folder is imported with the profile the Import \
                 screen is holding, once it has stopped changing. Files already in the folder \
                 are left alone.",
            )
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
        ]
        .spacing(6);

        match &self.settings.watch_folder {
            Some(folder) => {
                section = section.push(
                    text(folder.display().to_string())
                        .size(typography::BODY_SIZE)
                        .font(typography::READOUT),
                );
                if !folder.is_dir() {
                    section = section.push(
                        text("This folder is not there; nothing is being watched.")
                            .size(typography::LABEL_SIZE)
                            .style(ui::warned),
                    );
                }
                section = section.push(
                    row![
                        command("Watch another folder…", Message::ChooseWatchFolder),
                        command("Stop watching", Message::StopWatching),
                    ]
                    .spacing(6),
                );
            }
            None => {
                section = section.push(
                    text("No folder is watched.")
                        .size(typography::BODY_SIZE)
                        .style(ui::dim),
                );
                section = section.push(command("Watch a folder…", Message::ChooseWatchFolder));
            }
        }
        section.into()
    }

    fn storage_section(&self) -> Element<'_, Message> {
        let mut section = column![row![
            heading("This library"),
            Space::with_width(Length::Fill),
            command("Refresh", Message::Refresh),
            command("Clear stage cache", Message::ClearCache),
            command("Reclaim unused blobs", Message::Sweep),
        ]
        .spacing(6)
        .align_y(Alignment::Center)]
        .spacing(6);

        if let Some(notice) = &self.notice {
            section = section.push(
                text(notice)
                    .size(typography::BODY_SIZE)
                    .style(text::success),
            );
        }
        if let Some(error) = self.error.as_ref().or(self.storage_error.as_ref()) {
            section = section.push(text(error).size(typography::BODY_SIZE).style(text::danger));
        }

        let Some((summary, storage)) = &self.storage else {
            return section
                .push(ui::empty(
                    "No figures yet.",
                    "These are counted from the open library; Refresh reads them again.",
                ))
                .into();
        };

        let rows = [
            ("Datasets", summary.datasets.to_string()),
            ("Trains", summary.trains.to_string()),
            ("Groups", summary.groups.to_string()),
            ("Signals", summary.signals.to_string()),
            ("Pulse fields", summary.pulse_fields.to_string()),
            ("Property definitions", storage.property_defs.to_string()),
            ("Tags", storage.tags.to_string()),
            ("Pipelines", storage.pipelines.to_string()),
            ("Runs", storage.runs.to_string()),
            ("Artifacts", storage.artifacts.to_string()),
            ("Stage cache keys", storage.cache_entries.to_string()),
            ("Baselines", storage.baselines.to_string()),
            ("Samples and pulse fields", fmt_bytes(storage.sample_bytes)),
            ("Render pyramids", fmt_bytes(storage.pyramid_bytes)),
            ("Artifact payloads", fmt_bytes(storage.artifact_bytes)),
            ("Unreferenced", fmt_bytes(storage.unreferenced_bytes)),
            ("File on disk", fmt_bytes(storage.file_bytes)),
        ];
        // Seventeen counts, read down a column. A caption on the left and a
        // flush-right reading on the right lets the eye run down either the
        // names or the figures without tracking across the pair.
        for (label, value) in rows {
            section = section.push(
                row![
                    container(ui::caption(label)).width(Length::Fixed(240.0)),
                    ui::value(value, 140.0),
                ]
                .align_y(Alignment::Center),
            );
        }
        section
            .push(
                text(
                    "Render pyramids are rebuilt on demand, so reclaiming them costs only the \
                     time to draw a signal again.",
                )
                .size(typography::LABEL_SIZE)
                .style(ui::dim),
            )
            .into()
    }
}

impl State {
    /// The whole keyboard map, action by action (§15.11).
    ///
    /// Every entry in the catalogue is here, not only the ones with keys: the
    /// list is what tells the user what *could* have a key, and an action with
    /// none says so and names the palette instead.
    fn keyboard_section(&self) -> Element<'_, Message> {
        let keymap = &self.settings.keymap;
        let mut section = column![
            row![
                heading("Keyboard"),
                Space::with_width(Length::Fill),
                command("Reset every shortcut", Message::ResetKeymap),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(
                "Ctrl+K opens the command palette, which reaches every one of these whether \
                 or not it has a key. A key on a screen's own row works while that screen is \
                 showing; a global one works anywhere.",
            )
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
        ]
        .spacing(6);

        if let Some(action) = self.rebinding {
            section = section.push(
                row![
                    text(format!("Waiting for a key for '{}'…", action.label()))
                        .size(typography::BODY_SIZE)
                        .style(ui::warned),
                    command("Cancel", Message::StopRebinding),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }

        // Shadowed bindings, named before the list rather than hidden in it:
        // a key that silently reaches the wrong action is the failure this
        // whole screen exists to prevent.
        for (kept, shadowed) in keymap.conflicts() {
            section = section.push(
                text(format!(
                    "{} runs '{}', so it no longer reaches '{}'.",
                    keymap.chord(kept).map(Chord::label).unwrap_or_default(),
                    kept.label(),
                    shadowed.label(),
                ))
                .size(typography::LABEL_SIZE)
                .style(ui::warned),
            );
        }

        let mut group = "";
        for action in Action::all() {
            if action.group() != group {
                group = action.group();
                section = section.push(Space::with_height(Length::Fixed(8.0)));
                section = section.push(ui::caption(group));
            }
            section = section.push(self.binding_row(action));
        }
        section.into()
    }

    fn binding_row(&self, action: Action) -> Element<'_, Message> {
        let chord = self.settings.keymap.chord(action);
        let reading = chord.map_or_else(|| "—".to_owned(), Chord::label);

        // The key is a key on a keyboard, so it is set as one; what it does is
        // prose. A rebound key is set at full strength and a default is dim,
        // which is how the list answers "what have I changed?" without a
        // column of ticks.
        let key = text(reading)
            .size(typography::BODY_SIZE)
            .font(typography::READOUT);
        let key = if self.settings.keymap.is_rebound(action) {
            key
        } else {
            key.style(ui::dim)
        };

        let mut controls = row![command(
            if chord.is_some() { "Change" } else { "Bind" },
            Message::Rebind(action),
        )]
        .spacing(4);
        if chord.is_some() {
            controls = controls.push(command("Clear", Message::Unbind(action)));
        }

        row![
            container(key).width(Length::Fixed(160.0)),
            text(action.label())
                .size(typography::BODY_SIZE)
                .style(ui::dim)
                .width(Length::Fill),
            controls,
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }
}

fn heading(label: &str) -> Element<'_, Message> {
    text(label)
        .size(typography::HEADING_SIZE)
        .font(typography::HEADING)
        .into()
}

fn labelled<'a>(label: &'a str, control: Element<'a, Message>) -> Element<'a, Message> {
    column![ui::caption(label), control].spacing(4).into()
}

/// A command on this screen. None of them is what the screen is for — the
/// screen is for the settings themselves — so none of them is filled.
fn command(label: &str, message: Message) -> Element<'_, Message> {
    button(
        text(label)
            .size(typography::LABEL_SIZE)
            .font(typography::LABEL),
    )
    .padding([4, 10])
    .style(button::text)
    .on_press(message)
    .into()
}

/// The break between two groups of settings.
///
/// More air above the rule than below it, so the rule belongs to the heading
/// that follows rather than sitting in no-man's-land between two groups.
fn section_rule<'a>() -> Element<'a, Message> {
    column![
        Space::with_height(Length::Fixed(16.0)),
        ui::rule(),
        Space::with_height(Length::Fixed(10.0)),
    ]
    .into()
}

/// Bytes with a binary prefix, one decimal.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut state = State::default();
        state.adopt(Settings::default(), None, None);
        state
    }

    #[test]
    fn changing_a_setting_marks_it_for_saving() {
        let mut state = state();
        assert!(!state.take_changed());
        let _ = state.update(None, Message::ThemePicked(ThemeEntry(ThemeChoice::Light)));
        assert_eq!(state.settings().theme, ThemeChoice::Light);
        assert!(state.take_changed());
        assert!(!state.take_changed(), "the flag is taken, not left set");
    }

    #[test]
    fn a_sample_rate_is_only_taken_when_it_parses() {
        let mut state = state();
        let _ = state.update(None, Message::RateChanged("not a rate".into()));
        let _ = state.update(None, Message::RateCommitted);
        assert!(state.rate_error.is_some());
        assert_eq!(
            state.settings().default_sample_rate_hz,
            crate::settings::DEFAULT_SAMPLE_RATE_HZ
        );
        assert!(!state.take_changed());

        let _ = state.update(None, Message::RateChanged("1e6".into()));
        let _ = state.update(None, Message::RateCommitted);
        assert!(state.rate_error.is_none());
        assert_eq!(state.settings().default_sample_rate_hz, 1e6);
        assert!(state.take_changed());
    }

    #[test]
    fn a_negative_rate_is_refused() {
        let mut state = state();
        let _ = state.update(None, Message::RateChanged("-5".into()));
        let _ = state.update(None, Message::RateCommitted);
        assert!(state.rate_error.is_some());
    }

    #[test]
    fn choosing_a_library_asks_the_root_to_reopen_it() {
        let mut state = state();
        assert!(state.take_library_request().is_none());
        let _ = state.update(None, Message::LibraryChosen(Some("/tmp/lib.db".into())));
        assert_eq!(
            state.take_library_request(),
            Some(Some(PathBuf::from("/tmp/lib.db")))
        );
        assert!(state.take_changed());

        let _ = state.update(None, Message::UseDefaultLibrary);
        assert_eq!(state.take_library_request(), Some(None));
        assert!(state.settings().library_file.is_none());
    }

    #[test]
    fn a_cancelled_file_dialog_changes_nothing() {
        let mut state = state();
        let _ = state.update(None, Message::LibraryChosen(None));
        assert!(state.take_library_request().is_none());
        assert!(!state.take_changed());
    }

    #[test]
    fn histogram_bins_stay_inside_the_range() {
        let mut state = state();
        let _ = state.update(None, Message::UseHistogramBins(1));
        assert_eq!(state.settings().histogram_bins, MIN_BINS);
        let _ = state.update(None, Message::UseHistogramBins(1_000_000));
        assert_eq!(state.settings().histogram_bins, MAX_BINS);
    }

    #[test]
    fn typing_a_rate_leaves_the_setting_alone_until_the_field_is_submitted() {
        // The box is a draft: the setting moves when the user presses enter,
        // not on the keystroke that leaves "9" a valid rate of its own.
        let mut state = state();
        let _ = state.update(None, Message::RateChanged("9".into()));
        let _ = state.update(None, Message::RateChanged("96000".into()));
        assert_eq!(
            state.settings().default_sample_rate_hz,
            crate::settings::DEFAULT_SAMPLE_RATE_HZ
        );
        assert!(!state.take_changed());

        let _ = state.update(None, Message::RateCommitted);
        assert_eq!(state.settings().default_sample_rate_hz, 96_000.0);
        assert!(state.take_changed());
    }

    #[test]
    fn a_rate_that_is_not_a_finite_number_of_hertz_is_refused() {
        for text in ["0", "inf", "NaN", "   ", "48 kHz"] {
            let mut state = state();
            let _ = state.update(None, Message::RateChanged(text.into()));
            let _ = state.update(None, Message::RateCommitted);
            assert!(state.rate_error.is_some(), "'{text}' was taken as a rate");
            assert_eq!(
                state.settings().default_sample_rate_hz,
                crate::settings::DEFAULT_SAMPLE_RATE_HZ,
                "'{text}'"
            );
            assert!(!state.take_changed(), "'{text}'");
        }
    }

    #[test]
    fn a_rate_that_parses_clears_the_complaint_about_the_last_one() {
        let mut state = state();
        let _ = state.update(None, Message::RateChanged("wobble".into()));
        let _ = state.update(None, Message::RateCommitted);
        assert!(state.rate_error.is_some());

        let _ = state.update(None, Message::RateChanged("192e3".into()));
        let _ = state.update(None, Message::RateCommitted);
        assert!(state.rate_error.is_none());
        assert_eq!(state.settings().default_sample_rate_hz, 192_000.0);
    }

    #[test]
    fn every_picker_writes_the_setting_it_stands_for() {
        let mut state = state();
        let _ = state.update(None, Message::ModePicked(ModeEntry(CountMode::Strict)));
        assert_eq!(state.settings().import_mode, CountMode::Strict);
        assert!(state.take_changed());

        let _ = state.update(None, Message::QualityPicked(QualityEntry(Quality::Fine)));
        assert_eq!(state.settings().decimation, Quality::Fine);
        assert!(state.take_changed());

        let _ = state.update(
            None,
            Message::RetentionPicked(RetentionEntry(Retention::Keep(5))),
        );
        assert_eq!(state.settings().retention, Retention::Keep(5));
        assert!(state.take_changed());

        let _ = state.update(None, Message::CapPicked(CapEntry(SampleCap::Mib(1024))));
        assert_eq!(state.settings().sample_cap, SampleCap::Mib(1024));
        assert!(state.take_changed());
    }

    #[test]
    fn clearing_the_cache_needs_a_library_and_says_what_it_unpublished() {
        let mut state = state();
        let _ = state.update(None, Message::ClearCache);
        assert_eq!(state.error.as_deref(), Some("No library is open."));

        let _ = state.update(None, Message::CacheCleared(Ok(0)));
        assert_eq!(
            state.notice.as_deref(),
            Some("The stage cache was already empty.")
        );
        let _ = state.update(None, Message::CacheCleared(Ok(1)));
        let notice = state.notice.clone().unwrap();
        assert!(
            notice.starts_with("Unpublished 1 stage cache key."),
            "{notice}"
        );
        assert!(
            notice.contains("nothing recorded was deleted"),
            "clearing keys must not read as deleting runs: {notice}"
        );
        let _ = state.update(None, Message::CacheCleared(Ok(3)));
        assert!(state.notice.clone().unwrap().contains("3 stage cache keys"));
    }

    #[test]
    fn reclaiming_space_needs_a_library() {
        let mut state = state();
        let _ = state.update(None, Message::Sweep);
        assert_eq!(state.error.as_deref(), Some("No library is open."));
    }

    #[test]
    fn a_reclaim_says_how_many_blobs_it_freed() {
        let mut state = state();
        let _ = state.update(None, Message::Swept(Ok(0)));
        assert_eq!(state.notice.as_deref(), Some("Nothing to reclaim."));
        let _ = state.update(None, Message::Swept(Ok(1)));
        assert_eq!(
            state.notice.as_deref(),
            Some("Reclaimed 1 unreferenced blob.")
        );
        let _ = state.update(None, Message::Swept(Ok(4)));
        assert_eq!(
            state.notice.as_deref(),
            Some("Reclaimed 4 unreferenced blobs.")
        );
        let _ = state.update(None, Message::Swept(Err("the file is locked".into())));
        assert_eq!(state.error.as_deref(), Some("the file is locked"));
    }

    #[test]
    fn an_allowed_library_is_listed_once_and_can_be_taken_off_the_list() {
        let mut state = state();
        let path = PathBuf::from("vendor-stages.dll");
        state.settings.external_libraries.push(path.clone());

        let _ = state.update(None, Message::ExternalLibraryChosen(Some(path.clone())));
        assert_eq!(state.settings().external_libraries.len(), 1);
        assert_eq!(
            state.notice.as_deref(),
            Some("That library is already allowed.")
        );
        assert!(!state.take_changed(), "nothing changed, so nothing to save");

        let _ = state.update(None, Message::RemoveExternalLibrary(path));
        assert!(state.settings().external_libraries.is_empty());
        assert!(state.take_changed());
        assert!(state.notice.is_some(), "the removal says what it means");
    }

    #[test]
    fn a_library_that_will_not_load_is_reported_rather_than_allowed() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::ExternalLibraryChosen(Some(PathBuf::from("no-such-vendor-library.dll"))),
        );
        assert!(state.error.is_some());
        assert!(
            state.settings().external_libraries.is_empty(),
            "a library that cannot be loaded is not added to the allow-list"
        );
        assert!(!state.take_changed());
    }

    #[test]
    fn the_screen_builds_a_view_in_every_state_it_can_be_in() {
        let mut state = state();
        let _ = state.view();

        // With the figures the storage section reports …
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let figures = store
            .read(|conn| Ok((sp_store::library::summary(conn)?, stats::storage(conn)?)))
            .unwrap();
        let _ = state.update(None, Message::Storage(Ok(figures)));
        state
            .settings
            .external_libraries
            .push(PathBuf::from("vendor-stages.dll"));
        state.notice = Some("Reclaimed 2 unreferenced blobs.".to_owned());
        let _ = state.view();

        // … and with every message the screen can show at once.
        let _ = state.update(None, Message::Storage(Err("the file is locked".into())));
        let _ = state.update(None, Message::RateChanged("wobble".into()));
        let _ = state.update(None, Message::RateCommitted);
        state.error = Some("boom".to_owned());
        let _ = state.view();
    }

    #[test]
    fn bytes_format_with_binary_prefixes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(2048), "2.0 KiB");
    }

    // ----------------------------------------------------------- M13 (§7.6)

    #[test]
    fn choosing_a_watched_folder_says_what_will_and_will_not_happen() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state();
        assert_eq!(state.settings().watch_folder, None);

        // An empty folder: nothing is left behind, so nothing is warned about.
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let _ = state.update(None, Message::WatchFolderChosen(Some(empty.clone())));
        assert_eq!(state.settings().watch_folder, Some(empty));
        assert!(state.take_changed());
        assert!(state
            .notice
            .as_deref()
            .unwrap()
            .contains("imports on its own"));

        // A folder with captures in it: the census is announced, because a
        // user expecting two imports should find out here (§7.6).
        let full = dir.path().join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("a.csv"), "x\n1\n").unwrap();
        std::fs::write(full.join("b.csv"), "x\n1\n").unwrap();
        let _ = state.update(None, Message::WatchFolderChosen(Some(full)));
        let notice = state.notice.clone().unwrap();
        assert!(notice.contains("2 files"), "{notice}");
        assert!(notice.contains("left alone"), "{notice}");
        let _ = state.view();

        // A cancelled dialog changes nothing; stopping clears the setting.
        let before = state.settings().watch_folder.clone();
        let _ = state.update(None, Message::WatchFolderChosen(None));
        assert_eq!(state.settings().watch_folder, before);
        let _ = state.update(None, Message::StopWatching);
        assert_eq!(state.settings().watch_folder, None);
        assert!(state.take_changed());
        // And with nothing watched there is nothing to stop.
        let _ = state.update(None, Message::StopWatching);
        assert!(!state.take_changed());
        let _ = state.view();
    }

    #[test]
    fn a_watched_folder_that_is_gone_is_said_so_rather_than_hidden() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::WatchFolderChosen(Some(PathBuf::from("no-such-folder"))),
        );
        // The setting is still the setting — it is the user's text, and the
        // folder may come back — and the screen says the watch is not running.
        assert!(state.settings().watch_folder.is_some());
        let _ = state.view();
    }

    #[test]
    fn a_key_is_captured_into_the_map_and_reported() {
        let mut state = state();
        assert!(state.rebinding().is_none());

        let _ = state.update(None, Message::Rebind(Action::PipelineRun));
        assert_eq!(state.rebinding(), Some(Action::PipelineRun));
        let _ = state.view();

        let chord = Chord::ctrl(crate::keymap::Key::Char('r'));
        let _ = state.update(None, Message::BindCaptured(chord));
        assert!(state.rebinding().is_none(), "one key, one binding");
        assert_eq!(
            state.settings().keymap.chord(Action::PipelineRun),
            Some(chord)
        );
        assert!(state.take_changed());
        assert!(state.notice.as_deref().unwrap().contains("Ctrl R"));

        // A key with nothing waiting for it is dropped on the floor.
        let _ = state.update(
            None,
            Message::BindCaptured(Chord::ctrl(crate::keymap::Key::Char('q'))),
        );
        assert_eq!(
            state.settings().keymap.chord(Action::PipelineRun),
            Some(chord)
        );
    }

    /// A chord bound twice resolves rather than being refused, and the screen
    /// says what the key no longer reaches — both in the notice and in a
    /// standing line above the list.
    #[test]
    fn binding_a_key_that_is_taken_names_what_it_shadows() {
        let mut state = state();
        let _ = state.update(None, Message::Rebind(Action::PipelineRun));
        let _ = state.update(
            None,
            Message::BindCaptured(Chord::ctrl(crate::keymap::Key::Char('t'))),
        );
        let notice = state.notice.clone().unwrap();
        assert!(
            notice.contains("Toggle the light and dark theme"),
            "{notice}"
        );
        assert_eq!(state.settings().keymap.conflicts().len(), 1);
        let _ = state.view();
    }

    #[test]
    fn clearing_and_resetting_a_binding_both_write_the_settings() {
        let mut state = state();
        let _ = state.update(None, Message::Unbind(Action::ToggleTheme));
        assert_eq!(state.settings().keymap.chord(Action::ToggleTheme), None);
        assert!(state.take_changed());
        assert!(state.notice.as_deref().unwrap().contains("command palette"));
        let _ = state.view();

        let _ = state.update(None, Message::ResetKeymap);
        assert_eq!(
            state.settings().keymap.chord(Action::ToggleTheme),
            Some(Chord::ctrl(crate::keymap::Key::Char('t')))
        );
        assert!(state.take_changed());
    }

    #[test]
    fn escape_out_of_a_rebinding_leaves_the_map_alone() {
        let mut state = state();
        let before = state.settings().keymap.clone();
        let _ = state.update(None, Message::Rebind(Action::ScopeToggle));
        let _ = state.update(None, Message::StopRebinding);
        assert!(state.rebinding().is_none());
        assert_eq!(state.settings().keymap, before);
        assert!(!state.take_changed());
    }

    /// Every action in the catalogue has a row, so the list is what tells the
    /// user what could have a key rather than only what does.
    #[test]
    fn the_shortcut_list_has_a_row_for_every_action() {
        let state = state();
        for action in Action::all() {
            let _ = state.binding_row(action);
        }
        let _ = state.keyboard_section();
    }
}
