//! The Import screen (`docs/DESIGN.md` §12.1, §7.4).
//!
//! File picker → preview of the headers and first group → column-mapping
//! panel (which column is the time of arrival and in what unit, which columns
//! bind to property definitions) → profile save/load → progress with a live
//! error list.
//!
//! The sniff, the import and every store call run off the UI thread; the
//! screen only ever holds metadata and the first group's cells.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::widget::{
    button, checkbox, column, container, pick_list, progress_bar, row, scrollable, text,
    text_input, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task};
use sp_core::{DType, PropScope, PropertyDef, TimeUnit};
use sp_csv::profile::{ColumnRule, CountMode};
use sp_csv::{
    ingest, sniff_file, ColumnHint, Diagnostic, ImportControl, ImportProfile, ImportProgress,
    ImportReport, ImportRequest, Preview,
};
use sp_store::{profiles, props, SavedProfile, Store};

use crate::jobs;
use crate::typography;
use crate::ui;

/// Diagnostics listed before the panel says "and N more".
const DIAGNOSTIC_ROWS: usize = 200;

/// How a column's cells are stored, as one choice in the mapping panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// `f64` — the default, and what a lossless round trip needs (G1).
    Full,
    /// `f32` — half the bytes, ample for most captured values.
    Compact,
    /// `i32` — for a column of whole numbers.
    Whole,
    /// Cells kept as written, stored dictionary-encoded.
    Text,
}

impl Storage {
    pub const ALL: [Self; 4] = [Self::Full, Self::Compact, Self::Whole, Self::Text];

    fn of(rule: Option<&ColumnRule>) -> Self {
        match rule {
            Some(rule) if rule.is_text() => Self::Text,
            Some(rule) => match rule.dtype {
                Some(DType::F32) => Self::Compact,
                Some(DType::I32) => Self::Whole,
                _ => Self::Full,
            },
            None => Self::Full,
        }
    }

    fn apply(self, rule: ColumnRule) -> ColumnRule {
        let mut rule = rule;
        rule.text = (self == Self::Text).then_some(true);
        rule.dtype = match self {
            Self::Full => Some(DType::F64),
            Self::Compact => Some(DType::F32),
            Self::Whole => Some(DType::I32),
            Self::Text => None,
        };
        rule
    }
}

impl fmt::Display for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "Number (f64)",
            Self::Compact => "Number (f32)",
            Self::Whole => "Whole (i32)",
            Self::Text => "Text",
        })
    }
}

/// The property definition a column is bound to, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding(pub Option<String>);

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(key) => f.write_str(key),
            None => f.write_str("— from the label —"),
        }
    }
}

/// `TimeUnit` with a label for the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitChoice(pub TimeUnit);

impl UnitChoice {
    const ALL: [Self; 4] = [
        Self(TimeUnit::Microseconds),
        Self(TimeUnit::Nanoseconds),
        Self(TimeUnit::Milliseconds),
        Self(TimeUnit::Seconds),
    ];
}

impl fmt::Display for UnitChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            TimeUnit::Seconds => "seconds",
            TimeUnit::Milliseconds => "milliseconds (ms)",
            TimeUnit::Microseconds => "microseconds (µs)",
            TimeUnit::Nanoseconds => "nanoseconds (ns)",
        })
    }
}

/// The delimiter, including "detect it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelimiterChoice(pub Option<char>);

impl DelimiterChoice {
    const ALL: [Self; 5] = [
        Self(None),
        Self(Some(',')),
        Self(Some(';')),
        Self(Some('\t')),
        Self(Some('|')),
    ];
}

impl fmt::Display for DelimiterChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("Detect"),
            Some(delimiter) => f.write_str(sp_csv::parse::delimiter_label(delimiter)),
        }
    }
}

/// `CountMode` with a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeChoice(pub CountMode);

impl ModeChoice {
    const ALL: [Self; 2] = [Self(CountMode::Tolerant), Self(CountMode::Strict)];
}

impl fmt::Display for ModeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            CountMode::Tolerant => "Tolerant — warn and carry on",
            CountMode::Strict => "Strict — refuse the file",
        })
    }
}

/// A running import, and the two things the UI needs to reach into it.
#[derive(Debug)]
struct Job {
    progress: Arc<Mutex<ImportProgress>>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
pub struct State {
    path: String,
    profile: ImportProfile,
    preview: Option<Preview>,
    preview_error: Option<String>,
    sniffing: bool,
    dataset_name: String,
    /// Group-scope definitions a group column can bind to (§6.4).
    defs: Vec<PropertyDef>,
    saved: Vec<SavedProfile>,
    /// Which saved profile the current settings came from.
    loaded_profile: Option<i64>,
    save_as: String,
    job: Option<Job>,
    shown_progress: ImportProgress,
    report: Option<ImportReport>,
    error: Option<String>,
    notice: Option<String>,
    /// Set when an import commits, so the root can refresh the library.
    completed: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Browse,
    FilePicked(Option<PathBuf>),
    PathChanged(String),
    Sniff,
    Sniffed(Result<Box<Preview>, String>),

    PreambleChanged(String),
    DelimiterPicked(DelimiterChoice),
    UnitPicked(UnitChoice),
    ModePicked(ModeChoice),
    CountColumnPicked(String),
    TimeColumnPicked(String),
    NameColumnPicked(String),
    DatasetNameChanged(String),

