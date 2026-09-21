//! The command palette (`docs/DESIGN.md` §12.5, §15.11).
//!
//! `Ctrl`+`K` over **every action, signal and stage**: the actions come from
//! [`crate::actions::Action::ALL`], the signals from the list the Scope screen
//! has already loaded, and the stages from the pipeline's registry — so the
//! palette invents no third copy of any of the three, and a library or a
//! vendor DLL that appears in the application appears here.
//!
//! The palette is a *finder*, not a screen: it has no state worth keeping,
//! nothing in it is destructive by itself, and closing it leaves the
//! application exactly where it was. Running an entry navigates to where the
//! entry lives and then does the thing — which is the only reading of "run
//! *Play or pause* from the Library screen" that makes sense.

use iced::widget::{
    column, container, mouse_area, opaque, row, scrollable, text, text_input, Space,
};
use iced::{Alignment, Element, Length, Task, Theme};
use sp_core::SignalId;
use sp_proc::StageDescriptor;

use crate::actions::Action;
use crate::screens::{inspector::Target, library, pipeline, Screen};
use crate::state::Message as Root;
use crate::typography;
use crate::ui;

/// Rows drawn at once. A query narrows faster than a user scrolls, and a
/// palette that lays out ten thousand signals to show eight of them is a
/// dropped frame for nothing.
const VISIBLE: usize = 40;

/// One thing the palette can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A command from the catalogue.
    Command(Action),
    /// A stored signal, which the palette opens in the Inspector.
    Signal {
        id: SignalId,
        name: String,
        group: String,
    },
    /// A registered stage, which the palette appends to the pipeline.
    Stage {
        kind: &'static str,
        label: &'static str,
        summary: &'static str,
    },
}

impl Entry {
    /// What the row reads.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Command(action) => action.label(),
            Self::Signal { name, .. } => name.clone(),
            Self::Stage { label, .. } => (*label).to_owned(),
        }
    }

    /// The quiet right-hand column: where the entry lives, or what it is.
    #[must_use]
    pub fn context(&self) -> String {
        match self {
            Self::Command(action) => action.group().to_owned(),
            Self::Signal { group, .. } => group.clone(),
            Self::Stage { .. } => "Stage".to_owned(),
        }
    }

    /// The second line, when the entry has one worth a line.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Stage { summary, .. } => Some(summary),
            _ => None,
        }
    }

    /// The text the query is scored against.
    ///
    /// The label carries the weight; the identifier is matched as a fallback
    /// so a user who thinks in kinds (`dsp.fft`, `nav.runs`) finds the thing
    /// they are thinking of, just a little lower than someone who typed its
    /// name.
    fn keys(&self) -> (String, Option<String>) {
        match self {
            Self::Command(action) => (action.label(), Some(action.id().to_owned())),
            Self::Signal { name, group, .. } => (name.clone(), Some(group.clone())),
            Self::Stage { label, kind, .. } => ((*label).to_owned(), Some((*kind).to_owned())),
        }
    }

    /// What running this entry sends to the root.
    #[must_use]
    pub fn message(&self) -> Root {
        match self {
            Self::Command(action) => Root::Run(*action),
            // The Library screen's own handover to the Inspector, reused
            // rather than reimplemented: one path from "show me this signal"
            // to a loaded Inspector (§12.2).
            Self::Signal { id, .. } => {
                Root::Library(library::Message::Inspect(Target::Signal(*id)))
            }
            Self::Stage { kind, .. } => Root::Pipeline(pipeline::Message::AddStage(kind)),
        }
    }

    /// Where running this entry takes the user, if anywhere.
    #[must_use]
    pub fn screen(&self) -> Option<Screen> {
        match self {
            Self::Command(action) => action.screen(),
            // The Inspector is reached through the Library message above,
            // which navigates on its own.
            Self::Signal { .. } => None,
            Self::Stage { .. } => Some(Screen::Pipeline),
        }
    }
}

