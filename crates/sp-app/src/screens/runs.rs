//! The Runs screen (`docs/DESIGN.md` §12.1): the history of what has been run.
//!
//! Every run the library has recorded, newest first, with the pipeline it ran,
//! the hash that says *which* algorithm that was (§9.2), how it ended, how
//! long it took, how its assertions came out and which baselines name it. A
//! run is the record of an experiment, so this screen is where one is found
//! again — and where it is promoted, opened, diffed or thrown away.
//!
//! Neither the diff nor the stage-by-stage view is drawn here: `Open` and
//! `Diff` hand the run to the Results screen, which already draws both
//! (§10.3, §10.4). One renderer, one set of conventions.

use std::collections::BTreeMap;

use iced::widget::{button, column, container, row, scrollable, text, text_input, Space};
use iced::{Alignment, Element, Length, Task};
use sp_core::run::{AssertStatus, RunStatus};
use sp_core::{RunId, Tolerances};
use sp_store::{regress, runs, AssertionResultRow, BaselineRow, RunRow, Store};

use crate::jobs;
use crate::typography;
use crate::ui;

/// One row of the history, with everything the table shows.
#[derive(Debug, Clone)]
pub struct Entry {
    pub run: RunRow,
    pub pipeline: String,
    /// Groups recorded, and how many of them failed.
    pub groups: usize,
    pub failed_groups: usize,
    /// Assertions evaluated, and how many failed (§9.7).
    pub assertions: usize,
    pub failed_assertions: usize,
    /// Baseline names pointing at this run.
    pub baselines: Vec<String>,
    /// Wall time from start to finish, in seconds.
    pub duration_s: Option<f64>,
}

impl Entry {
    /// Whether anything about this run went wrong — a failed group or a failed
    /// assertion. A run whose stages all ran but whose assertions failed is a
    /// failing run (§9.7).
    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.run.status == RunStatus::Failed || self.failed_groups > 0 || self.failed_assertions > 0
    }
}

/// What the detail pane shows about the selected run.
#[derive(Debug, Clone, Default)]
pub struct Detail {
    pub assertions: Vec<AssertionResultRow>,
    pub group_names: BTreeMap<i64, String>,
}

#[derive(Debug, Default)]
pub struct State {
    entries: Vec<Entry>,
    baselines: Vec<BaselineRow>,
    selected: Option<RunId>,
    detail: Option<(RunId, Detail)>,
    /// The run a diff would be against, chosen with `Diff against`.
    against: Option<RunId>,
    promote_as: String,
    filter_failures: bool,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    /// A run to open on the Results screen, and what to diff it against,
    /// waiting for the root to navigate.
    open_request: Option<(RunId, Option<RunId>)>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Loaded(Result<(Vec<Entry>, Vec<BaselineRow>), String>),
    Select(RunId),
    DetailLoaded(RunId, Result<Detail, String>),
    ToggleFailuresOnly(bool),
    MarkAgainst(RunId),
    ClearAgainst,
    Open(RunId),
    Diff(RunId),
    PromoteAs(String),
    Promote(RunId),
    Delete(RunId),
    Done(Result<String, String>),
}