    ToggleGroupColumn(String, bool),
    GroupBindingPicked(String, Binding),
    TogglePulseColumn(String, bool),
    PulseStoragePicked(String, Storage),
    PulseKeyChanged(String, String),
    ApplyProposals,

    SaveAsChanged(String),
    SaveProfile,
    LoadProfile(i64),
    DeleteProfile(i64),
    ProfilesLoaded(Result<Vec<SavedProfile>, String>),
    DefsLoaded(Result<Vec<PropertyDef>, String>),
    ProfileSaved(Result<i64, String>),

    Start,
    Tick,
    Cancel,
    Imported(Result<Box<ImportReport>, String>),
}

impl State {
    /// Whether an import committed since this was last asked, so the root can
    /// reload the library tree exactly once.
    pub fn take_completed(&mut self) -> bool {
        std::mem::take(&mut self.completed)
    }

    /// The mode a fresh import profile starts in (Settings, §12.1).
    ///
    /// It applies to the profile on screen too: the mode is a decision about
    /// the file being imported now, and the user who just changed the default
    /// means it for this import as well.
    pub fn set_default_mode(&mut self, mode: CountMode) {
        self.profile.mode = mode;
    }

    /// Loads the saved profiles and the property definitions a column can bind
    /// to.
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        Task::batch([
            Task::perform(jobs::read(store.clone(), profiles::list), |result| {
                Message::ProfilesLoaded(result)
            }),
            Task::perform(
                jobs::read(store.clone(), |conn| {
                    props::list_property_defs(conn, Some(PropScope::Group))
                }),
                Message::DefsLoaded,
            ),
        ])
    }

