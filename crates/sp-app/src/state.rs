//! Application state and the root `update`/`view` (`docs/DESIGN.md` §12.2).

use std::path::PathBuf;

use iced::widget::{button, column, container, row, scrollable, stack, text, Space};
use iced::{keyboard, Alignment, Element, Length, Subscription, Task, Theme};
use sp_store::Store;

use crate::actions::Action;
use crate::keymap::{Chord, Key};
use crate::palette;
use crate::screens::settings::fmt_bytes;
use crate::screens::{
    generate, import, inspector, library, pipeline, properties, results, runs, scope, settings,
    Screen, Section,
};
use crate::settings::Settings;
use crate::typography;
use crate::ui;
use crate::watch;

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

    /// The command palette. It is the root's and not a screen's: it reaches
    /// every screen and is drawn over whichever one is showing (§12.5).
    palette: palette::State,
    /// The watched folder, polled from the root for the same reason — it
    /// belongs to the application rather than to the screen it feeds (§7.6).
    watcher: watch::Watcher,
    /// Files currently being dragged over the window, for the drop hint.
    hovering: usize,
}

/// Root message. Each built screen owns a nested message type this
/// delegates to.
#[derive(Debug, Clone)]
pub enum Message {
    Nav(Screen),
    ToggleTheme,
    /// Run a catalogued action, from a key, the palette or a button. One
    /// path, so an action cannot behave differently depending on how it was
    /// reached (`crate::actions`).
    Run(Action),
    /// A key press, resolved against the keymap in `update` rather than in
    /// the subscription — which is the only place the keymap is in scope.
    Key(Chord),
    OpenPalette,
    Palette(palette::Message),
    /// A file is being dragged over the window; one per file.
    FileHovered,
    FileDropped(PathBuf),
    FilesHoveredLeft,
    /// Scan the watched folder.
    WatchTick,
    /// Queue everything in the watched folder, the census included.
    SweepWatchedFolder,
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
            palette: palette::State::default(),
            watcher: watch::Watcher::default(),
            hovering: 0,
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
        self.pipeline.set_libraries(&settings.external_libraries);
        self.pipeline.set_sample_cap(settings.sample_cap.bytes());
        self.results.set_libraries(&settings.external_libraries);
        // Re-adopting the folder takes its census again, which is what makes
        // a file that arrived while the app was shut *adopted* rather than
        // imported on the first tick (§7.6).
        if self.watcher.folder() != settings.watch_folder.as_deref() {
            self.watcher.watch(settings.watch_folder.clone());
        }
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

            // ------------------------------------------------- actions, keys
            Message::Run(action) => {
                // An action acts on its screen, so it goes there first. That
                // is what makes *Play or pause* meaningful from the Library
                // screen, and it is the same for a key, a button and the
                // palette (`crate::actions`).
                if let Some(screen) = action.screen() {
                    self.screen = screen;
                }
                let message = action.message();
                self.update(message)
            }
            Message::Key(chord) => self.on_key(chord),
            Message::OpenPalette => {
                // The same chord shuts it: a palette that will not close with
                // the key that opened it is a trap.
                if self.palette.is_open() {
                    self.palette.close();
                    return Task::none();
                }
                let signals = self.scope.entries().iter().map(|entry| {
                    (
                        entry.signal.id,
                        entry.signal.name.as_str(),
                        entry.group.as_str(),
                    )
                });
                self.palette
                    .open(signals, self.pipeline.stage_descriptors())
                    .map(Message::Palette)
            }
            Message::Palette(message) => {
                let task = self.palette.update(message).map(Message::Palette);
                match self.palette.take_chosen() {
                    Some(entry) => {
                        self.palette.close();
                        if let Some(screen) = entry.screen() {
                            self.screen = screen;
                        }
                        let message = entry.message();
                        Task::batch([task, self.update(message)])
                    }
                    None => task,
                }
            }

