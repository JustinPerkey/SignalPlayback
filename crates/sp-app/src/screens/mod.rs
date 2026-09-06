//! The application's screens (`docs/DESIGN.md` §12.1).
//!
//! [`Screen`] is the identity used for navigation; a screen's own
//! `State`/`Message`/`update`/`view` live in its module and are owned by the
//! root [`crate::state::App`], so a screen's state survives switching away
//! from it (§12.3). As of M8 every screen in §12.1 is built, so there is no
//! stand-in body any more.

pub mod generate;
pub mod import;
pub mod inspector;
pub mod library;
pub mod pipeline;
pub mod properties;
pub mod results;
pub mod runs;
pub mod scope;
pub mod settings;

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
