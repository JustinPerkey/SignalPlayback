//! The application's screens (`docs/DESIGN.md` §12.1).
//!
//! At M0 a screen is just an identity plus its description: navigation works,
//! the surfaces are empty. Each variant gains its own `State`/`Message`/
//! `update`/`view` module as its milestone lands, at which point [`Screen`]
//! starts carrying that state as a payload (§12.2).

use iced::widget::{column, container, row, text, text::secondary, Space};
use iced::{Element, Length};

use crate::state::Message;

/// Where a screen sits in the navigation rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// Browsing and describing what is already stored.
    Library,
    /// Getting signals into the library.
    Build,
    /// Running algorithms and reading the results.
    Process,
    /// Playback.
    View,
    /// Configuration.
    System,
}

impl Section {
    pub const ALL: [Self; 5] = [
        Self::Library,
        Self::Build,
        Self::Process,
        Self::View,
        Self::System,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Build => "Build",
            Self::Process => "Process",
            Self::View => "View",
            Self::System => "System",
        }
    }

    /// Screens in this section, in rail order.
    #[must_use]
    pub fn screens(self) -> Vec<Screen> {
        Screen::ALL
            .into_iter()
            .filter(|screen| screen.section() == self)
            .collect()
    }
}

/// One top-level surface of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Screen {
    #[default]
    Library,
    Import,
    Generate,
    Pipeline,
    Results,
    Runs,
    Scope,
    Inspector,
    Properties,
    Settings,
}

impl Screen {
    pub const ALL: [Self; 10] = [
        Self::Library,
        Self::Inspector,
        Self::Properties,
        Self::Import,
        Self::Generate,
        Self::Pipeline,
        Self::Results,
        Self::Runs,
        Self::Scope,
        Self::Settings,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Import => "Import",
            Self::Generate => "Generate",
            Self::Pipeline => "Pipeline",
            Self::Results => "Results",
            Self::Runs => "Runs",
            Self::Scope => "Scope",
            Self::Inspector => "Inspector",
            Self::Properties => "Properties",
            Self::Settings => "Settings",
        }
    }

    #[must_use]
    pub const fn section(self) -> Section {
        match self {
            Self::Library | Self::Inspector | Self::Properties => Section::Library,
            Self::Import | Self::Generate => Section::Build,
            Self::Pipeline | Self::Results | Self::Runs => Section::Process,
            Self::Scope => Section::View,
            Self::Settings => Section::System,
        }
    }

    /// What the screen is for, taken from §12.1.
    #[must_use]
    pub const fn purpose(self) -> &'static str {
        match self {
            Self::Library => {
                "Tree of Dataset → Group → Signal / pulse field, with search, property filters \
                 and a sortable detail table. Hosts cross-group pulse search: a field predicate \
                 returns matching pulses from every group."
            }
            Self::Import => {
                "File picker → preview of the headers and first group → column-mapping panel \
                 (which column is the time of arrival and in what unit, which columns bind to \
                 property definitions) → profile save/load → progress with a live error list."
            }
            Self::Generate => {
                "Node tree editor, parameter form, live preview, sweep configuration, preset \
                 browser."
            }
            Self::Pipeline => {
                "Stage palette on the left, ordered stage list in the middle, generated parameter \
                 form on the right. Port validation inline. Run controls with group selection."
            }
            Self::Results => {
                "Group list, stage rail, scope and artifact panes. The main working surface for \
                 algorithm development."
            }
            Self::Runs => {
                "History of runs with pipeline hash, status, timing and assertion results; \
                 promote to baseline; diff two runs."
            }
            Self::Scope => {
                "Playback-focused view of stored signals, with the same stage rail available when \
                 a run is loaded."
            }
            Self::Inspector => {
                "Detail for one signal, pulse field or pulse: full metadata, property editor, \
                 tags, statistics, histogram, and a virtualised value table."
            }
            Self::Properties => "Manage property definitions and property sets.",
            Self::Settings => {
                "Library location, theme, default sample rate, strict/tolerant import, retention \
                 defaults, decimation quality, keyboard map."
            }
        }
    }

    /// The milestone that fills this screen in (§16).
    #[must_use]
    pub const fn milestone(self) -> &'static str {
        match self {
            Self::Library | Self::Properties => "M1 — Store",
            Self::Import => "M2 — Import",
            Self::Generate => "M3 — Generate",
            Self::Scope => "M4 — Playback",
            Self::Pipeline => "M5 — Pipeline",
            Self::Results => "M6 — Results",
            Self::Runs => "M7 — Regression",
            Self::Inspector | Self::Settings => "M8 — Polish",
        }
    }

    /// Digit pressed with the command modifier to reach this screen.
    #[must_use]
    pub fn shortcut(self) -> Option<char> {
        let index = Self::ALL.iter().position(|s| *s == self)?;
        match index {
            0..=8 => char::from_digit(index as u32 + 1, 10),
            9 => Some('0'),
            _ => None,
        }
    }

    /// The screen a command-modified digit selects.
    #[must_use]
    pub fn from_shortcut(digit: char) -> Option<Self> {
        let index = match digit {
            '0' => 9,
            '1'..='9' => digit.to_digit(10)? as usize - 1,
            _ => return None,
        };
        Self::ALL.get(index).copied()
    }
}

/// The stand-in body shown until a screen's milestone lands.
pub fn placeholder(screen: Screen) -> Element<'static, Message> {
    let heading = row![
        text(screen.label()).size(28),
        Space::with_width(Length::Fixed(12.0)),
        container(text(screen.milestone()).size(12))
            .padding([3, 8])
            .style(|theme: &iced::Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.into()),
                    text_color: Some(palette.background.base.text),
                    border: iced::border::rounded(4),
                    ..container::Style::default()
                }
            }),
    ]
    .align_y(iced::Alignment::Center);

    let body = column![
        heading,
        Space::with_height(Length::Fixed(10.0)),
        text(screen.purpose()).size(15),
        Space::with_height(Length::Fixed(24.0)),
        text("Not built yet — navigation, logging and the domain model are what M0 delivers.")
            .size(13)
            .style(secondary),
    ]
    .max_width(680);

    container(body)
        .padding(32)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_screen_is_reachable_from_exactly_one_section() {
        let listed: Vec<Screen> = Section::ALL
            .into_iter()
            .flat_map(Section::screens)
            .collect();
        assert_eq!(listed.len(), Screen::ALL.len());
        for screen in Screen::ALL {
            assert_eq!(
                listed.iter().filter(|s| **s == screen).count(),
                1,
                "{screen:?} is listed once"
            );
        }
    }

    #[test]
    fn shortcuts_round_trip_and_are_unique() {
        let mut seen = Vec::new();
        for screen in Screen::ALL {
            let digit = screen.shortcut().expect("every screen has a shortcut");
            assert_eq!(Screen::from_shortcut(digit), Some(screen));
            assert!(!seen.contains(&digit), "{digit} used twice");
            seen.push(digit);
        }
        assert_eq!(Screen::from_shortcut('x'), None);
    }
}