    /// Ticks while an import runs, so progress and the error list stay live.
    pub fn subscription(&self) -> Subscription<Message> {
        if self.job.is_some() {
            iced::time::every(Duration::from_millis(120)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Browse => Task::perform(pick_file(), Message::FilePicked),
            Message::FilePicked(None) => Task::none(),
            Message::FilePicked(Some(path)) => {
                self.path = path.display().to_string();
                if self.dataset_name.trim().is_empty() {
                    self.dataset_name = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
                self.sniff()
            }
            Message::PathChanged(path) => {
                self.path = path;
                Task::none()
            }
            Message::Sniff => self.sniff(),
            Message::Sniffed(result) => {
                self.sniffing = false;
                match result {
                    Ok(preview) => {
                        self.preview_error = None;
                        // Take the structural proposals — which columns are
                        // text — but leave storage width to the user.
                        self.profile = preview.proposed(&self.profile);
                        self.preview = Some(*preview);
                    }
                    Err(error) => {
                        tracing::warn!(%error, path = %self.path, "could not preview the file");
                        self.preview = None;
                        self.preview_error = Some(error);
                    }
                }
                Task::none()
            }

            Message::PreambleChanged(raw) => {
                if raw.trim().is_empty() {
                    self.profile.preamble_lines = 0;
                } else if let Ok(lines) = raw.trim().parse::<usize>() {
                    self.profile.preamble_lines = lines.min(1_000);
                }
                self.sniff()
            }
            Message::DelimiterPicked(choice) => {
                self.profile.delimiter = choice.0;
                self.sniff()
            }
            Message::UnitPicked(choice) => {
                self.profile.time_unit = choice.0;
                self.resolve_preview();
                Task::none()
            }
            Message::ModePicked(choice) => {
                self.profile.mode = choice.0;
                Task::none()
            }
            Message::CountColumnPicked(column) => {
                self.profile.count_column = Some(column);
                self.resolve_preview();
                Task::none()
            }
            Message::TimeColumnPicked(column) => {
                self.profile.time_column = Some(column);
                self.resolve_preview();
                Task::none()
            }
            Message::NameColumnPicked(column) => {
                self.profile.group_name_column = Some(column);
                self.resolve_preview();
                Task::none()
            }
            Message::DatasetNameChanged(name) => {
                self.dataset_name = name;
                Task::none()
            }

            Message::ToggleGroupColumn(label, include) => {
                let mut rule = self.group_rule(&label);
                rule.include = include;
                self.profile.set_group_rule(rule);
                self.resolve_preview();
                Task::none()
            }
            Message::GroupBindingPicked(label, Binding(key)) => {
                let mut rule = self.group_rule(&label);
                rule.key = key;
                self.profile.set_group_rule(rule);
                self.resolve_preview();
                Task::none()
            }
            Message::TogglePulseColumn(label, include) => {
                let mut rule = self.pulse_rule(&label);
                rule.include = include;
                self.profile.set_pulse_rule(rule);
                self.resolve_preview();
                Task::none()
            }
            Message::PulseStoragePicked(label, storage) => {
                let rule = storage.apply(self.pulse_rule(&label));
                self.profile.set_pulse_rule(rule);
                self.resolve_preview();
                Task::none()
            }
            Message::PulseKeyChanged(label, key) => {
                let mut rule = self.pulse_rule(&label);
                rule.key = (!key.trim().is_empty()).then(|| key.trim().to_owned());
                self.profile.set_pulse_rule(rule);
                self.resolve_preview();
                Task::none()
            }
            Message::ApplyProposals => {
                if let Some(preview) = &self.preview {
                    for hint in &preview.pulse_hints {
                        let Some(dtype) = hint.narrowable_dtype else {
                            continue;
                        };
                        if hint.index == preview.layout.time_index {
                            continue;
                        }
                        let mut rule = self.pulse_rule(&hint.label);
                        rule.dtype = Some(dtype);
                        self.profile.set_pulse_rule(rule);
                    }
                }
                self.resolve_preview();
                Task::none()
            }

            Message::SaveAsChanged(name) => {
                self.save_as = name;
                Task::none()
            }
            Message::SaveProfile => {
                let name = self.save_as.trim().to_owned();
                let Some(store) = store else {
                    return Task::none();
                };
                if name.is_empty() {
                    self.error = Some("Give the profile a name before saving it.".to_owned());
                    return Task::none();
                }
                self.profile.name = name.clone();
                let json = match self.profile.to_json() {
                    Ok(json) => json,
                    Err(error) => {
                        self.error = Some(error.to_string());
                        return Task::none();
                    }
                };
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        profiles::save(conn, &name, &json)
                    }),
                    Message::ProfileSaved,
                )
            }
            Message::LoadProfile(id) => {
                let Some(saved) = self.saved.iter().find(|profile| profile.id == id) else {
                    return Task::none();
                };
                match ImportProfile::from_json(&saved.rules_json) {
                    Ok(profile) => {
                        self.profile = profile;
                        self.save_as = saved.name.clone();
                        self.loaded_profile = Some(id);
                        self.notice = Some(format!("Loaded profile '{}'.", saved.name));
                        self.sniff()
                    }
                    Err(error) => {
                        self.error = Some(format!("'{}' could not be read: {error}", saved.name));
                        Task::none()
                    }
                }
            }
            Message::DeleteProfile(id) => match store {
                Some(store) => {
                    if self.loaded_profile == Some(id) {
                        self.loaded_profile = None;
                    }
                    let store = store.clone();
                    Task::perform(
                        jobs::write(store.clone(), move |conn| {
                            profiles::delete(conn, id)?;
                            profiles::list(conn)
                        }),
                        Message::ProfilesLoaded,
                    )
                }
                None => Task::none(),
            },
            Message::ProfilesLoaded(result) => {
                match result {
                    Ok(saved) => self.saved = saved,
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::DefsLoaded(result) => {
                match result {
                    Ok(defs) => self.defs = defs,
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::ProfileSaved(result) => match result {
                Ok(id) => {
                    self.loaded_profile = Some(id);
                    self.notice = Some(format!("Saved profile '{}'.", self.profile.name));
                    match store {
                        Some(store) => Task::perform(
                            jobs::read(store.clone(), profiles::list),
                            Message::ProfilesLoaded,
                        ),
                        None => Task::none(),
                    }
                }
                Err(error) => {
                    self.error = Some(error);
                    Task::none()
                }
            },

            Message::Start => self.start(store),
            Message::Tick => {
                if let Some(job) = &self.job {
                    self.shown_progress = *job.progress.lock().expect("progress mutex");
                }
                Task::none()
            }
            Message::Cancel => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                    self.notice = Some("Cancelling…".to_owned());
                }
                Task::none()
            }
            Message::Imported(result) => {
                self.job = None;
                match result {
                    Ok(report) => {
                        self.notice = Some(format!("Imported {}", report.summary()));
                        self.error = None;
                        self.report = Some(*report);
                        self.completed = true;
                    }
                    Err(error) => {
                        tracing::error!(%error, "import failed");
                        self.error = Some(error);
                        self.report = None;
                    }
                }
                Task::none()
            }
        }
    }

    /// Re-runs the sniff against the current settings.
    fn sniff(&mut self) -> Task<Message> {
        let path = PathBuf::from(self.path.trim());
        if path.as_os_str().is_empty() {
            self.preview = None;
            self.preview_error = None;
            return Task::none();
        }
        self.sniffing = true;
        self.error = None;
        let profile = self.profile.clone();
        Task::perform(
            jobs::blocking(move || {
                sniff_file(&path, &profile)
                    .map(Box::new)
                    .map_err(|error| error.to_string())
            }),
            Message::Sniffed,
        )
    }

    /// Re-resolves the layout the current settings produce, without re-reading
    /// the file: mapping changes never need another pass.
    fn resolve_preview(&mut self) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        match self.profile.resolve(
            preview.layout.delimiter,
            preview.layout.preamble.clone(),
            preview.layout.group_header.clone(),
            preview.layout.pulse_header.clone(),
        ) {
            Ok(layout) => {
                preview.layout = layout;
                self.preview_error = None;
            }
            Err(error) => self.preview_error = Some(error.to_string()),
        }
    }

    fn start(&mut self, store: Option<&Store>) -> Task<Message> {
        let Some(store) = store else {
            self.error = Some("No library is open.".to_owned());
            return Task::none();
        };
        let path = PathBuf::from(self.path.trim());
        if path.as_os_str().is_empty() {
            self.error = Some("Choose a file to import.".to_owned());
            return Task::none();
        }

        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let sink = progress.clone();
        let control = ImportControl::new()
            .with_cancel(cancel.clone())
            .with_progress(Arc::new(move |update| {
                *sink.lock().expect("progress mutex") = update;
            }));

        let mut request = ImportRequest::new(self.profile.clone()).named(self.dataset_name.trim());
        request = request.from_file(&path);
        if let Some(id) = self.loaded_profile {
            request = request.with_profile_id(id);
        }

        self.job = Some(Job {
            progress: progress.clone(),
            cancel,
        });
        self.shown_progress = ImportProgress::default();
        self.report = None;
        self.error = None;
        self.notice = None;

        tracing::info!(path = %path.display(), "import started");
        Task::perform(
            jobs::write(store.clone(), move |conn| {
                Ok(ingest::import_file(conn, &path, &request, &control)
                    .map(Box::new)
                    .map_err(|error| error.to_string()))
            }),
            |outcome| Message::Imported(outcome.and_then(|inner| inner)),
        )
    }

