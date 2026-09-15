//! Chords, and the map from an action to the chord that runs it
//! (`docs/DESIGN.md` §12.4, §15.11).
//!
//! The v1 map was fixed in the source: `Ctrl`+digit navigated, `Ctrl`+`T`
//! toggled the theme, and the transport keys were spelled out in a `match` in
//! the root subscription. This module is the map made data, so the Settings
//! screen can edit it and `settings.json` can carry it.
//!
//! Two decisions shape the file format. **Only what differs from the default
//! is written**: a build that adds an action gets its default binding rather
//! than coming up unbound in a file written before the action existed, and a
//! user who has changed one key does not inherit a snapshot of every other.
//! And **a binding that will not parse is dropped, not fatal** — the same rule
//! the rest of [`crate::settings`] follows, for the same reason: losing a
//! preference must never cost the user their application.
//!
//! `ctrl` in a chord means the platform's command modifier — `Ctrl` on Windows
//! and Linux, `Cmd` on macOS — because that is what Iced reports through
//! [`iced::keyboard::Modifiers::command`] and what the platform's own menus
//! use.

use std::collections::BTreeMap;
use std::fmt;

use iced::keyboard::{self, key::Named};
use serde::{Deserialize, Serialize};

use crate::actions::Action;

/// A key that can be bound, named independently of Iced's own spelling.
///
/// The set is deliberately small: the keys a bench user reaches for, plus the
/// digits and letters. A key this does not name cannot be bound, which is
/// better than a settings file full of `Key::Named(BrowserFavorites)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A printable character, held lowercase so `K` and `k` are one key and
    /// the shift flag is the only thing that tells them apart.
    Char(char),
    Space,
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    /// `F1`…`F12`.
    Function(u8),
}

impl Key {
    /// The names a chord string may use for this key, first one canonical.
    const NAMED: [(&'static str, Self); 15] = [
        ("space", Self::Space),
        ("enter", Self::Enter),
        ("escape", Self::Escape),
        ("tab", Self::Tab),
        ("backspace", Self::Backspace),
        ("delete", Self::Delete),
        ("insert", Self::Insert),
        ("home", Self::Home),
        ("end", Self::End),
        ("pageup", Self::PageUp),
        ("pagedown", Self::PageDown),
        ("left", Self::Left),
        ("right", Self::Right),
        ("up", Self::Up),
        ("down", Self::Down),
    ];

    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_ascii_lowercase();
        if let Some(named) = Self::NAMED
            .iter()
            .find_map(|(name, key)| (*name == text).then_some(*key))
        {
            return Some(named);
        }
        if let Some(number) = text.strip_prefix('f') {
            if let Ok(index) = number.parse::<u8>() {
                return (1..=12).contains(&index).then_some(Self::Function(index));
            }
        }
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(Self::Char(c)),
            _ => None,
        }
    }

    /// The key an Iced key press is bound as, or `None` for one that cannot be
    /// bound.
    #[must_use]
    pub fn from_iced(key: &keyboard::Key) -> Option<Self> {
        match key.as_ref() {
            keyboard::Key::Character(text) => {
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(Self::Char(c.to_ascii_lowercase())),
                    _ => None,
                }
            }
            keyboard::Key::Named(named) => Some(match named {
                Named::Space => Self::Space,
                Named::Enter => Self::Enter,
                Named::Escape => Self::Escape,
                Named::Tab => Self::Tab,
                Named::Backspace => Self::Backspace,
                Named::Delete => Self::Delete,
                Named::Insert => Self::Insert,
                Named::Home => Self::Home,
                Named::End => Self::End,
                Named::PageUp => Self::PageUp,
                Named::PageDown => Self::PageDown,
                Named::ArrowLeft => Self::Left,
                Named::ArrowRight => Self::Right,
                Named::ArrowUp => Self::Up,
                Named::ArrowDown => Self::Down,
                Named::F1 => Self::Function(1),
                Named::F2 => Self::Function(2),
                Named::F3 => Self::Function(3),
                Named::F4 => Self::Function(4),
                Named::F5 => Self::Function(5),
                Named::F6 => Self::Function(6),
                Named::F7 => Self::Function(7),
                Named::F8 => Self::Function(8),
                Named::F9 => Self::Function(9),
                Named::F10 => Self::Function(10),
                Named::F11 => Self::Function(11),
                Named::F12 => Self::Function(12),
                _ => return None,
            }),
            keyboard::Key::Unidentified => None,
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(c) => write!(f, "{c}"),
            Self::Function(index) => write!(f, "f{index}"),
            other => {
                let name = Self::NAMED
                    .iter()
                    .find_map(|(name, key)| (key == other).then_some(*name))
                    .unwrap_or("?");
                f.write_str(name)
            }
        }
    }
}