            // ------------------------------------------------ dropped files
            Message::FileHovered => {
                self.hovering += 1;
                Task::none()
            }
            Message::FilesHoveredLeft => {
                self.hovering = 0;
                Task::none()
            }
            Message::FileDropped(path) => {
                self.hovering = 0;
                // A drop names a *file*, not a set of framing rules, so it
                // queues and previews rather than importing: the mapping
                // panel is the point of the Import screen (§7.6).
                let task = self
                    .import
                    .enqueue(self.store.as_ref(), [path], import::Source::Dropped)
                    .map(Message::Import);
                self.screen = Screen::Import;
                Task::batch([task])
            }
            Message::WatchTick => {
                let ready = self.watcher.poll();
                if ready.is_empty() {
                    return Task::none();
                }
                // No navigation. An unattended import that moved the user off
                // the screen they were working on would not be unattended.
                self.import
                    .enqueue(self.store.as_ref(), ready, import::Source::Watched)
                    .map(Message::Import)
            }
            Message::SweepWatchedFolder => {
                let files = self.watcher.sweep();
                if files.is_empty() {
                    return Task::none();
                }
                self.import
                    .enqueue(self.store.as_ref(), files, import::Source::Watched)
                    .map(Message::Import)
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
            self.watch_tick(),
            Self::events(),
        ])
    }

    /// Key presses and dropped files, in one subscription.
    ///
    /// It has to be one, and it has to be here: Iced takes a plain `fn` for an
    /// event filter, so nothing about the application — least of all the
    /// keymap — can be captured in it. So the subscription does no deciding at
    /// all. It reports *what key was pressed* and `update` resolves it against
    /// the map for the screen that is showing ([`App::on_key`]), which is also
    /// the only place that knows whether a text field has the keyboard or a
    /// binding is being captured.
    ///
    /// Only key presses no widget took are reported; a character typed into a
    /// focused field is captured by it and never arrives.
    fn events() -> Subscription<Message> {
        use iced::event::{self, Event};
        use iced::window;

        event::listen_with(|event, status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if status == event::Status::Ignored =>
            {
                Chord::from_iced(&key, modifiers).map(Message::Key)
            }
            Event::Window(window::Event::FileHovered(_)) => Some(Message::FileHovered),
            Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path)),
            Event::Window(window::Event::FilesHoveredLeft) => Some(Message::FilesHoveredLeft),
            _ => None,
        })
    }

    /// The watched folder's timer, which exists only while a folder is
    /// watched (§7.6).
    fn watch_tick(&self) -> Subscription<Message> {
        if self.watcher.is_watching() {
            iced::time::every(watch::INTERVAL).map(|_| Message::WatchTick)
        } else {
            Subscription::none()
        }
    }

    /// What a key press means here and now.
    ///
    /// The order is the whole of it. A binding being captured wants the key
    /// itself, or `Ctrl`+`1` could never be rebound — the press would navigate
    /// before it could be recorded. Then the palette, which owns the keyboard
    /// while it is open. Then the keymap, for the screen that is showing.
    fn on_key(&mut self, chord: Chord) -> Task<Message> {
        if self.screen == Screen::Settings && self.settings_screen.rebinding().is_some() {
            let message = if chord.key == Key::Escape {
                settings::Message::StopRebinding
            } else {
                settings::Message::BindCaptured(chord)
            };
            return self.update(Message::Settings(message));
        }

        if self.palette.is_open() {
            let message = match chord.key {
                Key::Escape => palette::Message::Close,
                Key::Up => palette::Message::Move(-1),
                Key::Down => palette::Message::Move(1),
                Key::Enter => palette::Message::Activate,
                // `Ctrl`+`K` again, or anything else the field did not take.
                _ => {
                    return match self.settings.keymap.action_for(chord, self.screen) {
                        Some(Action::OpenPalette) => self.update(Message::OpenPalette),
                        _ => Task::none(),
                    }
                }
            };
            return self.update(Message::Palette(message));
        }

        // A bare letter or digit is a letter or digit wherever something is
        // listening for one. Iced's capture flag catches most of it; the
        // scope's filter is the case it does not, because the canvas keeps
        // the keyboard while the field has the text (§11.4).
        if chord.is_typeable() && !self.accepts_typeable_keys() {
            return Task::none();
        }

        match self.settings.keymap.action_for(chord, self.screen) {
            Some(action) => self.update(Message::Run(action)),
            None => Task::none(),
        }
    }

    /// Whether an unmodified key can be a shortcut on the screen showing.
    fn accepts_typeable_keys(&self) -> bool {
        match self.screen {
            Screen::Scope => self.scope.accepts_transport_keys(),
            _ => true,
        }
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

        let window: Element<'_, Message> =
            row![self.nav_rail(), content].height(Length::Fill).into();

        // The palette is drawn over whatever is showing rather than replacing
        // it: it is a way of reaching the application, not a screen of its
        // own, and seeing the trace behind it is what makes that true.
        match self.palette.view() {
            Some(overlay) => stack![window, overlay.map(Message::Palette)].into(),
            None => window,
        }
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
        // The key shown is the key that is bound, not the key that shipped:
        // the rail is the most-read list of shortcuts in the application, and
        // one that lies about a rebound key is worse than none (§15.11).
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
        .push_maybe(
            self.settings
                .keymap
                .chord(Action::Navigate(screen))
                .map(|chord| {
                    text(chord.label())
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT)
                        .style(ui::dim)
                }),
        )
        .spacing(6)
        .align_y(Alignment::Center);

        button(label)
            .width(Length::Fill)
            .padding([6.0, 14.0])
            .style(ui::selectable(active))
            .on_press(Message::Run(Action::Navigate(screen)))
            .into()
    }

    fn header(&self) -> Element<'_, Message> {
        let theme_label = format!("{} theme", self.settings.theme.toggled().label());

        // The screen name is the page heading, so it is set as one rather
        // than as a slightly larger line of body text. Switching the theme is
        // a preference and not what this screen is for, so it is a quiet
        // command at the far edge.
        // The palette is the one thing in the chrome that reaches everything
        // else, so its key is written out: a shortcut nobody knows about is a
        // shortcut nobody has.
        let palette_label = match self.settings.keymap.chord(Action::OpenPalette) {
            Some(chord) => format!("Commands  {}", chord.label()),
            None => "Commands".to_owned(),
        };

        container(
            row![
                text(self.screen.label())
                    .size(typography::TITLE_SIZE)
                    .font(typography::TITLE),
                Space::with_width(Length::Fill),
                button(
                    text(palette_label)
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL)
                        .style(ui::dim),
                )
                .padding([5.0, 10.0])
                .style(button::text)
                .on_press(Message::OpenPalette),
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
            row![library]
                // A drag over the window is a state, and the status bar is
                // where this application says what state it is in. It is the
                // accent because it is the one moment the window is asking to
                // be dropped on.
                .push_maybe((self.hovering > 0).then(|| {
                    text(format!(
                        "Drop to queue {} file{} for import",
                        self.hovering,
                        if self.hovering == 1 { "" } else { "s" }
                    ))
                    .size(typography::LABEL_SIZE)
                    .style(|theme: &Theme| text::Style {
                        color: Some(crate::theme::tokens(theme).playhead),
                    })
                }))
                .push(Space::with_width(Length::Fill))
                .push_maybe(self.watch_reading())
                .push(ui::caption("Logs"))
                .push(
                    text(logs)
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT)
                        .style(ui::dim),
                )
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([6, 20])
        .width(Length::Fill)
        .into()
    }

    /// What the watched folder is doing, when there is one (§7.6).
    ///
    /// A folder importing on its own is a thing happening to the library
    /// without anyone asking, so the window says so at all times rather than
    /// only on the screen it feeds.
    fn watch_reading(&self) -> Option<Element<'_, Message>> {
        let folder = self.watcher.folder()?;
        let queued = self.import.queued();
        let reading = if self.import.is_running() && queued > 0 {
            format!("{} · importing, {queued} queued", folder.display())
        } else if self.import.is_running() {
            format!("{} · importing", folder.display())
        } else {
            format!(
                "{} · {} file(s) accounted for",
                folder.display(),
                self.watcher.known()
            )
        };
        Some(
            row![
                ui::caption("Watching"),
                text(reading)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        )
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
    fn every_screen_in_the_rail_can_be_navigated_to_and_drawn() {
        // The rail, the header and the status bar are rebuilt for every
        // screen, so each of the ten is opened and drawn in turn.
        let dir = tempfile::tempdir().unwrap();
        let mut opened = app_in(dir.path());
        for screen in Screen::ALL {
            let _ = opened.update(Message::Nav(screen));
            assert_eq!(opened.screen, screen);
            assert_eq!(
                opened.title(),
                format!("SignalPlayback — {}", screen.label())
            );
            let _ = opened.view();
        }

        // And with no library open, which is what the status bar reports.
        let mut app = app();
        assert!(app.store.is_none());
        for screen in Screen::ALL {
            let _ = app.update(Message::Nav(screen));
            let _ = app.view();
        }
    }

    #[test]
    fn the_theme_button_in_the_header_toggles_the_same_setting_the_rail_does() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let before = app.settings.theme;
        let _ = app.update(Message::ToggleTheme);
        assert_ne!(app.settings.theme, before);
        let _ = app.view();
    }

    #[test]
    fn inspecting_from_the_library_opens_the_inspector_on_that_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        assert_eq!(app.screen, Screen::Library);

        let target = inspector::Target::Signal(sp_core::SignalId::new(7));
        let _ = app.update(Message::Library(library::Message::Inspect(target)));
        assert_eq!(
            app.screen,
            Screen::Inspector,
            "Inspect is a navigation as well as a selection"
        );
        let _ = app.view();
    }

    #[test]
    fn opening_a_run_from_the_history_moves_to_the_results_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::Nav(Screen::Runs));

        let _ = app.update(Message::Runs(runs::Message::Open(sp_core::RunId::new(3))));
        assert_eq!(app.screen, Screen::Results);

        // A diff is the same handover, carrying the run to measure against.
        let _ = app.update(Message::Nav(Screen::Runs));
        let _ = app.update(Message::Runs(runs::Message::MarkAgainst(
            sp_core::RunId::new(2),
        )));
        let _ = app.update(Message::Runs(runs::Message::Diff(sp_core::RunId::new(3))));
        assert_eq!(app.screen, Screen::Results);
    }

    #[test]
    fn a_screen_that_asks_for_nothing_leaves_the_user_where_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::Nav(Screen::Pipeline));
        // A plain edit on another screen is not a navigation.
        let _ = app.update(Message::Library(library::Message::SearchChanged(
            "rf".to_owned(),
        )));
        assert_eq!(app.screen, Screen::Pipeline);
    }

    #[test]
    fn a_setting_changed_on_one_screen_reaches_the_screens_that_work_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        // Typing in the box changes nothing until the field is submitted.
        let _ = app.update(Message::Settings(settings_screen::Message::RateChanged(
            "96000".to_owned(),
        )));
        assert_ne!(app.settings.default_sample_rate_hz, 96_000.0);

        let _ = app.update(Message::Settings(settings_screen::Message::RateCommitted));
        assert_eq!(app.settings.default_sample_rate_hz, 96_000.0);
        assert_eq!(
            app.generate.sample_rate_hz(),
            96_000.0,
            "the Generate screen opens on the new default"
        );
        assert_eq!(
            Settings::load(&Settings::path_in(dir.path())).default_sample_rate_hz,
            96_000.0,
            "and it is on disk before the app is closed"
        );
    }

    // -----------------------------------------------------------------------
    // M13 — the keyboard map, the palette, the drop target, the watched folder
    // -----------------------------------------------------------------------

    /// The `Ctrl`+digit rail and `Ctrl`+`T` work exactly as they did, and now
    /// they go through the map rather than through a `match`.
    #[test]
    fn the_keys_v1_had_still_do_what_they_did() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());

        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('6'))));
        assert_eq!(app.screen, Screen::Pipeline);
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('0'))));
        assert_eq!(app.screen, Screen::Settings);

        let before = app.settings.theme;
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('t'))));
        assert_ne!(app.settings.theme, before);

        // A chord nothing is bound to does nothing at all.
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('j'))));
        assert_eq!(app.screen, Screen::Settings);
    }

    /// A screen-scoped key fires only on its screen, which is what lets
    /// `Space` be the transport on two screens and a nothing elsewhere.
    #[test]
    fn a_transport_key_belongs_to_the_screen_it_acts_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let space = Chord::plain(Key::Space);

        assert_eq!(app.screen, Screen::Library);
        let _ = app.update(Message::Key(space));
        assert_eq!(
            app.screen,
            Screen::Library,
            "space on the Library screen is not a transport key"
        );

        let _ = app.update(Message::Nav(Screen::Scope));
        let _ = app.update(Message::Key(space));
        let _ = app.update(Message::Key(Chord::plain(Key::Char('['))));
        let _ = app.update(Message::Key(Chord::plain(Key::Home)));
        let _ = app.view();

        let _ = app.update(Message::Nav(Screen::Results));
        let _ = app.update(Message::Key(Chord::plain(Key::Right)));
        let _ = app.update(Message::Key(Chord::plain(Key::Left)));
        let _ = app.view();
    }

    /// Typing in the scope's filter is typing, not transport (§11.4).
    #[test]
    fn a_bare_key_is_a_key_while_a_field_has_the_keyboard() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::Nav(Screen::Scope));
        assert!(app.accepts_typeable_keys());

        let _ = app.update(Message::Scope(scope::Message::FilterChanged("to".into())));
        assert!(!app.accepts_typeable_keys());
        // A letter is suppressed; a named key is not a character anyone is
        // typing, so it still acts.
        let _ = app.update(Message::Key(Chord::plain(Key::Char('['))));
        let _ = app.update(Message::Key(Chord::plain(Key::Space)));
        let _ = app.view();
    }

    #[test]
    fn a_rebound_key_takes_effect_and_the_old_one_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::Nav(Screen::Settings));

        // Bind Ctrl+9 to the Library, which Ctrl+1 reaches by default. The
        // Settings screen has to take the key before the map does, or the
        // press would navigate away instead of being recorded.
        let _ = app.update(Message::Settings(settings::Message::Rebind(
            Action::Navigate(Screen::Library),
        )));
        assert!(app.settings_screen.rebinding().is_some());
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('9'))));
        assert_eq!(
            app.screen,
            Screen::Settings,
            "the press was a binding, not a navigation"
        );
        assert!(app.settings_screen.rebinding().is_none());
        assert_eq!(
            app.settings.keymap.chord(Action::Navigate(Screen::Library)),
            Some(Chord::ctrl(Key::Char('9')))
        );

        // The new key works …
        let _ = app.update(Message::Nav(Screen::Runs));
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('9'))));
        assert_eq!(app.screen, Screen::Library);
        // … and the old one no longer reaches it.
        let _ = app.update(Message::Nav(Screen::Runs));
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('1'))));
        assert_eq!(app.screen, Screen::Runs);

        // And it survives a restart, because it is in the settings file.
        drop(app);
        let reopened = app_in(dir.path());
        assert_eq!(
            reopened
                .settings
                .keymap
                .chord(Action::Navigate(Screen::Library)),
            Some(Chord::ctrl(Key::Char('9')))
        );
    }

    #[test]
    fn escape_abandons_a_rebinding_and_unbinding_leaves_the_action_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let _ = app.update(Message::Nav(Screen::Settings));
        let _ = app.update(Message::Settings(settings::Message::Rebind(
            Action::ToggleTheme,
        )));
        let _ = app.update(Message::Key(Chord::plain(Key::Escape)));
        assert!(app.settings_screen.rebinding().is_none());
        assert_eq!(
            app.settings.keymap.chord(Action::ToggleTheme),
            Some(Chord::ctrl(Key::Char('t'))),
            "escape changed nothing"
        );

        let _ = app.update(Message::Settings(settings::Message::Unbind(
            Action::ToggleTheme,
        )));
        assert_eq!(app.settings.keymap.chord(Action::ToggleTheme), None);
        let before = app.settings.theme;
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('t'))));
        assert_eq!(app.settings.theme, before, "the key does nothing now");
        // But the action itself is still there to be run.
        let _ = app.update(Message::Run(Action::ToggleTheme));
        assert_ne!(app.settings.theme, before);
    }

    /// The exit criterion's second clause, at the root: every action in the
    /// catalogue can be run, and running it leaves an application that still
    /// draws. The palette's own tests hold the *listing* half; this holds that
    /// the thing it lists actually works.
    #[test]
    fn every_action_in_the_palette_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        for action in Action::all() {
            let _ = app.update(Message::Run(action));
            if let Some(screen) = action.screen() {
                assert_eq!(
                    app.screen,
                    screen,
                    "{} should have taken us to its own screen",
                    action.id()
                );
            }
            let _ = app.view();
            // The palette must not be left open by an action that is not
            // about the palette, or the next one would be typed into it.
            if action != Action::OpenPalette {
                assert!(
                    !app.palette.is_open(),
                    "{} left the palette open",
                    action.id()
                );
            } else {
                app.palette.close();
            }
        }
    }

    #[test]
    fn the_palette_opens_over_the_application_and_runs_what_is_picked() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());

        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('k'))));
        assert!(app.palette.is_open());
        let _ = app.view();

        // While it is open its own keys walk the list and the rest of the map
        // is out of the way.
        let _ = app.update(Message::Key(Chord::plain(Key::Down)));
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('6'))));
        assert_eq!(app.screen, Screen::Library, "the palette has the keyboard");

        // Find an action and run it; the palette closes and the action lands.
        let _ = app.update(Message::Palette(palette::Message::QueryChanged(
            Action::PipelineRun.label(),
        )));
        let _ = app.update(Message::Key(Chord::plain(Key::Enter)));
        assert!(!app.palette.is_open());
        assert_eq!(
            app.screen,
            Screen::Pipeline,
            "a screen-scoped action goes to its screen first"
        );

        // The same chord shuts it again.
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('k'))));
        assert!(app.palette.is_open());
        let _ = app.update(Message::Key(Chord::ctrl(Key::Char('k'))));
        assert!(!app.palette.is_open());

        // And escape does too.
        let _ = app.update(Message::OpenPalette);
        let _ = app.update(Message::Key(Chord::plain(Key::Escape)));
        assert!(!app.palette.is_open());
    }

    #[test]
    fn the_palette_lists_the_stages_the_pipeline_knows_about() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let stages = app.pipeline.stage_count();

        let _ = app.update(Message::OpenPalette);
        let _ = app.update(Message::Palette(palette::Message::QueryChanged(
            "biquad".to_owned(),
        )));
        let _ = app.update(Message::Palette(palette::Message::Activate));
        assert_eq!(app.screen, Screen::Pipeline);
        assert_eq!(
            app.pipeline.stage_count(),
            stages + 1,
            "picking a stage adds it to the algorithm"
        );
    }

    /// A drop is the user naming a file, so it queues it and shows it — and
    /// does not import it, because the mapping panel is what the Import
    /// screen is for (§7.6).
    #[test]
    fn a_dropped_file_queues_for_import_and_opens_the_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let capture = dir.path().join("capture.csv");
        std::fs::write(&capture, SAMPLE).unwrap();

        // The hint while it is over the window.
        let _ = app.update(Message::FileHovered);
        let _ = app.update(Message::FileHovered);
        assert_eq!(app.hovering, 2);
        let _ = app.view();

        let _ = app.update(Message::FileDropped(capture));
        assert_eq!(app.hovering, 0);
        assert_eq!(app.screen, Screen::Import);
        assert_eq!(app.import.queued(), 1);
        assert!(!app.import.is_running(), "a drop waits to be told");
        let _ = app.view();

        // A drag that leaves without dropping clears the hint.
        let _ = app.update(Message::FileHovered);
        let _ = app.update(Message::FilesHoveredLeft);
        assert_eq!(app.hovering, 0);
    }

    #[test]
    fn dropping_a_folder_queues_the_captures_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_in(dir.path());
        let drop = dir.path().join("captures");
        std::fs::create_dir_all(&drop).unwrap();
        std::fs::write(drop.join("a.csv"), SAMPLE).unwrap();
        std::fs::write(drop.join("b.csv"), SAMPLE).unwrap();
        std::fs::write(drop.join("notes.md"), "not a capture").unwrap();

        let _ = app.update(Message::FileDropped(drop));
        assert_eq!(app.import.queued(), 2, "and not the notes");
    }

    /// **The exit criterion.** A folder is watched; a capture is written into
    /// it; and with nobody pressing anything the file is imported — right
    /// through to a dataset in the library.
    ///
    /// The second half runs the request the queue built, on the store the
    /// queue was handed. It is the same `ImportRequest` the real task carries
    /// (`import::State::request_for_source`), executed here rather than on the
    /// blocking pool, because a unit test has no Iced runtime to drain a
    /// `Task` with.
    #[test]
    fn a_watched_folder_imports_without_user_action() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("incoming");
        std::fs::create_dir_all(&watched).unwrap();
        // A capture that was already there when the folder was adopted.
        std::fs::write(watched.join("last-week.csv"), SAMPLE).unwrap();

        let mut app = app_in(dir.path());
        let _ = app.update(Message::Settings(settings::Message::WatchFolderChosen(
            Some(watched.clone()),
        )));
        assert_eq!(app.watcher.folder(), Some(watched.as_path()));

        // Nothing happens to what was already there, however long it waits.
        for _ in 0..4 {
            let _ = app.update(Message::WatchTick);
        }
        assert_eq!(app.import.queued(), 0);
        assert!(!app.import.is_running());

        // Now a capture arrives.
        let fresh = watched.join("bench-run-7.csv");
        std::fs::write(&fresh, SAMPLE).unwrap();

        let _ = app.update(Message::WatchTick);
        assert!(
            !app.import.is_running(),
            "the first scan only starts watching it — a file may still be being written"
        );
        let _ = app.update(Message::WatchTick);
        assert!(
            app.import.is_running(),
            "and the second imports it, with nobody having pressed anything"
        );
        assert_eq!(app.screen, Screen::Library, "and without moving the user");
        let _ = app.view();

        // What the queue is importing, imported: the request it built, run
        // against the library it was handed.
        let request = app
            .import
            .request_for_source(&fresh, import::Source::Watched);
        let store = app.store.as_ref().expect("the library is open");
        let file = fresh.clone();
        let report = store
            .write(move |conn| {
                Ok(sp_csv::ingest::import_file(
                    conn,
                    &file,
                    &request,
                    &sp_csv::ImportControl::new(),
                ))
            })
            .unwrap()
            .expect("the watched file imports");
        assert_eq!(report.groups, 1);

        let datasets = store.read(sp_store::library::list_datasets).unwrap();
        assert!(
            datasets.iter().any(|dataset| dataset.name == "bench-run-7"),
            "the dataset is named after the file that arrived: {:?}",
            datasets.iter().map(|d| &d.name).collect::<Vec<_>>(),
        );

        // The watcher does not offer it again.
        for _ in 0..4 {
            let _ = app.update(Message::WatchTick);
        }
        assert_eq!(app.import.queued(), 0);
    }

    #[test]
    fn the_watched_folder_is_a_setting_that_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("incoming");
        std::fs::create_dir_all(&watched).unwrap();

        let mut app = app_in(dir.path());
        assert!(!app.watcher.is_watching());
        let _ = app.update(Message::Settings(settings::Message::WatchFolderChosen(
            Some(watched.clone()),
        )));
        drop(app);

        let mut reopened = app_in(dir.path());
        assert_eq!(reopened.watcher.folder(), Some(watched.as_path()));
        let _ = reopened.view();

        let _ = reopened.update(Message::Run(Action::SettingsStopWatching));
        assert!(!reopened.watcher.is_watching());
        assert_eq!(reopened.settings.watch_folder, None);
    }

    /// The census leaves what is already there alone; this is the action that
    /// says *no, take those too*.
    #[test]
    fn sweeping_the_watched_folder_queues_what_the_census_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("incoming");
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::write(watched.join("one.csv"), SAMPLE).unwrap();
        std::fs::write(watched.join("two.csv"), SAMPLE).unwrap();

        let mut app = app_in(dir.path());
        let _ = app.update(Message::Settings(settings::Message::WatchFolderChosen(
            Some(watched),
        )));
        let _ = app.update(Message::WatchTick);
        assert_eq!(app.import.queued(), 0);

        let _ = app.update(Message::Run(Action::ImportSweepWatched));
        assert_eq!(app.screen, Screen::Import);
        assert!(
            app.import.is_running(),
            "one is being read and the other is behind it"
        );
        assert_eq!(app.import.queued(), 1);

        // And with nothing to sweep it is a no-op rather than an error.
        let _ = app.update(Message::Run(Action::SettingsStopWatching));
        let _ = app.update(Message::Run(Action::ImportSweepWatched));
    }

    /// A grouped-block CSV with one group, small enough to import inside a
    /// test (§7.1).
    const SAMPLE: &str = "Skip Row
groupID,total time, count, info
time, pulse width, power, band
1, 1000, 2, info
10, 100, 100.5, L
20, 100, 100.5, S
";

    #[test]
    fn bytes_format_with_binary_prefixes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