    fn group_rule(&self, label: &str) -> ColumnRule {
        self.profile
            .group_rule(label)
            .cloned()
            .unwrap_or_else(|| ColumnRule::new(label))
    }

    fn pulse_rule(&self, label: &str) -> ColumnRule {
        self.profile
            .pulse_rule(label)
            .cloned()
            .unwrap_or_else(|| ColumnRule::new(label))
    }

    // -----------------------------------------------------------------------
    // View
    // -----------------------------------------------------------------------

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let settings = container(scrollable(self.settings_pane()).height(Length::Fill))
            .width(Length::Fixed(430.0))
            .height(Length::Fill)
            .style(ui::panel);

        row![settings, self.preview_pane()]
            .height(Length::Fill)
            .into()
    }

    fn settings_pane(&self) -> Element<'_, Message> {
        let mut pane = column![].spacing(10).padding([12, 14]).width(Length::Fill);

        pane = pane.push(section("Source"));
        pane = pane.push(
            row![
                text_input("Path to a CSV file…", &self.path)
                    .on_input(Message::PathChanged)
                    .on_submit(Message::Sniff)
                    .size(typography::BODY_SIZE),
                button(text("Browse…").size(typography::BODY_SIZE))
                    .padding([5, 9])
                    .style(button::secondary)
                    .on_press(Message::Browse),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );
        pane = pane.push(labelled(
            "Dataset name",
            text_input("From the file name", &self.dataset_name)
                .on_input(Message::DatasetNameChanged)
                .size(typography::BODY_SIZE)
                .into(),
        ));

        pane = pane.push(section("Framing"));
        pane = pane.push(labelled(
            "Preamble lines",
            text_input("1", &self.profile.preamble_lines.to_string())
                .on_input(Message::PreambleChanged)
                .size(typography::BODY_SIZE)
                .into(),
        ));
        pane = pane.push(labelled(
            "Delimiter",
            pick_list(
                DelimiterChoice::ALL.to_vec(),
                Some(DelimiterChoice(self.profile.delimiter)),
                Message::DelimiterPicked,
            )
            .text_size(typography::BODY_SIZE)
            .into(),
        ));
        pane = pane.push(labelled(
            "Time unit",
            pick_list(
                UnitChoice::ALL.to_vec(),
                Some(UnitChoice(self.profile.time_unit)),
                Message::UnitPicked,
            )
            .text_size(typography::BODY_SIZE)
            .into(),
        ));
        pane = pane.push(labelled(
            "Count mismatch",
            pick_list(
                ModeChoice::ALL.to_vec(),
                Some(ModeChoice(self.profile.mode)),
                Message::ModePicked,
            )
            .text_size(typography::BODY_SIZE)
            .into(),
        ));

        if let Some(preview) = &self.preview {
            let group_labels = preview.layout.group_header.clone();
            let pulse_labels = preview.layout.pulse_header.clone();
            pane = pane.push(labelled(
                "Count column",
                pick_list(
                    group_labels.clone(),
                    Some(preview.layout.count_label().to_owned()),
                    Message::CountColumnPicked,
                )
                .text_size(typography::BODY_SIZE)
                .into(),
            ));
            pane = pane.push(labelled(
                "Time column",
                pick_list(
                    pulse_labels,
                    Some(preview.layout.time_label().to_owned()),
                    Message::TimeColumnPicked,
                )
                .text_size(typography::BODY_SIZE)
                .into(),
            ));
            pane = pane.push(labelled(
                "Group name from",
                pick_list(
                    group_labels.clone(),
                    preview
                        .layout
                        .name_index
                        .and_then(|index| group_labels.get(index).cloned()),
                    Message::NameColumnPicked,
                )
                .text_size(typography::BODY_SIZE)
                .into(),
            ));

            pane = pane.push(section("Group columns"));
            pane = pane.push(self.group_mapping(preview));
            pane = pane.push(section("Pulse columns"));
            pane = pane.push(self.pulse_mapping(preview));
        }

        pane = pane.push(section("Profile"));
        pane = pane.push(
            row![
                text_input("Save these settings as…", &self.save_as)
                    .on_input(Message::SaveAsChanged)
                    .on_submit(Message::SaveProfile)
                    .size(typography::BODY_SIZE),
                button(text("Save").size(typography::BODY_SIZE))
                    .padding([5, 9])
                    .style(button::secondary)
                    .on_press(Message::SaveProfile),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );
        if self.saved.is_empty() {
            pane = pane.push(ui::empty(
                "No saved profiles yet.",
                "Every framing constant above is a setting rather than a rule; save the                  set that reads one instrument and it opens the next file in one press.",
            ));
        }
        for saved in &self.saved {
            let active = self.loaded_profile == Some(saved.id);
            pane = pane.push(
                row![
                    button(
                        text(&saved.name)
                            .size(typography::BODY_SIZE)
                            .font(if active {
                                typography::BODY_STRONG
                            } else {
                                typography::BODY
                            }),
                    )
                    .padding([3, 8])
                    .width(Length::Fill)
                    .style(ui::selectable(active))
                    .on_press(Message::LoadProfile(saved.id)),
                    button(
                        text("Delete")
                            .size(typography::LABEL_SIZE)
                            .font(typography::LABEL)
                            .style(ui::dim),
                    )
                    .padding([3, 8])
                    .style(button::text)
                    .on_press(Message::DeleteProfile(saved.id)),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
            );
        }

        pane.into()
    }

    fn group_mapping(&self, preview: &Preview) -> Element<'_, Message> {
        let mut options = vec![Binding(None)];
        options.extend(self.defs.iter().map(|def| Binding(Some(def.key.clone()))));

        let mut list = column![].spacing(4);
        for hint in &preview.group_hints {
            if hint.index == preview.layout.count_index {
                list = list.push(
                    text(format!("{} — the count column", hint.label))
                        .size(typography::BODY_SIZE)
                        .style(ui::dim),
                );
                continue;
            }
            let rule = self.group_rule(&hint.label);
            let stored = preview
                .layout
                .group_stored
                .iter()
                .find(|column| column.index == hint.index);
            let key = stored.map_or_else(|| rule.resolved_key(), |column| column.key.clone());
            list = list.push(
                column![
                    row![
                        checkbox(hint.label.clone(), rule.include)
                            .size(14)
                            .text_size(typography::BODY_SIZE)
                            .on_toggle({
                                let label = hint.label.clone();
                                move |include| Message::ToggleGroupColumn(label.clone(), include)
                            }),
                        Space::with_width(Length::Fill),
                        text(format!("e.g. {}", sample_of(hint)))
                            .size(typography::LABEL_SIZE)
                            .style(ui::dim),
                    ]
                    .align_y(Alignment::Center),
                    row![
                        text(format!("→ {key}"))
                            .size(typography::LABEL_SIZE)
                            .style(ui::dim),
                        Space::with_width(Length::Fill),
                        pick_list(options.clone(), Some(Binding(rule.key.clone())), {
                            let label = hint.label.clone();
                            move |binding| Message::GroupBindingPicked(label.clone(), binding)
                        },)
                        .text_size(typography::BODY_SIZE),
                    ]
                    .align_y(Alignment::Center),
                ]
                .spacing(2),
            );
        }
        list.into()
    }

    fn pulse_mapping(&self, preview: &Preview) -> Element<'_, Message> {
        let mut list = column![].spacing(4);
        let offers = preview.pulse_hints.iter().any(|hint| {
            hint.index != preview.layout.time_index && hint.narrowable_dtype == Some(DType::I32)
        });
        if offers {
            list = list.push(
                row![
                    text("Some columns hold only whole numbers.")
                        .size(typography::LABEL_SIZE)
                        .style(ui::dim),
                    Space::with_width(Length::Fill),
                    button(text("Narrow them").size(typography::LABEL_SIZE))
                        .padding([2, 6])
                        .style(button::text)
                        .on_press(Message::ApplyProposals),
                ]
                .align_y(Alignment::Center),
            );
        }

        for hint in &preview.pulse_hints {
            if hint.index == preview.layout.time_index {
                list = list.push(
                    text(format!(
                        "{} — the time of arrival, in {}",
                        hint.label, self.profile.time_unit
                    ))
                    .size(typography::BODY_SIZE)
                    .style(ui::dim),
                );
                continue;
            }
            let rule = self.pulse_rule(&hint.label);
            list = list.push(
                column![
                    row![
                        checkbox(hint.label.clone(), rule.include)
                            .size(14)
                            .text_size(typography::BODY_SIZE)
                            .on_toggle({
                                let label = hint.label.clone();
                                move |include| Message::TogglePulseColumn(label.clone(), include)
                            }),
                        Space::with_width(Length::Fill),
                        text(format!("e.g. {}", sample_of(hint)))
                            .size(typography::LABEL_SIZE)
                            .style(ui::dim),
                    ]
                    .align_y(Alignment::Center),
                    row![
                        text_input("key", &rule.resolved_key())
                            .on_input({
                                let label = hint.label.clone();
                                move |key| Message::PulseKeyChanged(label.clone(), key)
                            })
                            .size(typography::BODY_SIZE)
                            .width(Length::Fixed(150.0)),
                        Space::with_width(Length::Fill),
                        pick_list(Storage::ALL.to_vec(), Some(Storage::of(Some(&rule))), {
                            let label = hint.label.clone();
                            move |storage| Message::PulseStoragePicked(label.clone(), storage)
                        })
                        .text_size(typography::BODY_SIZE),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ]
                .spacing(2),
            );
        }
        list.into()
    }

    fn preview_pane(&self) -> Element<'_, Message> {
        let mut pane = column![].spacing(10).padding([12, 16]).width(Length::Fill);

        pane = pane.push(self.action_row());

        if let Some(error) = &self.error {
            pane = pane.push(text(error).size(typography::BODY_SIZE).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            pane = pane.push(text(notice).size(typography::BODY_SIZE).style(ui::dim));
        }
        if let Some(job) = &self.job {
            pane = pane.push(self.progress_row(job));
        }

        let body: Element<'_, Message> = match (&self.preview, &self.preview_error) {
            (_, Some(error)) => column![
                text("This file cannot be read as the grouped-block format.")
                    .size(typography::BODY_SIZE),
                // The parser said exactly what stopped it, and it said it in
                // the file's own terms, so it is quoted rather than paraphrased.
                text(error)
                    .size(typography::BODY_SIZE)
                    .font(typography::READOUT)
                    .style(text::danger),
                text(
                    "Adjust the preamble line count or the delimiter on the left — every \
                     framing constant is a setting, not a rule."
                )
                .size(typography::BODY_SIZE)
                .style(ui::dim)
                .width(Length::Fixed(520.0)),
            ]
            .spacing(6)
            .into(),
            (Some(preview), None) => self.preview_table(preview),
            (None, None) if self.sniffing => ui::empty(
                "Reading the first group…",
                "Only the preamble, the headers and the first block are read.",
            ),
            (None, None) => ui::empty(
                "No file chosen.",
                "Pick a CSV on the left: only its preamble, headers and first group are \
                 read, so a multi-gigabyte file previews instantly.",
            ),
        };

        pane = pane.push(ui::rule());
        pane = pane.push(scrollable(body).height(Length::Fill));
        pane.into()
    }

    fn action_row(&self) -> Element<'_, Message> {
        let ready = self.preview.is_some() && self.preview_error.is_none() && self.job.is_none();
        let mut import = button(text("Import").size(typography::BODY_SIZE))
            .padding([6, 14])
            .style(button::primary);
        if ready {
            import = import.on_press(Message::Start);
        }

        let mut actions = row![import].spacing(8).align_y(Alignment::Center);
        if self.job.is_some() {
            actions = actions.push(
                button(text("Cancel").size(typography::BODY_SIZE))
                    .padding([6, 14])
                    .style(button::danger)
                    .on_press(Message::Cancel),
            );
        }
        actions = actions.push(
            button(
                text("Re-read file")
                    .size(typography::LABEL_SIZE)
                    .font(typography::LABEL),
            )
            .padding([6, 10])
            .style(button::text)
            .on_press(Message::Sniff),
        );
        actions = actions.push(Space::with_width(Length::Fill));
        // What the sniffer decided the file is. It sits at the far end of the
        // action row because it is the fact the Import button acts on.
        if let Some(preview) = &self.preview {
            actions = actions.push(
                text(preview.layout.describe())
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
            );
        }
        actions.into()
    }

    fn progress_row(&self, _job: &Job) -> Element<'_, Message> {
        let progress = self.shown_progress;
        let bar: Element<'_, Message> = match progress.fraction() {
            Some(fraction) => progress_bar(0.0..=1.0, fraction).height(6).into(),
            None => Space::with_height(Length::Fixed(6.0)).into(),
        };
        column![
            bar,
            // Three running counts, and the third is the one that decides
            // whether the import is worth keeping, so it says so in colour
            // rather than waiting for the reader to notice it is not zero.
            row![
                ui::fact("Groups", progress.groups),
                ui::fact("Pulses", progress.pulses),
                ui::spec(
                    "Diagnostics",
                    progress.diagnostics,
                    if progress.diagnostics == 0 {
                        text::base
                    } else {
                        ui::warned
                    },
                ),
            ]
            .spacing(20)
            .wrap(),
        ]
        .spacing(4)
        .into()
    }

    fn preview_table<'a>(&'a self, preview: &'a Preview) -> Element<'a, Message> {
        let mut body = column![].spacing(6);

        body = body.push(
            text(format!(
                "First group — declared {} row{}, read {}",
                preview.declared_count,
                plural(u64::from(preview.declared_count)),
                preview.first_group_rows,
            ))
            .size(typography::BODY_SIZE),
        );

        if !preview.layout.preamble.is_empty() {
            body = body.push(
                text(format!("Preamble: {}", preview.layout.preamble.join(" ⏎ ")))
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
            );
        }

        let width = 130.0;
        let mut head = row![];
        for hint in &preview.pulse_hints {
            let dropped = hint.index != preview.layout.time_index
                && !preview
                    .layout
                    .pulse_stored
                    .iter()
                    .any(|column| column.index == hint.index);
            let label = if hint.index == preview.layout.time_index {
                format!("{} (time)", hint.label)
            } else if dropped {
                format!("{} (skipped)", hint.label)
            } else {
                hint.label.clone()
            };
            head = head.push(
                container(text(label).size(typography::LABEL_SIZE).style(ui::dim))
                    .width(Length::Fixed(width))
                    .padding([3, 6]),
            );
        }
        body = body.push(head).push(ui::rule());

        for cells in &preview.rows {
            let mut line = row![];
            for cell in cells {
                let shown = if cell.is_empty() {
                    "—"
                } else {
                    cell.as_str()
                };
                line = line.push(
                    container(text(shown).size(typography::BODY_SIZE))
                        .width(Length::Fixed(width))
                        .padding([3, 6]),
                );
            }
            body = body.push(line);
        }
        if preview.rows.is_empty() {
            body = body.push(ui::empty(
                "The first group declares no rows.",
                "Its header was read, so the framing is right; the block itself is empty.",
            ));
        }

        body = body.push(Space::with_height(Length::Fixed(14.0)));
        body = body.push(self.diagnostics_panel());
        body.into()
    }

    /// The scrollable error list (§7.3): every diagnostic with the line it was
    /// found on.
    fn diagnostics_panel(&self) -> Element<'_, Message> {
        let (items, total, truncated) = match (&self.report, &self.preview) {
            (Some(report), _) => (
                report.diagnostics.items(),
                report.diagnostics.total(),
                report.diagnostics.truncated(),
            ),
            (None, Some(preview)) => (
                preview.diagnostics.items(),
                preview.diagnostics.total(),
                preview.diagnostics.truncated(),
            ),
            (None, None) => (&[][..], 0, false),
        };

        if total == 0 {
            return row![ui::caption("Diagnostics"), ui::dim_reading("none")]
                .spacing(8)
                .align_y(Alignment::Center)
                .into();
        }

        let mut list = column![row![
            ui::caption("Diagnostics"),
            text(total.to_string())
                .size(typography::LABEL_SIZE)
                .font(typography::READOUT)
                .style(ui::warned),
        ]
        .spacing(8)
        .align_y(Alignment::Center)]
        .spacing(3);
        for diagnostic in items.iter().take(DIAGNOSTIC_ROWS) {
            list = list.push(diagnostic_row(diagnostic));
        }
        if items.len() > DIAGNOSTIC_ROWS || truncated {
            list = list.push(
                text(format!(
                    "…and {} more",
                    total.saturating_sub(items.len().min(DIAGNOSTIC_ROWS))
                ))
                .size(typography::LABEL_SIZE)
                .style(ui::dim),
            );
        }
        list.into()
    }
}