/// A key with the modifiers held down with it.
///
/// `PartialOrd` is derived so a list of chords has a stable order; it carries
/// no meaning beyond that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    pub key: Key,
    /// The platform command modifier: `Ctrl`, or `Cmd` on macOS.
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Chord {
    #[must_use]
    pub const fn plain(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    #[must_use]
    pub const fn ctrl(key: Key) -> Self {
        Self {
            key,
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    /// The chord a key press is, or `None` for a key that cannot be bound.
    #[must_use]
    pub fn from_iced(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<Self> {
        Some(Self {
            key: Key::from_iced(key)?,
            ctrl: modifiers.command(),
            alt: modifiers.alt(),
            // A shifted character arrives as the shifted character itself, so
            // recording the flag as well would make `Chord::parse("?")` and a
            // pressed `?` two different chords on a keyboard where `?` is
            // `Shift`+`/`. The flag is kept only for a named key, where the
            // character does not change.
            shift: modifiers.shift() && !matches!(key.as_ref(), keyboard::Key::Character(_)),
        })
    }

    /// Whether this chord can be typed into a text field, and so must not be
    /// taken as a shortcut while one has the keyboard.
    ///
    /// A bare letter is a letter; `Ctrl`+that letter is a command.
    #[must_use]
    pub const fn is_typeable(self) -> bool {
        !self.ctrl && !self.alt && matches!(self.key, Key::Char(_))
    }

    fn parse(text: &str) -> Option<Self> {
        let mut chord = Self::plain(Key::Space);
        let mut key = None;
        for part in text.split('+') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "command" | "super" | "meta" => chord.ctrl = true,
                "alt" | "option" => chord.alt = true,
                "shift" => chord.shift = true,
                _ => key = Some(Key::parse(part)?),
            }
        }
        chord.key = key?;
        Some(chord)
    }

    /// How the chord reads on screen: `Ctrl K`, `Shift Home`, `Space`.
    ///
    /// The separator is a space rather than a `+` because this is a label on a
    /// key, in the condensed face, and `Ctrl+Shift+PageDown` is a word.
    #[must_use]
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_owned());
        }
        if self.alt {
            parts.push("Alt".to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        parts.push(match self.key {
            Key::Char(c) => c.to_ascii_uppercase().to_string(),
            Key::Function(index) => format!("F{index}"),
            named => {
                let mut text = named.to_string();
                if let Some(first) = text.get(0..1) {
                    let upper = first.to_ascii_uppercase();
                    text.replace_range(0..1, &upper);
                }
                text
            }
        });
        parts.join(" ")
    }
}

impl fmt::Display for Chord {
    /// The form written to `settings.json`: `ctrl+shift+k`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.alt {
            f.write_str("alt+")?;
        }
        if self.shift {
            f.write_str("shift+")?;
        }
        write!(f, "{}", self.key)
    }
}

/// The map from an action to the chord that runs it.
///
/// Serialised as the *overrides* only — `{"scope.play_pause": "p"}` — with an
/// empty string meaning the action has been deliberately unbound. Everything
/// absent takes [`Action::default_chord`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keymap {
    overrides: BTreeMap<String, String>,
}