impl State {
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        self.loading = true;
        Task::perform(jobs::read(store.clone(), load_history), Message::Loaded)
    }

    /// The run the user asked to see on the Results screen, taken once.
    pub fn take_open_request(&mut self) -> Option<(RunId, Option<RunId>)> {
        self.open_request.take()
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Refresh => store.map_or_else(Task::none, |store| self.load(store)),
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok((entries, baselines)) => {
                        self.error = None;
                        // A selection whose run has been deleted goes with it.
                        if let Some(selected) = self.selected {
                            if !entries.iter().any(|entry| entry.run.id == selected) {
                                self.selected = None;
                                self.detail = None;
                            }
                        }
                        if self.against.is_some_and(|against| {
                            !entries.iter().any(|entry| entry.run.id == against)
                        }) {
                            self.against = None;
                        }
                        self.entries = entries;
                        self.baselines = baselines;
                    }
                    Err(error) => {
                        tracing::error!(%error, "runs could not be listed");
                        self.error = Some(error);
                    }
                }
                Task::none()
            }
            Message::Select(run) => {
                self.selected = Some(run);
                self.promote_as = self
                    .entries
                    .iter()
                    .find(|entry| entry.run.id == run)
                    .and_then(|entry| entry.baselines.first().cloned())
                    .unwrap_or_default();
                let Some(store) = store else {
                    return Task::none();
                };
                Task::perform(
                    jobs::read(store.clone(), move |conn| load_detail(conn, run)),
                    move |result| Message::DetailLoaded(run, result),
                )
            }
            Message::DetailLoaded(run, result) => {
                if self.selected == Some(run) {
                    match result {
                        Ok(detail) => self.detail = Some((run, detail)),
                        Err(error) => self.error = Some(error),
                    }
                }
                Task::none()
            }
            Message::ToggleFailuresOnly(on) => {
                self.filter_failures = on;
                Task::none()
            }
            Message::MarkAgainst(run) => {
                self.against = Some(run);
                Task::none()
            }
            Message::ClearAgainst => {
                self.against = None;
                Task::none()
            }
            Message::Open(run) => {
                self.open_request = Some((run, None));
                Task::none()
            }
            Message::Diff(run) => {
                match self.against {
                    // Diffing a run against itself would show nothing and say
                    // nothing, so it is refused where it is asked for.
                    Some(other) if other == run => {
                        self.error = Some("A run does not differ from itself.".to_owned());
                    }
                    Some(other) => self.open_request = Some((run, Some(other))),
                    None => {
                        self.error = Some("Mark a run to diff against first.".to_owned());
                    }
                }
                Task::none()
            }
            Message::PromoteAs(name) => {
                self.promote_as = name;
                Task::none()
            }
            Message::Promote(run) => {
                let Some(store) = store else {
                    self.error = Some("No library is open.".to_owned());
                    return Task::none();
                };
                let name = self.promote_as.trim().to_owned();
                if name.is_empty() {
                    self.error = Some("A baseline needs a name.".to_owned());
                    return Task::none();
                }
                self.error = None;
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        // Promoting is exact unless the baseline already
                        // carries tolerances of its own, which re-promoting
                        // under the same name keeps (§10.4).
                        let tolerances = regress::get_baseline_by_name(conn, &name)
                            .map_or(Tolerances::EXACT, |row| row.tolerances);
                        regress::promote(conn, &name, run, &tolerances)?;
                        Ok(format!("Run {} is now '{name}'.", run.get()))
                    }),
                    Message::Done,
                )
            }
            Message::Delete(run) => {
                let Some(store) = store else {
                    return Task::none();
                };
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        runs::delete_run(conn, run)?;
                        Ok(format!("Deleted run {}.", run.get()))
                    }),
                    Message::Done,
                )
            }
            Message::Done(result) => {
                match result {
                    Ok(notice) => {
                        self.notice = Some(notice);
                        self.error = None;
                    }
                    Err(error) => {
                        self.notice = None;
                        self.error = Some(error);
                    }
                }
                store.map_or_else(Task::none, |store| self.load(store))
            }
        }
    }

    /// The rows the table shows under the current filter.
    fn visible(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(move |entry| !self.filter_failures || entry.is_failure())
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let table = container(scrollable(self.table()))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([12, 16]);
        // The detail sits on its own surface with a hairline between it and
        // the history, rather than in a bordered box floating over it.
        let panel = container(scrollable(self.panel()))
            .width(Length::Fixed(340.0))
            .height(Length::Fill)
            .padding([16, 16])
            .style(ui::panel);
        row![table, panel].height(Length::Fill).into()
    }

    fn table(&self) -> Element<'_, Message> {
        let failures = self
            .entries
            .iter()
            .filter(|entry| entry.is_failure())
            .count();
        let mut list = column![row![
            text("Runs")
                .size(typography::TITLE_SIZE)
                .font(typography::TITLE),
            Space::with_width(Length::Fixed(12.0)),
            // How many ran and how many of those failed are two different
            // readings, and the failing one is the reason anyone opens this
            // screen, so it takes the warning colour when it is not zero.
            text(format!(
                "{} run{}",
                self.entries.len(),
                if self.entries.len() == 1 { "" } else { "s" },
            ))
            .size(typography::BODY_SIZE)
            .font(typography::READOUT)
            .style(ui::dim),
            text(format!("{failures} failing"))
                .size(typography::BODY_SIZE)
                .font(typography::READOUT)
                .style(if failures == 0 { ui::dim } else { ui::warned }),
            Space::with_width(Length::Fill),
            iced::widget::checkbox("Failures only", self.filter_failures)
                .text_size(typography::BODY_SIZE)
                .on_toggle(Message::ToggleFailuresOnly),
            button(
                text("Refresh")
                    .size(typography::LABEL_SIZE)
                    .font(typography::LABEL)
                    .style(ui::dim),
            )
            .padding([3, 8])
            .style(button::text)
            .on_press(Message::Refresh),
        ]
        .spacing(10)
        .align_y(Alignment::Center)]
        .spacing(8);

        if let Some(error) = &self.error {
            list = list.push(text(error).size(typography::BODY_SIZE).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            list = list.push(
                text(notice)
                    .size(typography::BODY_SIZE)
                    .style(text::success),
            );
        }
        if self.entries.is_empty() {
            let state = if self.loading {
                ui::empty(
                    "Reading the history…",
                    "Every run this application has made is kept, whatever it returned.",
                )
            } else {
                ui::empty(
                    "Nothing has been run yet.",
                    "Build a pipeline, run it over a dataset, and it appears here with                      the algorithm hash that produced it.",
                )
            };
            return list.push(state).into();
        }

        let widths = [60.0, 150.0, 110.0, 165.0, 95.0, 80.0, 110.0, 120.0];
        // A pipeline and a baseline have names, which are read; everything
        // else in this table is a quantity or an identifier, which is scanned.
        // The two want opposite alignments, and the heading goes wherever its
        // column went.
        let columns: [(&str, Alignment); 8] = [
            ("Run", Alignment::End),
            ("Pipeline", Alignment::Start),
            ("Algorithm", Alignment::Start),
            ("Started", Alignment::Start),
            ("Duration", Alignment::End),
            ("Groups", Alignment::End),
            ("Assertions", Alignment::End),
            ("Baseline", Alignment::Start),
        ];
        let mut head = row![];
        for ((label, align), width) in columns.into_iter().zip(widths) {
            head = head.push(ui::heading_aligned(label, width, align));
        }
        list = list
            .push(Space::with_height(Length::Fixed(4.0)))
            .push(head)
            .push(ui::rule());

        for entry in self.visible() {
            let run = &entry.run;
            let selected = self.selected == Some(run.id);
            let marked = self.against == Some(run.id);
            let cells = [
                format!("#{}", run.id.get()),
                entry.pipeline.clone(),
                // The hash is what says two runs ran the same algorithm, so a
                // recognisable prefix of it is a column of its own (§9.2).
                run.pipeline_hash.chars().take(10).collect::<String>(),
                format_timestamp(run.started_utc),
                entry
                    .duration_s
                    .map_or_else(|| "—".to_owned(), |s| format!("{s:.2} s")),
                format!("{}/{}", entry.groups - entry.failed_groups, entry.groups),
                if entry.assertions == 0 {
                    "—".to_owned()
                } else {
                    format!(
                        "{}/{}",
                        entry.assertions - entry.failed_assertions,
                        entry.assertions
                    )
                },
                entry.baselines.join(", "),
            ];

            let mut line = row![].align_y(Alignment::Center);
            for (index, ((value, (_, align)), width)) in
                cells.into_iter().zip(columns).zip(widths).enumerate()
            {
                let cell = text(value).size(typography::BODY_SIZE);
                // A name is set in the prose face; the id, the hash, the clock
                // time and the counts are all machine output and are set
                // monospaced, so the column reads as a column.
                let cell = if matches!(index, 1 | 7) {
                    cell
                } else {
                    cell.font(typography::READOUT)
                };
                // The status colours the run's own number and its counts.
                let cell = if entry.is_failure() && matches!(index, 0 | 5 | 6) {
                    cell.style(text::danger)
                } else {
                    cell
                };
                line = line.push(
                    container(cell)
                        .width(Length::Fixed(width))
                        .align_x(align)
                        .padding([3, 6]),
                );
            }
            // Marking is a selection, not an action, so it takes the same band
            // the selected row takes rather than a fill of the accent.
            line = line.push(
                button(
                    text(if marked { "Marked" } else { "Mark" })
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL),
                )
                .padding([2, 8])
                .style(ui::selectable(marked))
                .on_press(if marked {
                    Message::ClearAgainst
                } else {
                    Message::MarkAgainst(run.id)
                }),
            );

            list = list.push(
                button(line)
                    .width(Length::Fill)
                    .padding(0)
                    .style(ui::selectable(selected))
                    .on_press(Message::Select(run.id)),
            );
        }
        list.into()
    }

    fn panel(&self) -> Element<'_, Message> {
        let Some(entry) = self
            .selected
            .and_then(|id| self.entries.iter().find(|entry| entry.run.id == id))
        else {
            return ui::empty(
                "Pick a run on the left.",
                "A run records the pipeline, the algorithm hash it resolved to, and                  what every assertion returned.",
            );
        };
        let run = &entry.run;

        // The four facts under the heading were four sentences, three of
        // which began with a word the reader had to skip to reach the value.
        // As a strip, the caption is the word and the value is what is left.
        let mut panel = column![
            text(format!("Run {}", run.id.get()))
                .size(typography::HEADING_SIZE)
                .font(typography::HEADING),
            row![
                ui::spec(
                    "Status",
                    run.status.label(),
                    if entry.is_failure() {
                        ui::warned
                    } else {
                        text::base
                    },
                ),
                ui::fact("Pipeline", &entry.pipeline),
                ui::fact("Started", format_timestamp(run.started_utc)),
            ]
            .spacing(20)
            .wrap(),
            row![
                ui::fact("Algorithm", &run.pipeline_hash),
                ui::fact("Built by", &run.app_version),
            ]
            .spacing(20)
            .wrap(),
        ]
        .spacing(10);

        if let Some(notes) = &run.notes {
            panel = panel.push(text(notes).size(typography::BODY_SIZE));
        }

        let against = self.against.filter(|id| *id != run.id);
        panel = panel.push(Space::with_height(Length::Fixed(6.0))).push(
            row![
                button(text("Open in Results").size(typography::BODY_SIZE))
                    .padding([5, 10])
                    .style(button::primary)
                    .on_press(Message::Open(run.id)),
                button(text("Diff").size(typography::BODY_SIZE))
                    .padding([5, 10])
                    .style(button::secondary)
                    .on_press_maybe(against.map(|_| Message::Diff(run.id))),
            ]
            .spacing(6),
        );
        panel = panel.push(
            text(match against {
                Some(other) => format!("Diffs against run {}.", other.get()),
                None => "Mark another run to diff against it.".to_owned(),
            })
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
        );

        panel = panel
            .push(Space::with_height(Length::Fixed(10.0)))
            .push(ui::caption("Baseline"))
            .push(
                row![
                    text_input("name", &self.promote_as)
                        .on_input(Message::PromoteAs)
                        .on_submit(Message::Promote(run.id))
                        .size(typography::BODY_SIZE),
                    button(text("Promote").size(typography::BODY_SIZE))
                        .padding([4, 10])
                        .style(button::secondary)
                        .on_press(Message::Promote(run.id)),
                ]
                .spacing(6),
            );
        if !entry.baselines.is_empty() {
            panel = panel.push(
                text(format!(
                    "Named by {}. Deleting it is refused while a baseline points at it.",
                    entry.baselines.join(", ")
                ))
                .size(typography::LABEL_SIZE)
                .style(ui::dim),
            );
        }

        panel = panel.push(Space::with_height(Length::Fixed(10.0))).push(
            button(text("Delete this run").size(typography::BODY_SIZE))
                .padding([4, 10])
                .style(button::danger)
                .on_press(Message::Delete(run.id)),
        );

        // Assertion outcomes, failures first: that is what a reader of the
        // history is looking for (§9.7).
        if let Some((id, detail)) = &self.detail {
            if *id == run.id && !detail.assertions.is_empty() {
                panel = panel
                    .push(Space::with_height(Length::Fixed(12.0)))
                    .push(ui::caption("Assertions"));
                let mut rows: Vec<&AssertionResultRow> = detail.assertions.iter().collect();
                rows.sort_by_key(|row| match row.status {
                    AssertStatus::Fail | AssertStatus::Error => 0,
                    AssertStatus::NotApplicable => 1,
                    AssertStatus::Pass => 2,
                });
                for outcome in rows.into_iter().take(40) {
                    let group = detail
                        .group_names
                        .get(&outcome.group_id.get())
                        .cloned()
                        .unwrap_or_else(|| format!("group {}", outcome.group_id.get()));
                    let style = match outcome.status {
                        AssertStatus::Fail | AssertStatus::Error => text::danger,
                        AssertStatus::Pass => text::success,
                        AssertStatus::NotApplicable => ui::dim,
                    };
                    panel = panel.push(
                        column![
                            text(outcome.expression.clone())
                                .size(typography::BODY_SIZE)
                                .font(typography::READOUT)
                                .style(style),
                            text(format!(
                                "{group} · {}{}",
                                outcome.status.label(),
                                outcome
                                    .message
                                    .as_ref()
                                    .map_or_else(String::new, |m| format!(" · {m}")),
                            ))
                            .size(typography::LABEL_SIZE)
                            .style(ui::dim),
                        ]
                        .spacing(1)
                        .padding([2, 0]),
                    );
                }
            }
        }

        panel.into()
    }
}