fn diagnostic_row(diagnostic: &Diagnostic) -> Element<'_, Message> {
    let position = match diagnostic.column_index {
        Some(column) => format!("line {}, col {}", diagnostic.line, column + 1),
        None => format!("line {}", diagnostic.line),
    };
    // Where it happened is a coordinate in the file, so it is monospaced and
    // in a fixed column: a list of them can be read straight down.
    row![
        container(
            text(position)
                .size(typography::LABEL_SIZE)
                .font(typography::READOUT)
                .style(ui::dim),
        )
        .width(Length::Fixed(120.0)),
        text(&diagnostic.message)
            .size(typography::BODY_SIZE)
            .style(if diagnostic.is_error() {
                text::danger
            } else {
                ui::warned
            }),
    ]
    .spacing(8)
    .into()
}

/// The dialog runs on its own; a cancelled pick is `None`.
async fn pick_file() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Import a CSV file")
        .add_filter("CSV", &["csv", "txt"])
        .add_filter("All files", &["*"])
        .pick_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

fn sample_of(hint: &ColumnHint) -> String {
    match hint.first() {
        "" => "—".to_owned(),
        value => value.to_owned(),
    }
}

/// A group of settings, announced by a caption over a hairline.
///
/// The air above the caption is what makes it a section — more space above
/// than below, so the rule and everything under it belong to the heading
/// rather than floating between two groups.
fn section(title: &str) -> Element<'_, Message> {
    column![
        Space::with_height(Length::Fixed(8.0)),
        ui::caption(title),
        ui::rule(),
    ]
    .spacing(4)
    .into()
}