impl Keymap {
    /// The chord bound to `action`, whether by default or by the user.
    #[must_use]
    pub fn chord(&self, action: Action) -> Option<Chord> {
        match self.overrides.get(action.id()) {
            // An override that will not parse is not a binding. It is left in
            // the file — a typo is the user's text and theirs to fix — but it
            // never silently becomes some other key.
            Some(text) => Chord::parse(text),
            None => action.default_chord(),
        }
    }

    /// Binds `action` to `chord`, or unbinds it with `None`.
    pub fn bind(&mut self, action: Action, chord: Option<Chord>) {
        match chord {
            Some(chord) if action.default_chord() == Some(chord) => {
                // Back to the default: drop the override rather than write a
                // copy of it, so a later build that moves the default moves
                // this binding with it.
                self.overrides.remove(action.id());
            }
            Some(chord) => {
                self.overrides
                    .insert(action.id().to_owned(), chord.to_string());
            }
            None if action.default_chord().is_none() => {
                self.overrides.remove(action.id());
            }
            None => {
                self.overrides.insert(action.id().to_owned(), String::new());
            }
        }
    }

    /// Whether `action` is bound to something other than its default.
    #[must_use]
    pub fn is_rebound(&self, action: Action) -> bool {
        self.overrides.contains_key(action.id())
    }

    /// Puts every binding back to its default.
    pub fn reset(&mut self) {
        self.overrides.clear();
    }

    /// The action `chord` runs on `screen`, if any.
    ///
    /// A screen-scoped binding wins over a global one with the same chord: the
    /// screen the key was pressed on is the more specific answer, and it is
    /// what lets `Space` play on the Scope screen while meaning nothing
    /// anywhere else. Between two actions of equal specificity the earlier one
    /// in [`Action::ALL`] wins, so a conflict resolves the same way every
    /// time — and is reported on the Settings screen rather than left as a
    /// surprise.
    #[must_use]
    pub fn action_for(&self, chord: Chord, screen: crate::screens::Screen) -> Option<Action> {
        let matches = |wanted: Option<crate::screens::Screen>| {
            Action::all()
                .find(|action| action.screen() == wanted && self.chord(*action) == Some(chord))
        };
        matches(Some(screen)).or_else(|| matches(None))
    }