fn load_history(conn: &sp_store::Connection) -> sp_store::Result<(Vec<Entry>, Vec<BaselineRow>)> {
    let baselines = regress::list_baselines(conn)?;
    let mut names: BTreeMap<i64, String> = BTreeMap::new();
    let mut entries = Vec::new();

    for run in runs::list_runs(conn, None)? {
        let pipeline = match names.get(&run.pipeline_id.get()) {
            Some(name) => name.clone(),
            None => {
                let name = runs::get_pipeline(conn, run.pipeline_id)
                    .map(|row| row.name)
                    .unwrap_or_else(|_| format!("Pipeline {}", run.pipeline_id.get()));
                names.insert(run.pipeline_id.get(), name.clone());
                name
            }
        };
        let groups = runs::run_groups(conn, run.id)?;
        let assertions = regress::run_assertions(conn, run.id)?;
        let duration_s = run
            .finished_utc
            .map(|finished| (finished - run.started_utc).as_seconds_f64());
        entries.push(Entry {
            pipeline,
            groups: groups.len(),
            failed_groups: groups
                .iter()
                .filter(|group| group.status == RunStatus::Failed)
                .count(),
            failed_assertions: assertions
                .iter()
                .filter(|row| matches!(row.status, AssertStatus::Fail | AssertStatus::Error))
                .count(),
            assertions: assertions.len(),
            baselines: baselines
                .iter()
                .filter(|row| row.run_id == run.id)
                .map(|row| row.name.clone())
                .collect(),
            duration_s,
            run,
        });
    }
    Ok((entries, baselines))
}