/// How well `query` matches `text`, or `None` when it does not match at all.
///
/// A subsequence match — every character of the query appears in order, not
/// necessarily adjacently — scored so that the closer a match is to how the
/// thing is actually spelled, the higher it ranks: the start of the text and
/// the start of a word are worth most, a run of adjacent characters next, and
/// every character skipped along the way costs one.
///
/// The scan is greedy rather than optimal. Optimal alignment is a dynamic
/// program and this runs over every entry on every keystroke; greedy is
/// deterministic, which is what a *testable* ranking needs, and on names of
/// this length the two agree.
#[must_use]
pub fn score(query: &str, text: &str) -> Option<i32> {
    const AT_START: i32 = 18;
    const AT_WORD: i32 = 12;
    const ADJACENT: i32 = 8;

    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if query.is_empty() {
        return Some(0);
    }

    let text: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let is_boundary = |index: usize| match index {
        0 => true,
        index => matches!(text[index - 1], ' ' | '.' | '_' | '-' | '/' | '(' | '\''),
    };

    let mut total = 0;
    let mut cursor = 0;
    let mut previous: Option<usize> = None;
    for wanted in query {
        let found = (cursor..text.len()).find(|index| text[*index] == wanted)?;
        total += if found == 0 {
            AT_START
        } else if is_boundary(found) {
            AT_WORD
        } else if previous == Some(found - 1) {
            ADJACENT
        } else {
            0
        };
        total -= i32::try_from(found - cursor).unwrap_or(i32::MAX);
        previous = Some(found);
        cursor = found + 1;
    }
    Some(total)
}

#[derive(Debug, Clone)]
pub enum Message {
    QueryChanged(String),
    /// Up or down the list, clamped at both ends.
    Move(i32),
    /// Run what is selected.
    Activate,
    /// Run row `n` — a click.
    Pick(usize),
    Close,
}

#[derive(Debug, Default)]
pub struct State {
    open: bool,
    query: String,
    selected: usize,
    /// Everything the palette can run, in catalogue order: commands, then
    /// signals, then stages.
    entries: Vec<Entry>,
    /// Set when an entry is run, for the root to dispatch and close on.
    chosen: Option<Entry>,
}

