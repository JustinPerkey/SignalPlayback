//! Application state and the root `update`/`view` (`docs/DESIGN.md` §12.2).

use std::path::PathBuf;

use iced::widget::{button, column, container, horizontal_rule, row, scrollable, text, Space};
use iced::{keyboard, Alignment, Element, Length, Subscription, Task, Theme};
use sp_store::Store;

use crate::screens::{
    self, generate, import, library, pipeline, properties, results, scope, Screen, Section,
};

/// Everything the UI reads.
///
/// Screen state lives here rather than inside [`Screen`] so it survives
/// navigation (§12.3) — the scope keeps its playhead, viewport and traces
/// while the user is off looking at something else. Later milestones add the
/// stage registry and the active run alongside.
#[derive(Debug)]
pub struct App {
    screen: Screen,
    theme: Theme,
    /// The open library, or `None` when opening it failed.
    store: Option<Store>,
    /// Why the library is not open, for the status bar.
    store_error: Option<String>,
    library: library::State,
    import: import::State,
    generate: generate::State,
    pipeline: pipeline::State,
    properties: properties::State,
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
    Results(results::Message),
    Scope(scope::Message),
}

impl App {
    /// Boots the application and opens the library at `library_file`. A
    /// library that will not open is reported in the status bar rather than
    /// aborting: the app is still useful for looking at the design of the
    /// other screens, and the Settings screen (M8) is where the path changes.
    pub fn new(log_dir: Option<PathBuf>, library_file: Option<PathBuf>) -> (Self, Task<Message>) {
        tracing::info!(
            library = ?library_file,
            log_dir = ?log_dir,
            "SignalPlayback {} starting",
            env!("CARGO_PKG_VERSION"),
        );

        let (store, store_error) = match library_file {
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
        };

        let mut app = Self {
            screen: Screen::default(),
            theme: Theme::Dark,
            store,
            store_error,
            library: library::State::default(),
            import: import::State::default(),
            generate: generate::State::default(),
            pipeline: pipeline::State::default(),
            properties: properties::State::default(),
            results: results::State::default(),
            scope: scope::State::default(),
            log_dir,
        };

        let task = match app.store.clone() {
            Some(store) => Task::batch([
                app.library.load(&store).map(Message::Library),
                app.import.load(&store).map(Message::Import),
                app.generate.load(&store).map(Message::Generate),
                app.pipeline.load(&store).map(Message::Pipeline),
                app.properties.load(&store).map(Message::Properties),
                app.results.load(&store).map(Message::Results),
                app.scope.load(&store).map(Message::Scope),
            ]),
            None => Task::none(),
        };
        (app, task)
    }

    #[must_use]
    pub fn title(&self) -> String {
        format!("SignalPlayback — {}", self.screen.label())
    }