fn load_detail(conn: &sp_store::Connection, run: RunId) -> sp_store::Result<Detail> {
    let assertions = regress::run_assertions(conn, run)?;
    let mut group_names = BTreeMap::new();
    for outcome in &assertions {
        if let std::collections::btree_map::Entry::Vacant(slot) =
            group_names.entry(outcome.group_id.get())
        {
            if let Ok(group) = sp_store::library::get_group(conn, outcome.group_id) {
                slot.insert(group.display_name());
            }
        }
    }
    Ok(Detail {
        assertions,
        group_names,
    })
}

fn format_timestamp(when: sp_core::time::Timestamp) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        when.year(),
        u8::from(when.month()),
        when.day(),
        when.hour(),
        when.minute(),
        when.second(),
    )
}

#[cfg(test)]
mod tests {
    use sp_core::run::RunStatus;
    use sp_core::PipelineId;

    use super::*;

    fn entry(id: i64, status: RunStatus, failed_assertions: usize) -> Entry {
        Entry {
            run: RunRow {
                id: RunId::new(id),
                pipeline_id: PipelineId::new(1),
                dataset_id: None,
                started_utc: sp_core::time::Timestamp::UNIX_EPOCH,
                finished_utc: None,
                status,
                pipeline_hash: "0123456789abcdef".to_owned(),
                app_version: "0.1.0".to_owned(),
                notes: None,
            },
            pipeline: "detector".to_owned(),
            groups: 2,
            failed_groups: 0,
            assertions: 2,
            failed_assertions,
            baselines: Vec::new(),
            duration_s: Some(1.5),
        }
    }