impl State {
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the palette over a freshly taken snapshot of what there is to
    /// run.
    ///
    /// A snapshot rather than a live borrow: the signal list is the Scope
    /// screen's and the stage list is the Pipeline screen's, and neither
    /// changes while a palette is open — a query that re-read them on every
    /// keystroke would buy nothing and hold two screens borrowed.
    pub fn open<'a>(
        &mut self,
        signals: impl Iterator<Item = (SignalId, &'a str, &'a str)>,
        stages: impl Iterator<Item = &'static StageDescriptor>,
    ) -> Task<Message> {
        self.entries = Action::all().map(Entry::Command).collect();
        self.entries
            .extend(signals.map(|(id, name, group)| Entry::Signal {
                id,
                name: name.to_owned(),
                group: group.to_owned(),
            }));
        self.entries.extend(stages.map(|descriptor| Entry::Stage {
            kind: descriptor.kind,
            label: descriptor.label,
            summary: descriptor.summary,
        }));
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.chosen = None;
        text_input::focus(query_id())
    }

    pub fn close(&mut self) {
        self.open = false;
        // The entries are a snapshot of a library that may be a hundred
        // thousand signals; holding it while the palette is shut would be a
        // copy of the library kept for nothing.
        self.entries = Vec::new();
        self.query.clear();
    }

    /// The entry the user ran, once.
    pub fn take_chosen(&mut self) -> Option<Entry> {
        self.chosen.take()
    }

    /// Everything that matches the current query, best first.
    ///
    /// Ties break on the shorter label and then on catalogue order, so the
    /// same query always produces the same list — which is what makes the
    /// ranking testable and what stops a row moving under the user's finger.
    #[must_use]
    pub fn matches(&self) -> Vec<&Entry> {
        let mut scored: Vec<(i32, usize, usize, &Entry)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ordinal, entry)| {
                let (label, alternate) = entry.keys();
                let by_label = score(&self.query, &label);
                // An identifier match is a match, one step behind a name.
                let by_alternate = alternate
                    .as_deref()
                    .and_then(|text| score(&self.query, text))
                    .map(|found| found - 6);
                let best = match (by_label, by_alternate) {
                    (Some(a), Some(b)) => a.max(b),
                    (Some(a), None) => a,
                    (None, Some(b)) => b,
                    (None, None) => return None,
                };
                Some((best, label.chars().count(), ordinal, entry))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        scored.into_iter().map(|(.., entry)| entry).collect()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::QueryChanged(query) => {
                self.query = query;
                self.selected = 0;
                Task::none()
            }
            Message::Move(delta) => {
                let last = self.matches().len().saturating_sub(1);
                self.selected = self
                    .selected
                    .saturating_add_signed(delta as isize)
                    .min(last);
                Task::none()
            }
            Message::Activate => {
                self.chosen = self
                    .matches()
                    .get(self.selected)
                    .map(|entry| (*entry).clone());
                Task::none()
            }
            Message::Pick(index) => {
                self.selected = index;
                self.chosen = self.matches().get(index).map(|entry| (*entry).clone());
                Task::none()
            }
            Message::Close => {
                self.close();
                Task::none()
            }
        }
    }

    // -----------------------------------------------------------------------
    // View
    // -----------------------------------------------------------------------

    /// The palette, to be stacked over the application, or nothing when it is
    /// shut.
    #[must_use]
    pub fn view(&self) -> Option<Element<'_, Message>> {
        if !self.open {
            return None;
        }
        let matches = self.matches();

        let field = text_input("Run a command, open a signal, add a stage…", &self.query)
            .id(query_id())
            .on_input(Message::QueryChanged)
            .on_submit(Message::Activate)
            .size(typography::BODY_SIZE)
            .padding([8, 10]);

        let mut list = column![].width(Length::Fill);
        for (index, entry) in matches.iter().take(VISIBLE).enumerate() {
            list = list.push(self.row(index, entry));
        }

        let footing: Element<'_, Message> = if matches.is_empty() {
            container(ui::empty(
                format!("Nothing matches '{}'", self.query),
                "Clear the query to see every command, signal and stage.",
            ))
            .padding([10, 12])
            .into()
        } else {
            container(
                row![
                    ui::caption(if matches.len() > VISIBLE {
                        format!("first {VISIBLE} of {} matches", matches.len())
                    } else {
                        format!("{} of {}", matches.len(), self.entries.len())
                    }),
                    Space::with_width(Length::Fill),
                    // The keys are named rather than drawn: the embedded
                    // faces carry no arrow or return glyph, and a hollow box
                    // is worse than the word (`src/typography.rs`).
                    ui::caption("up/down move · enter runs · esc closes"),
                ]
                .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .into()
        };

        let panel = container(
            column![
                container(field).padding([8, 8]),
                ui::rule(),
                scrollable(list).height(Length::Shrink),
                ui::rule(),
                footing,
            ]
            .width(Length::Fill),
        )
        .width(Length::Fixed(720.0))
        .max_height(560.0)
        .style(ui::panel);

        // The scrim is a click target as much as a shade: a palette with no
        // way out but the keyboard is a modal dialog, which §12.3 does not
        // want. `opaque` stops a click that lands on the palette itself from
        // falling through to it.
        let scrim = mouse_area(
            container(Space::with_width(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .style(scrim),
        )
        .on_press(Message::Close);

        // `opaque` wraps the *panel* and not the layer holding it: a click
        // that lands on the palette must not fall through to the scrim, and a
        // click anywhere else must.
        Some(
            iced::widget::stack![
                scrim,
                container(opaque(panel))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Alignment::Center)
                    .padding([80, 20]),
            ]
            .into(),
        )
    }

    fn row<'a>(&'a self, index: usize, entry: &'a Entry) -> Element<'a, Message> {
        let active = index == self.selected;

        // The key that runs it, where there is one, set as a key and pushed to
        // the far edge — the same treatment the navigation rail gives a
        // shortcut, for the same reason (`state.rs`).
        let mut line = row![
            text(entry.label())
                .size(typography::BODY_SIZE)
                .font(if active {
                    typography::BODY_STRONG
                } else {
                    typography::BODY
                }),
            Space::with_width(Length::Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(detail) = entry.detail() {
            line = line.push(
                text(detail.to_owned())
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
            );
        }
        line = line.push(
            text(entry.context())
                .size(typography::LABEL_SIZE)
                .font(typography::LABEL)
                .style(ui::dim),
        );

        iced::widget::button(line)
            .width(Length::Fill)
            .padding([5.0, 12.0])
            .style(ui::selectable(active))
            .on_press(Message::Pick(index))
            .into()
    }
}

/// The query field's identity, so opening the palette can focus it.
fn query_id() -> text_input::Id {
    text_input::Id::new("palette-query")
}

/// A wash over the application, heavy enough to say *this is in front* and
/// light enough that the trace behind it is still legible.
///
/// It has to *recede* in both themes, and the window background only does that
/// in the dark one — a white wash over a light theme washes out rather than
/// stepping back. So the light theme takes a step of the neutral ladder
/// instead, which is a grey over grey.
fn scrim(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    let wash = if palette.is_dark {
        palette.background.base.color
    } else {
        palette.background.strong.color
    };
    container::Style {
        background: Some(wash.scale_alpha(0.55).into()),
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> State {
        let mut state = State::default();
        let _ = state.open(std::iter::empty(), std::iter::empty());
        state
    }

    fn labels(state: &State) -> Vec<String> {
        state.matches().iter().map(|entry| entry.label()).collect()
    }

    #[test]
    fn a_query_matches_a_subsequence_and_scores_where_it_landed() {
        assert!(score("run", "Run the pipeline").is_some());
        assert!(score("rnpp", "Run the pipeline").is_some(), "not adjacent");
        assert_eq!(score("zz", "Run the pipeline"), None);
        // The start of the text beats the start of a word beats the middle.
        let at_start = score("r", "Run the pipeline").unwrap();
        let at_word = score("p", "Run the pipeline").unwrap();
        let inside = score("u", "Run the pipeline").unwrap();
        assert!(at_start > at_word, "{at_start} > {at_word}");
        assert!(at_word > inside, "{at_word} > {inside}");
        // An empty query matches everything equally.
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn matching_ignores_case_and_the_spaces_in_the_query() {
        assert_eq!(
            score("RUN", "run the pipeline"),
            score("run", "Run the pipeline")
        );
        assert_eq!(
            score("run t", "Run the pipeline"),
            score("runt", "Run the pipeline")
        );
    }

    /// The exit criterion, at the level this module can hold it: with nothing
    /// typed the palette offers every action in the catalogue, and each one is
    /// the first hit for its own name.
    #[test]
    fn every_action_is_reachable_from_the_palette() {
        let mut state = palette();
        let offered = labels(&state);
        for action in Action::all() {
            assert!(
                offered.contains(&action.label()),
                "{} is not offered",
                action.id()
            );
        }

        for action in Action::all() {
            let _ = state.update(Message::QueryChanged(action.label()));
            let found = state.matches();
            assert_eq!(
                found.first().map(|entry| (*entry).clone()),
                Some(Entry::Command(action)),
                "'{}' should be the first hit for its own name, got {:?}",
                action.label(),
                found.first().map(|entry| entry.label()),
            );
        }
    }

    /// And by its identifier, which is what a user who thinks in kinds types.
    #[test]
    fn an_action_is_also_reachable_by_its_identifier() {
        let mut state = palette();
        for action in Action::all() {
            let _ = state.update(Message::QueryChanged(action.id().to_owned()));
            assert!(
                state
                    .matches()
                    .iter()
                    .any(|entry| **entry == Entry::Command(action)),
                "{} is not found by its own id",
                action.id()
            );
        }
    }

    #[test]
    fn signals_and_stages_are_in_the_list_beside_the_commands() {
        let mut state = State::default();
        let (registry, _) = crate::stages::registry(&[]);
        let _ = state.open(
            [
                (SignalId::new(1), "envelope", "ladder 12 dB"),
                (SignalId::new(2), "carrier", "ladder 12 dB"),
            ]
            .into_iter(),
            registry.descriptors(),
        );

        let _ = state.update(Message::QueryChanged("envelope".to_owned()));
        assert_eq!(
            state.matches().first().map(|entry| (*entry).clone()),
            Some(Entry::Signal {
                id: SignalId::new(1),
                name: "envelope".to_owned(),
                group: "ladder 12 dB".to_owned(),
            })
        );

        let _ = state.update(Message::QueryChanged("biquad".to_owned()));
        let hit = state.matches().first().map(|entry| (*entry).clone());
        assert!(
            matches!(hit, Some(Entry::Stage { kind, .. }) if kind == "dsp.filter.biquad"),
            "{hit:?}"
        );
        assert_eq!(hit.as_ref().unwrap().screen(), Some(Screen::Pipeline));
    }

    #[test]
    fn a_signal_opens_in_the_inspector_and_a_stage_lands_on_the_pipeline() {
        let signal = Entry::Signal {
            id: SignalId::new(7),
            name: "envelope".to_owned(),
            group: "g".to_owned(),
        };
        assert!(matches!(
            signal.message(),
            Root::Library(library::Message::Inspect(Target::Signal(id))) if id == SignalId::new(7)
        ));
        let stage = Entry::Stage {
            kind: "dsp.condition.gain",
            label: "Gain",
            summary: "scales",
        };
        assert!(matches!(
            stage.message(),
            Root::Pipeline(pipeline::Message::AddStage("dsp.condition.gain"))
        ));
    }

    #[test]
    fn the_keyboard_walks_the_list_and_stops_at_both_ends() {
        let mut state = palette();
        assert_eq!(state.selected, 0);
        let _ = state.update(Message::Move(-1));
        assert_eq!(state.selected, 0, "up from the top stays at the top");
        let _ = state.update(Message::Move(1));
        assert_eq!(state.selected, 1);
        for _ in 0..1_000 {
            let _ = state.update(Message::Move(1));
        }
        assert_eq!(
            state.selected,
            state.matches().len() - 1,
            "down from the bottom stays at the bottom"
        );

        // Typing puts the selection back on the best match.
        let _ = state.update(Message::QueryChanged("runt".to_owned()));
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn activating_hands_the_entry_over_exactly_once() {
        let mut state = palette();
        let _ = state.update(Message::QueryChanged("Run the pipeline".to_owned()));
        let _ = state.update(Message::Activate);
        assert_eq!(
            state.take_chosen(),
            Some(Entry::Command(Action::PipelineRun))
        );
        assert_eq!(state.take_chosen(), None, "and not a second time");
    }

    #[test]
    fn a_query_that_matches_nothing_still_draws() {
        let mut state = palette();
        let _ = state.update(Message::QueryChanged("qqqqzzzz".to_owned()));
        assert!(state.matches().is_empty());
        assert!(state.view().is_some());
        let _ = state.update(Message::Activate);
        assert_eq!(state.take_chosen(), None, "there is nothing to run");
    }

    #[test]
    fn closing_drops_the_snapshot_it_was_holding() {
        let mut state = State::default();
        let _ = state.open(
            (0..64).map(|_| (SignalId::new(1), "s", "g")),
            std::iter::empty(),
        );
        assert!(state.is_open());
        assert!(state.view().is_some());
        let _ = state.update(Message::Close);
        assert!(!state.is_open());
        assert!(state.entries.is_empty());
        assert!(state.view().is_none());
    }
}