    #[must_use]
    pub fn theme(&self) -> Theme {
        self.theme.clone()
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
                self.theme = if matches!(self.theme, Theme::Dark) {
                    Theme::Light
                } else {
                    Theme::Dark
                };
                Task::none()
            }
            Message::Library(message) => self
                .library
                .update(self.store.as_ref(), message)
                .map(Message::Library),
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
                        self.library.load(&store).map(Message::Library),
                        self.results.load(&store).map(Message::Results),
                        self.scope.load(&store).map(Message::Scope),
                    ]),
                    _ => task,
                }
            }
            Message::Properties(message) => self
                .properties
                .update(self.store.as_ref(), message)
                .map(Message::Properties),
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
            Screen::Results => self.results.view().map(Message::Results),
            Screen::Scope => self.scope.view().map(Message::Scope),
            other => screens::placeholder(other),
        };

        let content = column![
            self.header(),
            horizontal_rule(1),
            container(body).height(Length::Fill),
            horizontal_rule(1),
            self.status_bar(),
        ];

        row![self.nav_rail(), content].height(Length::Fill).into()
    }

    /// Sectioned navigation rail. Sections keep the ten screens legible and
    /// group them by what the user is doing rather than by milestone.
    fn nav_rail(&self) -> Element<'_, Message> {
        // The scrollable's content must size to its contents, so the rail's
        // fixed brand and version rows sit outside it.
        let mut list = column![].width(Length::Fill);
        for section in Section::ALL {
            list = list.push(
                container(text(section.label()).size(11).style(text::secondary)).padding([10, 12]),
            );
            for screen in section.screens() {
                list = list.push(self.nav_button(screen));
            }
        }

        let rail = column![
            container(text("SignalPlayback").size(16))
                .padding([14, 12])
                .width(Length::Fill),
            scrollable(list).height(Length::Fill),
            container(
                text(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(11)
                    .style(text::secondary),
            )
            .padding([10, 12]),
        ]
        .width(Length::Fixed(190.0))
        .height(Length::Fill);

        container(rail)
            .height(Length::Fill)
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.into()),
                    ..container::Style::default()
                }
            })
            .into()
    }

    fn nav_button(&self, screen: Screen) -> Element<'_, Message> {
        let active = self.screen == screen;
        let label = match screen.shortcut() {
            Some(digit) => format!("{}   ⌃{digit}", screen.label()),
            None => screen.label().to_owned(),
        };

        button(text(label).size(14))
            .width(Length::Fill)
            .padding([7.0, 14.0])
            .style(if active {
                button::primary
            } else {
                button::text
            })
            .on_press(Message::Nav(screen))
            .into()
    }

    fn header(&self) -> Element<'_, Message> {
        let theme_label = if matches!(self.theme, Theme::Dark) {
            "Light theme"
        } else {
            "Dark theme"
        };

        container(
            row![
                text(self.screen.label()).size(15),
                Space::with_width(Length::Fill),
                button(text(theme_label).size(12))
                    .padding([5.0, 10.0])
                    .style(button::secondary)
                    .on_press(Message::ToggleTheme),
            ]
            .align_y(Alignment::Center),
        )
        .padding([10, 16])
        .width(Length::Fill)
        .into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        let library: Element<'_, Message> = match (&self.store, &self.store_error) {
            (Some(store), _) => {
                let counts = self.library.summary().map_or_else(String::new, |s| {
                    format!(
                        " · {} dataset{} · {} train{} · {} group{} · {} signal{} · \
                         {} pulse field{} · {}",
                        s.datasets,
                        plural(s.datasets),
                        s.trains,
                        plural(s.trains),
                        s.groups,
                        plural(s.groups),
                        s.signals,
                        plural(s.signals),
                        s.pulse_fields,
                        plural(s.pulse_fields),
                        fmt_bytes(s.blob_bytes),
                    )
                });
                text(format!("Library: {}{counts}", store.path().display()))
                    .size(11)
                    .style(text::secondary)
                    .into()
            }
            (None, Some(error)) => text(format!("Library not open: {error}"))
                .size(11)
                .style(text::danger)
                .into(),
            (None, None) => text("Library: none").size(11).style(text::secondary).into(),
        };

        let logs = self
            .log_dir
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |p| p.display().to_string());

        container(
            row![
                library,
                Space::with_width(Length::Fixed(24.0)),
                text(format!("Logs: {logs}"))
                    .size(11)
                    .style(text::secondary),
            ]
            .align_y(Alignment::Center),
        )
        .padding([6, 16])
        .width(Length::Fill)
        .into()
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Bytes with a binary prefix, one decimal.
fn fmt_bytes(bytes: u64) -> String {
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

    fn app() -> App {
        App::new(None, None).0
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

    #[test]
    fn theme_toggles_both_ways() {
        let mut app = app();
        assert!(matches!(app.theme(), Theme::Dark));
        let _ = app.update(Message::ToggleTheme);
        assert!(matches!(app.theme(), Theme::Light));
        let _ = app.update(Message::ToggleTheme);
        assert!(matches!(app.theme(), Theme::Dark));
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
        let (app, _task) = App::new(None, Some(path.clone()));
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
        let (app, _task) = App::new(None, Some(path));
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