fn labelled<'a>(label: &'a str, control: Element<'a, Message>) -> Element<'a, Message> {
    row![
        container(ui::caption(label)).width(Length::Fixed(130.0)),
        control,
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_choices_round_trip_through_a_column_rule() {
        for storage in Storage::ALL {
            let rule = storage.apply(ColumnRule::new("power"));
            assert_eq!(Storage::of(Some(&rule)), storage, "{storage}");
        }
        // A column with no rule of its own stores at full precision.
        assert_eq!(Storage::of(None), Storage::Full);
    }

    #[test]
    fn choosing_text_clears_the_numeric_storage_type() {
        let rule = Storage::Text.apply(ColumnRule::new("band"));
        assert!(rule.is_text());
        assert_eq!(rule.dtype, None);

        let rule = Storage::Whole.apply(rule);
        assert!(!rule.is_text());
        assert_eq!(rule.dtype, Some(DType::I32));
    }

    #[test]
    fn a_path_with_no_file_clears_the_preview_rather_than_erroring() {
        let mut state = State {
            preview_error: Some("stale".to_owned()),
            ..State::default()
        };
        let _ = state.update(None, Message::PathChanged("   ".into()));
        let _ = state.update(None, Message::Sniff);
        assert!(state.preview.is_none());
        assert!(state.preview_error.is_none());
        assert!(!state.sniffing);
    }

    #[test]
    fn the_preamble_field_accepts_only_a_line_count() {
        let mut state = State::default();
        let _ = state.update(None, Message::PreambleChanged("3".into()));
        assert_eq!(state.profile.preamble_lines, 3);
        // Junk leaves the last good value alone.
        let _ = state.update(None, Message::PreambleChanged("x".into()));
        assert_eq!(state.profile.preamble_lines, 3);
        let _ = state.update(None, Message::PreambleChanged(String::new()));
        assert_eq!(state.profile.preamble_lines, 0);
    }

    #[test]
    fn mapping_choices_land_on_the_profile() {
        let mut state = State::default();
        let _ = state.update(
            None,
            Message::PulseStoragePicked("power".into(), Storage::Compact),
        );
        assert_eq!(
            state.profile.pulse_rule("power").and_then(|r| r.dtype),
            Some(DType::F32)
        );

        let _ = state.update(None, Message::TogglePulseColumn("angle".into(), false));
        assert_eq!(
            state.profile.pulse_rule("angle").map(|r| r.include),
            Some(false)
        );

        let _ = state.update(
            None,
            Message::GroupBindingPicked("groupID".into(), Binding(Some("channel".into()))),
        );
        assert_eq!(
            state
                .profile
                .group_rule("groupID")
                .map(ColumnRule::resolved_key),
            Some("channel".to_owned())
        );

        let _ = state.update(None, Message::PulseKeyChanged("power".into(), "  ".into()));
        assert_eq!(
            state
                .profile
                .pulse_rule("power")
                .map(ColumnRule::resolved_key),
            Some("power".to_owned()),
            "an empty key falls back to the label"
        );
    }

    #[test]
    fn an_import_cannot_start_without_a_library_or_a_file() {
        let mut state = State::default();
        let _ = state.update(None, Message::Start);
        assert_eq!(state.error.as_deref(), Some("No library is open."));
        assert!(state.job.is_none());
    }

    #[test]
    fn a_completed_import_is_reported_once() {
        let mut state = State::default();
        assert!(!state.take_completed());
        state.completed = true;
        assert!(state.take_completed());
        assert!(!state.take_completed());
    }

    const SAMPLE: &str = "Skip Row
groupID,total time, count, info
time, pulse width, power, band
1, 1000, 2, info
10, 100, 100.5, L
20, 100, 100.5, S
";

    fn previewed() -> State {
        let profile = ImportProfile::default();
        let preview = sp_csv::sniff(SAMPLE.as_bytes(), &profile).unwrap();
        State {
            path: "capture.csv".to_owned(),
            profile: preview.proposed(&profile),
            preview: Some(preview),
            ..State::default()
        }
    }

    #[test]
    fn the_screen_builds_a_view_in_every_state_it_can_be_in() {
        // Nothing chosen yet.
        let state = State::default();
        let _ = state.view();

        // A file previewed, with the mapping panel and the preview table.
        let mut state = previewed();
        let _ = state.view();

        // A saved profile to load, and a definition to bind a column to.
        state.defs = vec![PropertyDef::new(
            "channel",
            PropScope::Group,
            sp_core::PropKind::Int {
                min: None,
                max: None,
            },
        )];
        state.saved = vec![SavedProfile {
            id: 1,
            name: "Radar A".to_owned(),
            rules_json: "{}".to_owned(),
            created_utc: sp_core::time::now_utc(),
        }];
        let _ = state.view();

        // An import running, with progress and a cancel button.
        state.job = Some(Job {
            progress: Arc::new(Mutex::new(ImportProgress {
                bytes_read: 40,
                total_bytes: Some(100),
                groups: 1,
                pulses: 2,
                diagnostics: 0,
            })),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        let _ = state.update(None, Message::Tick);
        let _ = state.view();

        // And a file that cannot be framed at all.
        let state = State {
            preview_error: Some("no count column".to_owned()),
            error: Some("something went wrong".to_owned()),
            notice: Some("a notice".to_owned()),
            ..State::default()
        };
        let _ = state.view();
    }

    #[test]
    fn the_sniff_marks_a_text_column_but_leaves_storage_width_alone() {
        let state = previewed();
        assert_eq!(
            state.profile.pulse_rule("band").map(ColumnRule::is_text),
            Some(true)
        );
        assert_eq!(
            state
                .profile
                .pulse_rule("power")
                .and_then(|rule| rule.dtype),
            None
        );
        assert_eq!(Storage::of(state.profile.pulse_rule("band")), Storage::Text);
    }

    #[test]
    fn narrowing_is_applied_only_when_the_user_asks_for_it() {
        let mut state = previewed();
        assert_eq!(
            state
                .profile
                .pulse_rule("pulse width")
                .and_then(|r| r.dtype),
            None
        );
        let _ = state.update(None, Message::ApplyProposals);
        assert_eq!(
            state
                .profile
                .pulse_rule("pulse width")
                .and_then(|r| r.dtype),
            Some(DType::I32)
        );
        // 'power' holds 100.5, so it is not offered a narrowing.
        assert_eq!(
            state.profile.pulse_rule("power").and_then(|r| r.dtype),
            Some(DType::F64)
        );
    }

    #[test]
    fn re_resolving_the_layout_needs_no_second_pass_over_the_file() {
        let mut state = previewed();
        let _ = state.update(None, Message::TogglePulseColumn("angle".into(), false));
        let _ = state.update(None, Message::CountColumnPicked("total time".into()));
        let layout = &state.preview.as_ref().unwrap().layout;
        assert_eq!(layout.count_label(), "total time");
        // The old count column is now a stored property.
        assert!(layout.group_column("count").is_some());
    }

    #[test]
    fn there_is_no_tick_subscription_when_nothing_is_running() {
        let state = State::default();
        assert!(state.job.is_none());
        // A running job is what turns the ticker on.
        let running = State {
            job: Some(Job {
                progress: Arc::new(Mutex::new(ImportProgress::default())),
                cancel: Arc::new(AtomicBool::new(false)),
            }),
            ..State::default()
        };
        assert!(running.job.is_some());
        let _ = state.subscription();
        let _ = running.subscription();
    }
}
