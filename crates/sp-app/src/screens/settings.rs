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

use crate::jobs;
use crate::settings::{Retention, Settings, ThemeChoice, MAX_BINS, MIN_BINS};
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
}

#[derive(Debug, Clone)]
pub enum Message {
    ThemePicked(ThemeEntry),
    ModePicked(ModeEntry),
    QualityPicked(QualityEntry),
    RetentionPicked(RetentionEntry),
    RateChanged(String),
    RateCommitted,
    ChooseLibrary,
    LibraryChosen(Option<PathBuf>),
    UseDefaultLibrary,
    Refresh,
    Storage(Result<(LibrarySummary, StorageStats), String>),
    Sweep,
    Swept(Result<usize, String>),
    UseHistogramBins(usize),
    AddExternalLibrary,
    ExternalLibraryChosen(Option<PathBuf>),
    RemoveExternalLibrary(PathBuf),
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
            self.storage_section(),
            section_rule(),
            self.external_section(),
            section_rule(),
            keyboard_section(),
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
            labelled("Inspector histogram bins", bin_buttons.into()),
        ]
        .spacing(8)
        .into()
    }

    fn storage_section(&self) -> Element<'_, Message> {
        let mut section = column![row![
            heading("This library"),
            Space::with_width(Length::Fill),
            command("Refresh", Message::Refresh),
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
            ("Baselines", storage.baselines.to_string()),
            ("Samples and pulse fields", fmt_bytes(storage.sample_bytes)),
            ("Render pyramids", fmt_bytes(storage.pyramid_bytes)),
            ("Artifact payloads", fmt_bytes(storage.artifact_bytes)),
            ("Unreferenced", fmt_bytes(storage.unreferenced_bytes)),
            ("File on disk", fmt_bytes(storage.file_bytes)),
        ];
        // Sixteen counts, read down a column. A caption on the left and a
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

fn keyboard_section<'a>() -> Element<'a, Message> {
    let mut section = column![
        heading("Keyboard"),
        text("Fixed in this version; a configurable map is a later milestone (§15.11).")
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
    ]
    .spacing(4);
    for (keys, action) in [
        ("Ctrl+1 … Ctrl+9, Ctrl+0", "Jump to a screen"),
        ("Ctrl+T", "Switch theme"),
        ("Space", "Play or pause (Scope, Results)"),
        ("Home / End", "Jump to the start or the end"),
        ("[ / ]", "Set the loop points (Scope)"),
        ("← / →", "Step through the stage rail (Results)"),
    ] {
        // The keys are keys on a keyboard, so they are set as keys; what
        // they do is prose, so it is set as prose.
        section = section.push(
            row![
                container(
                    text(keys)
                        .size(typography::BODY_SIZE)
                        .font(typography::READOUT),
                )
                .width(Length::Fixed(240.0)),
                text(action).size(typography::BODY_SIZE).style(ui::dim),
            ]
            .align_y(Alignment::Center),
        );
    }
    section.into()
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
    fn bytes_format_with_binary_prefixes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(2048), "2.0 KiB");
    }
}