    fn state() -> State {
        State {
            entries: vec![
                entry(3, RunStatus::Ok, 0),
                entry(2, RunStatus::Ok, 1),
                entry(1, RunStatus::Failed, 0),
            ],
            ..State::default()
        }
    }

    #[test]
    fn a_run_whose_assertions_failed_is_a_failing_run() {
        let state = state();
        assert!(!state.entries[0].is_failure());
        // Every stage ran, and the run still failed (§9.7).
        assert!(state.entries[1].is_failure());
        assert!(state.entries[2].is_failure());
    }

    #[test]
    fn the_filter_shows_only_the_failures() {
        let mut state = state();
        assert_eq!(state.visible().count(), 3);
        let _ = state.update(None, Message::ToggleFailuresOnly(true));
        assert_eq!(state.visible().count(), 2);
    }

    #[test]
    fn a_diff_needs_two_different_runs() {
        let mut state = state();
        let _ = state.update(None, Message::Diff(RunId::new(3)));
        assert!(state.error.is_some(), "nothing marked to diff against");
        assert!(state.open_request.is_none());

        let _ = state.update(None, Message::MarkAgainst(RunId::new(3)));
        let _ = state.update(None, Message::Diff(RunId::new(3)));
        assert!(
            state.open_request.is_none(),
            "a run does not differ from itself"
        );

        let _ = state.update(None, Message::MarkAgainst(RunId::new(2)));
        let _ = state.update(None, Message::Diff(RunId::new(3)));
        assert_eq!(
            state.open_request,
            Some((RunId::new(3), Some(RunId::new(2))))
        );
    }

