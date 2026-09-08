//! Application state and the root `update`/`view` (`docs/DESIGN.md` §12.2).

use std::path::PathBuf;

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{keyboard, Alignment, Element, Length, Subscription, Task, Theme};
use sp_store::Store;

use crate::screens::settings::fmt_bytes;
use crate::screens::{
    generate, import, inspector, library, pipeline, properties, results, runs, scope, settings,
    Screen, Section,
};
use crate::settings::Settings;
use crate::typography;
use crate::ui;

/// Everything the UI reads.
///
/// Screen state lives here rather than inside [`Screen`] so it survives
/// navigation (§12.3) — the scope keeps its playhead, viewport and traces
/// while the user is off looking at something else. Later milestones add the
/// stage registry and the active run alongside.
#[derive(Debug)]
pub struct App {
    screen: Screen,
    /// User settings, applied to the screens that care and written back to
    /// `settings.json` whenever one changes (§12.1).
    settings: Settings,
    settings_path: Option<PathBuf>,
    /// Where a library lives when the user has not chosen one.
    default_library: Option<PathBuf>,
    /// The open library, or `None` when opening it failed.
    store: Option<Store>,
    /// Why the library is not open, for the status bar.
    store_error: Option<String>,
    library: library::State,
    import: import::State,
    generate: generate::State,
    pipeline: pipeline::State,
    properties: properties::State,
    inspector: inspector::State,
    settings_screen: settings::State,
    runs: runs::State,
    /// The results screen keeps its run, group and stage selection — and its
    /// playhead — across navigation, for the same reason the scope does.
    results: results::State,
    /// Playback state survives navigation, so comparing two screens never
    /// costs the user their place (§12.3).
    scope: scope::State,
    log_dir: Option<PathBuf>,
}

/// Root message. Each built screen owns a nested message type this
/// delegates to.
#[derive(Debug, Clone)]
pub enum Message {
    Nav(Screen),
    ToggleTheme,
    Library(library::Message),
    Import(import::Message),
    Generate(generate::Message),
    Pipeline(pipeline::Message),
    Properties(properties::Message),
    Inspector(inspector::Message),
    Settings(settings::Message),
    Runs(runs::Message),
    Results(results::Message),
    Scope(scope::Message),
}

impl App {
    /// Boots the application: reads the settings file, opens the library they
    /// name — or the default one — and hands each screen the defaults it
    /// works from.
    ///
    /// A library that will not open is reported in the status bar rather than
    /// aborting: the app is still useful, and the Settings screen is where the
    /// path is changed.
    pub fn new(
        log_dir: Option<PathBuf>,
        settings_path: Option<PathBuf>,
        default_library: Option<PathBuf>,
    ) -> (Self, Task<Message>) {
        let settings = settings_path
            .as_deref()
            .map(Settings::load)
            .unwrap_or_default();
        let library_file = settings.library_file(default_library.clone());

        tracing::info!(
            library = ?library_file,
            log_dir = ?log_dir,
            "SignalPlayback {} starting",
            env!("CARGO_PKG_VERSION"),
        );

        let (store, store_error) = open_library(library_file);

        let mut app = Self {
            screen: Screen::default(),
            settings: settings.clone(),
            settings_path,
            default_library: default_library.clone(),
            store,
            store_error,
            library: library::State::default(),
            import: import::State::default(),
            generate: generate::State::default(),
            pipeline: pipeline::State::default(),
            properties: properties::State::default(),
            inspector: inspector::State::default(),
            settings_screen: settings::State::default(),
            runs: runs::State::default(),
            results: results::State::default(),
            scope: scope::State::default(),
            log_dir,
        };
        app.settings_screen
            .adopt(settings, app.settings_path.clone(), default_library);
        let applied = app.apply_settings();

        let task = Task::batch([applied, app.reload_everything()]);
        (app, task)
    }

    /// Hands the current settings to the screens that work from them.
    fn apply_settings(&mut self) -> Task<Message> {
        let settings = self.settings.clone();
        self.generate
            .set_default_sample_rate(settings.default_sample_rate_hz);
        self.import.set_default_mode(settings.import_mode);
        self.scope.set_quality(settings.decimation);
        self.results.set_quality(settings.decimation);
        self.inspector
            .set_bins(settings.histogram_bins, self.store.as_ref())
            .map(Message::Inspector)
    }

