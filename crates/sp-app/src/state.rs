//! Application state and the root `update`/`view` (`docs/DESIGN.md` §12.2).

use std::path::PathBuf;

use iced::widget::{button, column, container, horizontal_rule, row, scrollable, text, Space};
use iced::{keyboard, Alignment, Element, Length, Subscription, Task, Theme};

use crate::paths;
use crate::screens::{self, Screen, Section};

/// Everything the UI reads.
///
/// Later milestones add the store handle, the stage registry, the library
/// index and the scope state alongside these; the screen state itself moves
/// into the [`Screen`] variants.
#[derive(Debug)]
pub struct App {
    screen: Screen,
    theme: Theme,
    library_root: Option<PathBuf>,
    log_dir: Option<PathBuf>,
}

/// Root message. Screen modules will own nested message types that this
/// delegates to as they land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Nav(Screen),
    ToggleTheme,
}

impl App {
    /// Boots the application. Nothing is read from disk yet — the library is
    /// opened at M1.
    pub fn new(log_dir: Option<PathBuf>) -> (Self, Task<Message>) {
        let library_root = paths::default_library_root();
        tracing::info!(
            library_root = ?library_root,
            log_dir = ?log_dir,
            "SignalPlayback {} starting",
            env!("CARGO_PKG_VERSION"),
        );

        (
            Self {
                screen: Screen::default(),
                theme: Theme::Dark,
                library_root,
                log_dir,
            },
            Task::none(),
        )
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
            }
            Message::ToggleTheme => {
                self.theme = if matches!(self.theme, Theme::Dark) {
                    Theme::Light
                } else {
                    Theme::Dark
                };
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
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
        let content = column![
            self.header(),
            horizontal_rule(1),
            screens::placeholder(self.screen),
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
        let describe = |label: &str, path: Option<&PathBuf>| {
            let value = path.map_or_else(|| "unavailable".to_owned(), |p| p.display().to_string());
            text(format!("{label}: {value}"))
                .size(11)
                .style(text::secondary)
        };

        container(
            row![
                describe("Library", self.library_root.as_ref()),
                Space::with_width(Length::Fixed(24.0)),
                describe("Logs", self.log_dir.as_ref()),
            ]
            .align_y(Alignment::Center),
        )
        .padding([6, 16])
        .width(Length::Fill)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(None).0
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
}