    /// Bindings one chord cannot both reach, as `(kept, shadowed)` pairs.
    ///
    /// There are two ways to lose an action to a key, and both are here
    /// because both surprise the same way:
    ///
    /// - **Two actions in one scope.** The earlier one in [`Action::ALL`]
    ///   wins, and the later one is not reachable by that key at all.
    /// - **A screen's own key over a global one.** [`Self::action_for`]
    ///   prefers the screen, so binding `Ctrl`+`T` to *Run the pipeline* means
    ///   `Ctrl`+`T` stops toggling the theme *while the Pipeline screen is
    ///   showing*. It still works everywhere else, and that is exactly the
    ///   sort of thing a user finds out at the worst moment if nobody says it.
    ///
    /// Two screen-scoped actions on different screens are not a conflict:
    /// that is the mechanism, not a collision — it is how `Space` plays on
    /// both the Scope and the Results screen.
    #[must_use]
    pub fn conflicts(&self) -> Vec<(Action, Action)> {
        let mut conflicts = Vec::new();
        for (index, action) in Action::all().enumerate() {
            let Some(chord) = self.chord(action) else {
                continue;
            };
            let kept = Action::all()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .find(|(other_index, other)| {
                    self.chord(*other) == Some(chord)
                        && match (other.screen(), action.screen()) {
                            (theirs, ours) if theirs == ours => *other_index < index,
                            (Some(_), None) => true,
                            _ => false,
                        }
                });
            if let Some((_, kept)) = kept {
                conflicts.push((kept, action));
            }
        }
        conflicts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::Screen;

    #[test]
    fn a_chord_round_trips_through_its_written_form() {
        for text in [
            "ctrl+k",
            "ctrl+shift+k",
            "ctrl+alt+shift+f5",
            "space",
            "left",
            "ctrl+0",
            "[",
        ] {
            let chord = Chord::parse(text).unwrap_or_else(|| panic!("{text} parses"));
            assert_eq!(chord.to_string(), text, "{text} writes back as itself");
        }
    }

    #[test]
    fn modifier_spellings_and_case_are_all_accepted() {
        let wanted = Chord::ctrl(Key::Char('k'));
        for text in ["ctrl+k", "Ctrl+K", "control+k", "cmd+k", "COMMAND + k"] {
            assert_eq!(Chord::parse(text), Some(wanted), "{text}");
        }
        assert_eq!(Chord::parse("f12"), Some(Chord::plain(Key::Function(12))));
        // Not a key this map can name, and not a key at all.
        assert_eq!(Chord::parse("f13"), None);
        assert_eq!(Chord::parse("ctrl+"), None);
        assert_eq!(Chord::parse("ctrl+nonsense"), None);
    }

    #[test]
    fn a_chord_reads_as_keys_on_a_keyboard() {
        assert_eq!(Chord::ctrl(Key::Char('k')).label(), "Ctrl K");
        assert_eq!(Chord::plain(Key::Space).label(), "Space");
        assert_eq!(
            Chord {
                key: Key::PageDown,
                ctrl: true,
                alt: false,
                shift: true,
            }
            .label(),
            "Ctrl Shift Pagedown"
        );
    }

    #[test]
    fn a_letter_is_typeable_and_a_command_is_not() {
        assert!(Chord::plain(Key::Char('p')).is_typeable());
        assert!(!Chord::ctrl(Key::Char('p')).is_typeable());
        assert!(!Chord::plain(Key::Space).is_typeable());
    }

    #[test]
    fn an_iced_key_press_becomes_the_chord_it_is() {
        let ctrl = keyboard::Modifiers::CTRL;
        assert_eq!(
            Chord::from_iced(&keyboard::Key::Character("K".into()), ctrl),
            Some(Chord::ctrl(Key::Char('k'))),
            "a shifted letter is the letter, and the case is not a second key"
        );
        assert_eq!(
            Chord::from_iced(
                &keyboard::Key::Named(Named::Home),
                keyboard::Modifiers::SHIFT
            ),
            Some(Chord {
                key: Key::Home,
                ctrl: false,
                alt: false,
                shift: true,
            }),
            "shift is kept for a named key, whose identity it does not change"
        );
        assert_eq!(
            Chord::from_iced(
                &keyboard::Key::Named(Named::BrowserBack),
                keyboard::Modifiers::empty()
            ),
            None
        );
    }

    #[test]
    fn the_default_map_is_the_one_the_fixed_map_was() {
        let map = Keymap::default();
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('t')), Screen::Library),
            Some(Action::ToggleTheme)
        );
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('1')), Screen::Library),
            Some(Action::Navigate(Screen::Library))
        );
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('0')), Screen::Library),
            Some(Action::Navigate(Screen::Settings))
        );
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('k')), Screen::Library),
            Some(Action::OpenPalette)
        );
        assert!(map.conflicts().is_empty(), "{:?}", map.conflicts());
    }

    #[test]
    fn a_screen_scoped_key_only_fires_on_its_screen() {
        let map = Keymap::default();
        let space = Chord::plain(Key::Space);
        assert_eq!(
            map.action_for(space, Screen::Scope),
            Some(Action::ScopeToggle)
        );
        assert_eq!(
            map.action_for(space, Screen::Results),
            Some(Action::ResultsToggle),
            "the same key plays on both, through the action that belongs to each"
        );
        assert_eq!(map.action_for(space, Screen::Library), None);
    }

    #[test]
    fn rebinding_and_unbinding_survive_the_file() {
        let mut map = Keymap::default();
        map.bind(Action::ScopeToggle, Some(Chord::plain(Key::Char('p'))));
        map.bind(Action::ToggleTheme, None);

        let json = serde_json::to_string(&map).unwrap();
        let read: Keymap = serde_json::from_str(&json).unwrap();
        assert_eq!(read, map);
        assert_eq!(
            read.chord(Action::ScopeToggle),
            Some(Chord::plain(Key::Char('p')))
        );
        assert_eq!(
            read.chord(Action::ToggleTheme),
            None,
            "unbound stays unbound"
        );
        assert_eq!(
            read.action_for(Chord::plain(Key::Char('p')), Screen::Scope),
            Some(Action::ScopeToggle)
        );
    }

    #[test]
    fn only_the_overrides_are_written() {
        let mut map = Keymap::default();
        map.bind(Action::ScopeToggle, Some(Chord::plain(Key::Char('p'))));
        assert_eq!(
            serde_json::to_string(&map).unwrap(),
            r#"{"scope.play_pause":"p"}"#,
            "an unchanged binding is the default's to move, not the file's to pin"
        );

        // Bound back to what it was, the override goes away again.
        map.bind(Action::ScopeToggle, Action::ScopeToggle.default_chord());
        assert!(!map.is_rebound(Action::ScopeToggle));
        assert_eq!(serde_json::to_string(&map).unwrap(), "{}");
    }

    #[test]
    fn a_binding_that_will_not_parse_is_dropped_rather_than_fatal() {
        let map: Keymap = serde_json::from_str(r#"{"theme.toggle":"ctrl+nonsense"}"#).unwrap();
        assert_eq!(map.chord(Action::ToggleTheme), None);
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('t')), Screen::Library),
            None,
            "and it does not fall back to the default it was written to replace"
        );
        // The rest of the map is untouched.
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('k')), Screen::Library),
            Some(Action::OpenPalette)
        );
    }

    #[test]
    fn two_actions_on_one_chord_resolve_the_same_way_every_time() {
        let mut map = Keymap::default();
        // Ctrl+T already toggles the theme; point the palette at it too.
        map.bind(Action::OpenPalette, Some(Chord::ctrl(Key::Char('t'))));

        let conflicts = map.conflicts();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0], (Action::OpenPalette, Action::ToggleTheme));
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('t')), Screen::Library),
            Some(Action::OpenPalette),
            "the earlier action in the catalogue wins, always"
        );
    }

    /// A screen's own key beats a global one on that screen, which is a real
    /// loss of the global action and is reported as one.
    #[test]
    fn a_screen_key_over_a_global_one_is_a_conflict_on_that_screen() {
        let mut map = Keymap::default();
        map.bind(Action::PipelineRun, Some(Chord::ctrl(Key::Char('t'))));

        assert_eq!(
            map.conflicts(),
            vec![(Action::PipelineRun, Action::ToggleTheme)],
        );
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('t')), Screen::Pipeline),
            Some(Action::PipelineRun),
        );
        assert_eq!(
            map.action_for(Chord::ctrl(Key::Char('t')), Screen::Library),
            Some(Action::ToggleTheme),
            "and everywhere else the global key is untouched"
        );
    }

    /// The same key on two different screens is the mechanism, not a
    /// collision: it is how `Space` plays on both.
    #[test]
    fn two_screens_sharing_a_key_is_not_a_conflict() {
        let map = Keymap::default();
        assert_eq!(
            map.chord(Action::ScopeToggle),
            map.chord(Action::ResultsToggle)
        );
        assert!(map.conflicts().is_empty(), "{:?}", map.conflicts());
    }

    #[test]
    fn reset_puts_every_key_back() {
        let mut map = Keymap::default();
        map.bind(Action::ScopeToggle, Some(Chord::plain(Key::Char('p'))));
        map.bind(Action::ToggleTheme, None);
        map.reset();
        assert_eq!(map, Keymap::default());
        assert_eq!(
            map.chord(Action::ToggleTheme),
            Some(Chord::ctrl(Key::Char('t')))
        );
    }
}