    /// Writes the settings file, if there is anywhere to write it.
    fn save_settings(&self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        if let Err(error) = self.settings.save(path) {
            tracing::warn!(%error, path = %path.display(), "could not save the settings");
        }
    }

    /// Loads every screen from the open library.
    fn reload_everything(&mut self) -> Task<Message> {
        let Some(store) = self.store.clone() else {
            return Task::none();
        };
        Task::batch([
            self.library.load(&store).map(Message::Library),
            self.import.load(&store).map(Message::Import),
            self.generate.load(&store).map(Message::Generate),
            self.pipeline.load(&store).map(Message::Pipeline),
            self.properties.load(&store).map(Message::Properties),
            self.inspector.load(&store).map(Message::Inspector),
            self.settings_screen.load(&store).map(Message::Settings),
            self.runs.load(&store).map(Message::Runs),
            self.results.load(&store).map(Message::Results),
            self.scope.load(&store).map(Message::Scope),
        ])
    }

    /// Opens another library in place — the Settings screen's doing. Screen
    /// state that referred to the old library is dropped, because a signal id
    /// means nothing in a different file.
    fn open(&mut self, library_file: Option<PathBuf>) -> Task<Message> {
        let (store, store_error) = open_library(library_file);
        self.store = store;
        self.store_error = store_error;
        self.library = library::State::default();
        self.inspector = inspector::State::default();
        self.results = results::State::default();
        self.runs = runs::State::default();
        // The pipeline keeps its stages — an algorithm is not a property of
        // the library it last ran over — but not what it ran on.
        self.pipeline.forget_library();
        self.scope = scope::State::default();
        self.reload_everything()
    }

    #[must_use]
    pub fn title(&self) -> String {
        format!("SignalPlayback — {}", self.screen.label())
    }