    #[test]
    fn opening_a_run_is_handed_to_the_root_once() {
        let mut state = state();
        let _ = state.update(None, Message::Open(RunId::new(2)));
        assert_eq!(state.take_open_request(), Some((RunId::new(2), None)));
        assert_eq!(state.take_open_request(), None);
    }

    #[test]
    fn selecting_a_run_offers_its_baseline_name_back() {
        let mut state = state();
        state.entries[0].baselines = vec!["golden".to_owned()];
        let _ = state.update(None, Message::Select(RunId::new(3)));
        // Re-promoting under the same name is how a change is accepted, so
        // the box starts on the name the run already answers to (§10.4).
        assert_eq!(state.promote_as, "golden");
    }

    #[test]
    fn a_promotion_without_a_name_is_refused() {
        let mut state = state();
        let _ = state.update(None, Message::Promote(RunId::new(3)));
        assert!(state.error.is_some());
    }

    #[test]
    fn a_selection_whose_run_was_deleted_is_dropped() {
        let mut state = state();
        let _ = state.update(None, Message::Select(RunId::new(3)));
        let _ = state.update(None, Message::MarkAgainst(RunId::new(3)));
        let _ = state.update(
            None,
            Message::Loaded(Ok((vec![entry(1, RunStatus::Failed, 0)], Vec::new()))),
        );
        assert!(state.selected.is_none());
        assert!(state.against.is_none());
    }
}