    #[must_use]
    pub fn theme(&self) -> Theme {
        self.settings.theme.theme()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Nav(screen) => {
                if self.screen != screen {
                    tracing::debug!(from = ?self.screen, to = ?screen, "navigate");
                    self.screen = screen;
                }
                Task::none()
            }
            Message::ToggleTheme => {
                // The shortcut and the Settings screen change one setting, so
                // the choice sticks whichever way it was made.
                self.settings.theme = self.settings.theme.toggled();
                self.settings_screen.adopt(
                    self.settings.clone(),
                    self.settings_path.clone(),
                    self.default_library.clone(),
                );
                self.save_settings();
                Task::none()
            }
            Message::Library(message) => {
                let task = self
                    .library
                    .update(self.store.as_ref(), message)
                    .map(Message::Library);
                // "Inspect" on a row is a navigation as well as a selection.
                match self.library.take_inspect_request() {
                    Some(target) => {
                        self.screen = Screen::Inspector;
                        let show = self
                            .inspector
                            .show(self.store.as_ref(), target)
                            .map(Message::Inspector);
                        Task::batch([task, show])
                    }
                    None => task,
                }
            }
            Message::Import(message) => {
                let task = self
                    .import
                    .update(self.store.as_ref(), message)
                    .map(Message::Import);
                // An import that committed changes what the library holds.
                match (self.import.take_completed(), self.store.clone()) {
                    (true, Some(store)) => Task::batch([
                        task,
                        self.library.load(&store).map(Message::Library),
                        self.scope.load(&store).map(Message::Scope),
                    ]),
                    _ => task,
                }
            }
            Message::Generate(message) => {
                let task = self
                    .generate
                    .update(self.store.as_ref(), message)
                    .map(Message::Generate);
                // A generation that committed changes what the library holds.
                match (self.generate.take_completed(), self.store.clone()) {
                    (true, Some(store)) => Task::batch([
                        task,
                        self.library.load(&store).map(Message::Library),
                        self.scope.load(&store).map(Message::Scope),
                    ]),
                    _ => task,
                }
            }
            Message::Pipeline(message) => {
                let task = self
                    .pipeline
                    .update(self.store.as_ref(), message)
                    .map(Message::Pipeline);
                // A finished run leaves derived signals and artifacts behind,
                // which the library's counts and the scope both read.
                match (self.pipeline.take_completed(), self.store.clone()) {
                    (true, Some(store)) => Task::batch([
                        task,
                        self.prune_runs(&store),
                        self.library.load(&store).map(Message::Library),
                        self.results.load(&store).map(Message::Results),
                        self.runs.load(&store).map(Message::Runs),
                        self.scope.load(&store).map(Message::Scope),
                    ]),
                    _ => task,
                }
            }
            Message::Properties(message) => self
                .properties
                .update(self.store.as_ref(), message)
                .map(Message::Properties),
            Message::Inspector(message) => self
                .inspector
                .update(self.store.as_ref(), message)
                .map(Message::Inspector),
            Message::Runs(message) => {
                let task = self
                    .runs
                    .update(self.store.as_ref(), message)
                    .map(Message::Runs);
                // Opening or diffing a run is the Results screen's job, so the
                // history hands it over rather than drawing a second one.
                match self.runs.take_open_request() {
                    Some((run, against)) => {
                        self.screen = Screen::Results;
                        let open = self
                            .results
                            .open_run(self.store.as_ref(), run, against)
                            .map(Message::Results);
                        Task::batch([task, open])
                    }
                    None => task,
                }
            }
            Message::Settings(message) => {
                let task = self
                    .settings_screen
                    .update(self.store.as_ref(), message)
                    .map(Message::Settings);
                let mut tasks = vec![task];
                if self.settings_screen.take_changed() {
                    self.settings = self.settings_screen.settings().clone();
                    self.save_settings();
                    tasks.push(self.apply_settings());
                }
                // Opening another library is the one setting that cannot be
                // applied in place: everything on screen refers to the old one.
                if let Some(request) = self.settings_screen.take_library_request() {
                    let path = request.or_else(|| self.default_library.clone());
                    tasks.push(self.open(path));
                }
                Task::batch(tasks)
            }
            Message::Results(message) => self
                .results
                .update(self.store.as_ref(), message)
                .map(Message::Results),
            Message::Scope(message) => self
                .scope
                .update(self.store.as_ref(), message)
                .map(Message::Scope),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            self.import.subscription().map(Message::Import),
            self.generate.subscription().map(Message::Generate),
            self.pipeline.subscription().map(Message::Pipeline),
            self.results.subscription().map(Message::Results),
            self.scope.subscription().map(Message::Scope),
            self.transport_shortcuts(),
            self.results_shortcuts(),
            Self::shortcuts(),
        ])
    }

    /// Keyboard transport, live only while the Scope screen is showing
    /// (§11.4): space plays and pauses, `[` and `]` set the loop points,
    /// `Home` and `End` jump to the bounds.
    ///
    /// They are suppressed while the user is typing in the scope's filter,
    /// where a space is a space.
    fn transport_shortcuts(&self) -> Subscription<Message> {
        use keyboard::key::Named;

        if self.screen != Screen::Scope || !self.scope.accepts_transport_keys() {
            return Subscription::none();
        }
        keyboard::on_key_press(|key, modifiers| {
            if modifiers.command() || modifiers.alt() {
                return None;
            }
            let message = match key.as_ref() {
                keyboard::Key::Named(Named::Space) => scope::Message::Toggle,
                keyboard::Key::Named(Named::Home) => scope::Message::SeekFraction(0.0),
                keyboard::Key::Named(Named::End) => scope::Message::SeekFraction(1.0),
                keyboard::Key::Character("[") => scope::Message::SetLoopStart,
                keyboard::Key::Character("]") => scope::Message::SetLoopEnd,
                _ => return None,
            };
            Some(Message::Scope(message))
        })
    }

    /// Keyboard on the Results screen (§10.2): left and right walk the stage
    /// rail, so stepping through an algorithm is one key, and space plays.
    fn results_shortcuts(&self) -> Subscription<Message> {
        use keyboard::key::Named;

        if self.screen != Screen::Results {
            return Subscription::none();
        }
        keyboard::on_key_press(|key, modifiers| {
            if modifiers.command() || modifiers.alt() {
                return None;
            }
            let message = match key.as_ref() {
                keyboard::Key::Named(Named::ArrowLeft) => results::Message::StepStage(-1),
                keyboard::Key::Named(Named::ArrowRight) => results::Message::StepStage(1),
                keyboard::Key::Named(Named::Space) => results::Message::Toggle,
                keyboard::Key::Named(Named::Home) => results::Message::SeekFraction(0.0),
                keyboard::Key::Named(Named::End) => results::Message::SeekFraction(1.0),
                _ => return None,
            };
            Some(Message::Results(message))
        })
    }

    fn shortcuts() -> Subscription<Message> {
        keyboard::on_key_press(|key, modifiers| {
            if !modifiers.command() {
                return None;
            }
            match key.as_ref() {
                keyboard::Key::Character(c) => {
                    let c = c.chars().next()?;
                    if c == 't' {
                        Some(Message::ToggleTheme)
                    } else {
                        Screen::from_shortcut(c).map(Message::Nav)
                    }
                }
                _ => None,
            }
        })
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match self.screen {
            Screen::Library => self.library.view().map(Message::Library),
            Screen::Import => self.import.view().map(Message::Import),
            Screen::Generate => self.generate.view().map(Message::Generate),
            Screen::Pipeline => self.pipeline.view().map(Message::Pipeline),
            Screen::Properties => self.properties.view().map(Message::Properties),
            Screen::Inspector => self.inspector.view().map(Message::Inspector),
            Screen::Runs => self.runs.view().map(Message::Runs),
            Screen::Settings => self.settings_screen.view().map(Message::Settings),
            Screen::Results => self.results.view().map(Message::Results),
            Screen::Scope => self.scope.view().map(Message::Scope),
        };

        let content = column![
            self.header(),
            ui::rule(),
            container(body).height(Length::Fill),
            ui::rule(),
            self.status_bar(),
        ];

        row![self.nav_rail(), content].height(Length::Fill).into()
    }

    /// Sectioned navigation rail. Sections keep the ten screens legible and
    /// group them by what the user is doing rather than by milestone.
    fn nav_rail(&self) -> Element<'_, Message> {
        // The scrollable's content must size to its contents, so the rail's
        // fixed brand and version rows sit outside it.
        // A section is a caption over the screens in it, with air above it
        // rather than a rule between: the group reads as a group because of
        // the space, and the rail stays a list rather than becoming five boxes.
        let mut list = column![].width(Length::Fill);
        for (ordinal, section) in Section::ALL.into_iter().enumerate() {
            list = list.push(Space::with_height(Length::Fixed(if ordinal == 0 {
                4.0
            } else {
                16.0
            })));
            list = list.push(container(ui::caption(section.label())).padding([0, 14]));
            list = list.push(Space::with_height(Length::Fixed(4.0)));
            for screen in section.screens() {
                list = list.push(self.nav_button(screen));
            }
        }

        // The one place the expanded face appears in the chrome. It is a
        // wordmark rather than a heading — it names the instrument, it does
        // not label the pane under it.
        let rail = column![
            container(
                text("SignalPlayback")
                    .size(typography::HEADING_SIZE)
                    .font(typography::TITLE),
            )
            .padding([16, 14])
            .width(Length::Fill),
            scrollable(list).height(Length::Fill),
            ui::rule(),
            container(
                text(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
            )
            .padding([8, 14]),
        ]
        .width(Length::Fixed(190.0))
        .height(Length::Fill);

        container(rail).height(Length::Fill).style(ui::panel).into()
    }

    fn nav_button(&self, screen: Screen) -> Element<'_, Message> {
        let active = self.screen == screen;

        // The name and the key that reaches it are two different things and
        // were being run together into one padded string. The name is read;
        // the shortcut is a key on a keyboard, so it is set as one and pushed
        // to the far edge where the eye can ignore it until it is wanted.
        let label = row![
            text(screen.label())
                .size(typography::BODY_SIZE)
                .font(if active {
                    typography::BODY_STRONG
                } else {
                    typography::BODY
                }),
            Space::with_width(Length::Fill),
        ]
        .push_maybe(screen.shortcut().map(|digit| {
            text(format!("Ctrl {digit}"))
                .size(typography::LABEL_SIZE)
                .font(typography::READOUT)
                .style(ui::dim)
        }))
        .spacing(6)
        .align_y(Alignment::Center);

        button(label)
            .width(Length::Fill)
            .padding([6.0, 14.0])
            .style(ui::selectable(active))
            .on_press(Message::Nav(screen))
            .into()
    }

    fn header(&self) -> Element<'_, Message> {
        let theme_label = format!("{} theme", self.settings.theme.toggled().label());

        // The screen name is the page heading, so it is set as one rather
        // than as a slightly larger line of body text. Switching the theme is
        // a preference and not what this screen is for, so it is a quiet
        // command at the far edge.
        container(
            row![
                text(self.screen.label())
                    .size(typography::TITLE_SIZE)
                    .font(typography::TITLE),
                Space::with_width(Length::Fill),
                button(
                    text(theme_label)
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL)
                        .style(ui::dim),
                )
                .padding([5.0, 10.0])
                .style(button::text)
                .on_press(Message::ToggleTheme),
            ]
            .align_y(Alignment::Center),
        )
        .padding([12, 20])
        .width(Length::Fill)
        .into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        let library: Element<'_, Message> = match (&self.store, &self.store_error) {
            (Some(store), _) => {
                // What the library holds, as counts rather than as a sentence
                // with six plural rules in it. The path is what is open; the
                // counts are what is in it.
                let counts = self.library.summary().map_or_else(String::new, |s| {
                    format!(
                        "{}d {}t {}g {}s {}f  {}",
                        s.datasets,
                        s.trains,
                        s.groups,
                        s.signals,
                        s.pulse_fields,
                        fmt_bytes(s.blob_bytes),
                    )
                });
                row![
                    ui::caption("Library"),
                    text(store.path().display().to_string())
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT),
                    text(counts)
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT)
                        .style(ui::dim),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            }
            (None, Some(error)) => row![
                ui::caption("Library"),
                text(format!("not open — {error}"))
                    .size(typography::LABEL_SIZE)
                    .style(text::danger),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
            (None, None) => row![
                ui::caption("Library"),
                text("none")
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        };

        let logs = self
            .log_dir
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |p| p.display().to_string());

        container(
            row![
                library,
                Space::with_width(Length::Fill),
                ui::caption("Logs"),
                text(logs)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([6, 20])
        .width(Length::Fill)
        .into()
    }
}

/// Applies the retention setting to the pipeline whose run just finished.
///
/// Retention is enforced here rather than in the scheduler because it is a
/// preference, not part of what a run *is*: a headless run in CI keeps
/// everything it records (§9.6).
impl App {
    fn prune_runs(&self, store: &Store) -> Task<Message> {
        let (Some(keep), Some(pipeline)) =
            (self.settings.retention.limit(), self.pipeline.saved_id())
        else {
            return Task::none();
        };
        let store = store.clone();
        Task::future(async move {
            let outcome = crate::jobs::write(store, move |conn| {
                sp_store::runs::prune_runs(conn, pipeline, keep)
            })
            .await;
            match outcome {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, keep, "retention pruned old runs"),
                Err(error) => tracing::warn!(%error, "retention could not prune old runs"),
            }
        })
        .discard()
    }
}

/// Opens a library file, turning a failure into the status bar's message.
fn open_library(path: Option<PathBuf>) -> (Option<Store>, Option<String>) {
    match path {
        Some(path) => match Store::open(&path) {
            Ok(store) => {
                tracing::info!(path = %path.display(), "library open");
                (Some(store), None)
            }
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "could not open the library");
                (None, Some(error.to_string()))
            }
        },
        None => (
            None,
            Some("no writable application data directory was found".to_owned()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::settings::{self as settings_screen, RetentionEntry};
    use crate::settings::{Retention, ThemeChoice};

    fn app() -> App {
        App::new(None, None, None).0
    }

    /// An app with a settings file and a library of its own, in `dir`.
    fn app_in(dir: &std::path::Path) -> App {
        App::new(
            None,
            Some(Settings::path_in(dir)),
            Some(dir.join("library").join("library.db")),
        )
        .0
    }

    #[test]
    fn navigation_switches_screens() {
        let mut app = app();
        assert_eq!(app.screen, Screen::Library);
        let _ = app.update(Message::Nav(Screen::Pipeline));
        assert_eq!(app.screen, Screen::Pipeline);
        assert_eq!(app.title(), "SignalPlayback — Pipeline");
    }

    #[test]
    fn navigating_to_the_current_screen_is_a_no_op() {
        let mut app = app();
        let _ = app.update(Message::Nav(Screen::Library));
        assert_eq!(app.screen, Screen::Library);
    }

    /// The application no longer opens Iced's built-in themes, so the two it
    /// does open are told apart by what they are rather than by which
    /// variant they are (`src/theme.rs`).
    #[test]
    fn theme_toggles_both_ways() {
        let mut app = app();
        assert!(app.theme().extended_palette().is_dark);
        let _ = app.update(Message::ToggleTheme);
        assert!(!app.theme().extended_palette().is_dark);
        let _ = app.update(Message::ToggleTheme);
        assert!(app.theme().extended_palette().is_dark);
    }

    #[test]
    fn the_theme_shortcut_and_the_settings_screen_agree() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::ToggleTheme);
        assert_eq!(app.settings.theme, ThemeChoice::Light);
        // The Settings screen shows what the shortcut did …
        assert_eq!(app.settings_screen.settings().theme, ThemeChoice::Light);
        // … and it survives a restart.
        drop(app);
        let reopened = app_in(dir.path());
        assert_eq!(reopened.settings.theme, ThemeChoice::Light);
        assert!(!reopened.theme().extended_palette().is_dark);
    }

    #[test]
    fn a_changed_setting_is_saved_and_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::Settings(
            settings_screen::Message::RetentionPicked(RetentionEntry(Retention::Keep(5))),
        ));
        assert_eq!(app.settings.retention, Retention::Keep(5));
        assert_eq!(
            Settings::load(&Settings::path_in(dir.path())).retention,
            Retention::Keep(5)
        );
    }

    #[test]
    fn the_settings_file_says_which_library_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = dir.path().join("chosen.db");
        Settings {
            library_file: Some(chosen.clone()),
            ..Settings::default()
        }
        .save(&Settings::path_in(dir.path()))
        .unwrap();

        let app = app_in(dir.path());
        assert!(app.store.is_some(), "{:?}", app.store_error);
        assert!(chosen.exists(), "the chosen library is the one opened");
        assert!(!dir.path().join("library").join("library.db").exists());
    }

    #[test]
    fn opening_another_library_drops_what_the_old_one_showed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let first = app.store.as_ref().map(|store| store.path().to_path_buf());

        let other = dir.path().join("other.db");
        let _ = app.update(Message::Settings(settings_screen::Message::LibraryChosen(
            Some(other.clone()),
        )));
        let now = app.store.as_ref().map(|store| store.path().to_path_buf());
        assert_ne!(first, now);
        assert_eq!(now, Some(other));
        assert!(
            app.library.summary().is_none(),
            "the tree is reloaded, not kept"
        );
    }

    #[test]
    fn opening_another_library_keeps_the_algorithm_but_not_what_it_ran_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let stages = app.pipeline.stage_count();

        let _ = app.update(Message::Settings(settings_screen::Message::LibraryChosen(
            Some(dir.path().join("other.db")),
        )));
        assert_eq!(
            app.pipeline.stage_count(),
            stages,
            "the stages are the user's"
        );
        assert!(
            app.pipeline.saved_id().is_none(),
            "a row id from the old library means nothing in the new one"
        );
    }

    #[test]
    fn without_a_library_path_the_app_still_boots() {
        let app = app();
        assert!(app.store.is_none());
        assert!(app.store_error.is_some());
    }

    #[test]
    fn a_library_path_opens_and_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib").join("library.db");
        let (app, _task) = App::new(None, None, Some(path.clone()));
        assert!(app.store.is_some(), "{:?}", app.store_error);
        assert!(path.exists());
        drop(app);
    }

    #[test]
    fn an_unopenable_library_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be.
        let path = dir.path().join("library.db");
        std::fs::create_dir_all(&path).unwrap();
        let (app, _task) = App::new(None, None, Some(path));
        assert!(app.store.is_none());
        assert!(app.store_error.is_some());
    }

    #[test]
    fn bytes_format_with_binary_prefixes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
