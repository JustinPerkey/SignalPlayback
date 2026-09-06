//! The Library screen (`docs/DESIGN.md` §12.1): a tree of dataset → train →
//! group, a detail table for the selected group, and full-text search over
//! signals.
//!
//! The train level is there because groups are not independent: one capture
//! resolves to several of them (§6.6), and a tree that hung groups straight
//! off a dataset said otherwise.
//!
//! Everything shown is metadata read from the store; a pulse group's table
//! previews the first rows of its columns and nothing more is materialised.
//!
//! Search and the tag filter narrow together: typing a query and selecting two
//! tags asks for the signals that match the text *and* carry both tags. Either
//! on its own works as you would expect, and clearing both puts the tree back.
//! A row's `Inspect` button hands the Inspector its target (§12.1), and a
//! dataset exports back to the format it was imported from (§7.5, G1).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use iced::widget::{
    button, column, container, horizontal_rule, row, scrollable, text, text_input, Space,
};
use iced::{Alignment, Element, Length, Task, Theme};
use sp_core::group::DatasetId;
use sp_core::{
    Dataset, GroupId, PulseField, SampleRange, Signal, SignalGroup, SignalId, SignalTrain, TrainId,
};
use sp_store::{library, pulses, trains, LibrarySummary, Store};

use crate::jobs;
use crate::screens::inspector::Target;

/// Pulse rows shown in the detail table before the user needs the Inspector.
const PULSE_PREVIEW_ROWS: u64 = 200;

/// Cached metadata for the whole library. Samples stay on disk.
#[derive(Debug, Clone, Default)]
pub struct LibraryIndex {
    pub datasets: Vec<Dataset>,
    pub trains: HashMap<DatasetId, Vec<SignalTrain>>,
    pub groups: HashMap<TrainId, Vec<SignalGroup>>,
    pub summary: LibrarySummary,
    /// Every tag in use, with how many signals carry it.
    pub tags: Vec<(String, u64)>,
}

impl LibraryIndex {
    /// The dataset, train and group a group id resolves to.
    fn group(&self, id: GroupId) -> Option<(&Dataset, &SignalTrain, &SignalGroup)> {
        self.datasets.iter().find_map(|dataset| {
            self.trains.get(&dataset.id)?.iter().find_map(|train| {
                self.groups
                    .get(&train.id)?
                    .iter()
                    .find(|group| group.id == id)
                    .map(|group| (dataset, train, group))
            })
        })
    }

    /// Pulses and sampled signals a train holds across its groups.
    fn train_extent(&self, train: &SignalTrain) -> (usize, u64) {
        let groups = self.groups.get(&train.id).map_or(&[][..], Vec::as_slice);
        let records = groups.iter().map(|g| u64::from(g.actual_count)).sum();
        (groups.len(), records)
    }
}

/// What the detail pane shows for the selected group.
#[derive(Debug, Clone)]
pub enum GroupDetail {
    Signals(Vec<Signal>),
    Pulses {
        fields: Vec<PulseField>,
        /// TOA of the previewed rows, in seconds.
        toa_s: Vec<f64>,
        /// One row per previewed pulse, one value per field.
        rows: Vec<Vec<f64>>,
        total: u32,
    },
}

#[derive(Debug, Default)]
pub struct State {
    index: Option<LibraryIndex>,
    loading: bool,
    error: Option<String>,
    collapsed: HashSet<DatasetId>,
    collapsed_trains: HashSet<TrainId>,
    selected: Option<GroupId>,
    detail: Option<(GroupId, GroupDetail)>,
    detail_error: Option<String>,
    search: String,
    hits: Option<Vec<Signal>>,
    /// Tags the list is narrowed to; a signal must carry all of them.
    tag_filter: Vec<String>,
    /// A target the user asked the Inspector for, waiting for the root to
    /// navigate there.
    inspect_request: Option<Target>,
    notice: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Loaded(Result<LibraryIndex, String>),
    ToggleDataset(DatasetId),
    ToggleTrain(TrainId),
    SelectGroup(GroupId),
    DetailLoaded(GroupId, Result<GroupDetail, String>),
    SearchChanged(String),
    Search,
    SearchDone(Result<Vec<Signal>, String>),
    ClearSearch,
    ToggleTag(String),
    Inspect(Target),
    ExportDataset(DatasetId),
    ExportTo(DatasetId, Option<PathBuf>),
    Exported(Result<String, String>),
}

impl State {
    /// Library-wide counts, once loaded.
    #[must_use]
    pub fn summary(&self) -> Option<LibrarySummary> {
        self.index.as_ref().map(|index| index.summary)
    }

    /// Reloads the tree from the store.
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        self.loading = true;
        Task::perform(jobs::read(store.clone(), load_index), Message::Loaded)
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Refresh => match store {
                Some(store) => self.load(store),
                None => Task::none(),
            },
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(index) => {
                        self.error = None;
                        // Keep the selection if its group still exists.
                        if let Some(selected) = self.selected {
                            if index.group(selected).is_none() {
                                self.selected = None;
                                self.detail = None;
                            }
                        }
                        self.index = Some(index);
                    }
                    Err(error) => {
                        tracing::error!(%error, "library load failed");
                        self.error = Some(error);
                    }
                }
                Task::none()
            }
            Message::ToggleDataset(id) => {
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id);
                }
                Task::none()
            }
            Message::ToggleTrain(id) => {
                if !self.collapsed_trains.remove(&id) {
                    self.collapsed_trains.insert(id);
                }
                Task::none()
            }
            Message::SelectGroup(id) => {
                self.selected = Some(id);
                self.detail_error = None;
                let Some(store) = store else {
                    return Task::none();
                };
                let is_pulse = self
                    .index
                    .as_ref()
                    .and_then(|index| index.group(id))
                    .is_some_and(|(_, _, group)| group.is_pulse_group());
                Task::perform(
                    jobs::read(store.clone(), move |conn| load_detail(conn, id, is_pulse)),
                    move |result| Message::DetailLoaded(id, result),
                )
            }
            Message::DetailLoaded(id, result) => {
                if self.selected == Some(id) {
                    match result {
                        Ok(detail) => self.detail = Some((id, detail)),
                        Err(error) => self.detail_error = Some(error),
                    }
                }
                Task::none()
            }
            Message::SearchChanged(query) => {
                self.search = query;
                if self.search.trim().is_empty() && self.tag_filter.is_empty() {
                    self.hits = None;
                }
                Task::none()
            }
            Message::Search => self.query(store),
            Message::SearchDone(result) => {
                match result {
                    Ok(hits) => self.hits = Some(hits),
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::ClearSearch => {
                self.search.clear();
                self.tag_filter.clear();
                self.hits = None;
                Task::none()
            }
            Message::ToggleTag(tag) => {
                if let Some(position) = self.tag_filter.iter().position(|t| *t == tag) {
                    self.tag_filter.remove(position);
                } else {
                    self.tag_filter.push(tag);
                }
                self.query(store)
            }
            Message::Inspect(target) => {
                self.inspect_request = Some(target);
                Task::none()
            }
            Message::ExportDataset(id) => {
                let name = self
                    .index
                    .as_ref()
                    .and_then(|index| index.datasets.iter().find(|d| d.id == id))
                    .map_or_else(|| "export".to_owned(), |dataset| dataset.name.clone());
                Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title("Export the dataset as CSV")
                            .set_file_name(format!("{name}.csv"))
                            .add_filter("CSV", &["csv"])
                            .save_file()
                            .await
                            .map(|handle| handle.path().to_path_buf())
                    },
                    move |path| Message::ExportTo(id, path),
                )
            }
            Message::ExportTo(id, path) => {
                let (Some(store), Some(path)) = (store, path) else {
                    return Task::none();
                };
                self.notice = Some(format!("Exporting to {}…", path.display()));
                Task::perform(
                    jobs::read(store.clone(), move |conn| {
                        // The writer reverses the grammar the file was read
                        // with, so an untouched dataset comes back byte for
                        // byte (§7.5, G1).
                        sp_csv::export_dataset_to_file(conn, id, &path)
                            .map_err(|error| sp_store::StoreError::Invalid(error.to_string()))?;
                        Ok(path.display().to_string())
                    }),
                    Message::Exported,
                )
            }
            Message::Exported(result) => {
                match result {
                    Ok(path) => {
                        self.notice = Some(format!("Exported to {path}."));
                        self.error = None;
                    }
                    Err(error) => {
                        self.notice = None;
                        self.error = Some(error);
                    }
                }
                Task::none()
            }
        }
    }

    /// A target the user asked the Inspector for, taken once.
    pub fn take_inspect_request(&mut self) -> Option<Target> {
        self.inspect_request.take()
    }

    /// Runs the text query and the tag filter together. Both narrow: with a
    /// query and two tags selected, a signal has to match all three.
    fn query(&mut self, store: Option<&Store>) -> Task<Message> {
        let query = self.search.trim().to_owned();
        let tags = self.tag_filter.clone();
        if query.is_empty() && tags.is_empty() {
            self.hits = None;
            return Task::none();
        }
        let Some(store) = store else {
            return Task::none();
        };
        Task::perform(
            jobs::read(store.clone(), move |conn| {
                let by_text = match query.is_empty() {
                    true => None,
                    false => Some(library::search_signals(conn, &query)?),
                };
                let by_tag = match tags.is_empty() {
                    true => None,
                    false => Some(library::signals_with_all_tags(conn, &tags)?),
                };
                let ids: Vec<SignalId> = match (by_text, by_tag) {
                    (Some(text), Some(tagged)) => {
                        text.into_iter().filter(|id| tagged.contains(id)).collect()
                    }
                    (Some(text), None) => text,
                    (None, Some(tagged)) => tagged,
                    (None, None) => Vec::new(),
                };
                ids.into_iter()
                    .map(|id| library::get_signal(conn, id))
                    .collect()
            }),
            Message::SearchDone,
        )
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let tree = container(self.tree())
            .width(Length::Fixed(360.0))
            .height(Length::Fill)
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.scale_alpha(0.5).into()),
                    ..container::Style::default()
                }
            });

        let detail = container(self.detail_pane())
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([12, 16]);

        row![tree, detail].height(Length::Fill).into()
    }

    fn tree(&self) -> Element<'_, Message> {
        let search_row = row![
            text_input("Search signals…", &self.search)
                .on_input(Message::SearchChanged)
                .on_submit(Message::Search)
                .size(13),
            button(text("Go").size(12))
                .padding([5, 9])
                .style(button::secondary)
                .on_press(Message::Search),
            button(text("Refresh").size(12))
                .padding([5, 9])
                .style(button::text)
                .on_press(Message::Refresh),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let mut list = column![].spacing(2).width(Length::Fill);

        if let Some(error) = &self.error {
            list = list.push(text(error).size(13).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            list = list.push(text(notice).size(12).style(text::success));
        }

        if let Some(index) = &self.index {
            if !index.tags.is_empty() {
                let mut chips = row![].spacing(4);
                for (tag, count) in &index.tags {
                    let active = self.tag_filter.contains(tag);
                    chips = chips.push(
                        button(text(format!("{tag} {count}")).size(11))
                            .padding([2, 8])
                            .style(if active {
                                button::primary
                            } else {
                                button::secondary
                            })
                            .on_press(Message::ToggleTag(tag.clone())),
                    );
                }
                list = list.push(container(chips.wrap()).padding([2, 0]));
            }
        }

        if let Some(hits) = &self.hits {
            let filter = if self.tag_filter.is_empty() {
                String::new()
            } else {
                format!(" tagged {}", self.tag_filter.join(" + "))
            };
            list = list.push(
                row![
                    text(format!(
                        "{} match{}{filter}",
                        hits.len(),
                        if hits.len() == 1 { "" } else { "es" }
                    ))
                    .size(12)
                    .style(text::secondary),
                    Space::with_width(Length::Fill),
                    button(text("Clear").size(11))
                        .padding([2, 6])
                        .style(button::text)
                        .on_press(Message::ClearSearch),
                ]
                .align_y(Alignment::Center),
            );
            for hit in hits {
                list = list.push(
                    row![
                        button(
                            column![
                                text(&hit.name).size(13),
                                text(format!(
                                    "{} · {} samples · group {}",
                                    hit.domain.label(),
                                    hit.sample_count,
                                    hit.group_id.get()
                                ))
                                .size(11)
                                .style(text::secondary),
                            ]
                            .spacing(1),
                        )
                        .width(Length::Fill)
                        .padding([4, 10])
                        .style(button::text)
                        .on_press(Message::SelectGroup(hit.group_id)),
                        button(text("Inspect").size(11))
                            .padding([2, 8])
                            .style(button::text)
                            .on_press(Message::Inspect(Target::Signal(hit.id))),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            list = list.push(horizontal_rule(1));
        }

        match &self.index {
            None if self.loading => {
                list = list.push(text("Loading library…").size(13).style(text::secondary));
            }
            None => {
                list = list.push(text("No library open.").size(13).style(text::secondary));
            }
            Some(index) if index.datasets.is_empty() => {
                list = list.push(
                    column![
                        text("The library is empty.").size(14),
                        text("Import a CSV file or generate a signal to add a dataset.")
                            .size(12)
                            .style(text::secondary),
                    ]
                    .spacing(4)
                    .padding([8, 4]),
                );
            }
            Some(index) => {
                for dataset in &index.datasets {
                    let trains = index.trains.get(&dataset.id).map_or(&[][..], Vec::as_slice);
                    let collapsed = self.collapsed.contains(&dataset.id);
                    list = list.push(
                        row![
                            button(
                                row![
                                    text(if collapsed { "▸" } else { "▾" }).size(13),
                                    text(&dataset.name).size(14),
                                    Space::with_width(Length::Fill),
                                    text(format!(
                                        "{} · {} train{}",
                                        dataset.source_kind.label(),
                                        trains.len(),
                                        if trains.len() == 1 { "" } else { "s" }
                                    ))
                                    .size(11)
                                    .style(text::secondary),
                                ]
                                .spacing(6)
                                .align_y(Alignment::Center),
                            )
                            .width(Length::Fill)
                            .padding([5, 8])
                            .style(button::text)
                            .on_press(Message::ToggleDataset(dataset.id)),
                            // Only an imported dataset carries the layout the
                            // writer reverses, so only one offers the export.
                            button(text("Export").size(11))
                                .padding([2, 6])
                                .style(button::text)
                                .on_press_maybe(
                                    dataset
                                        .attributes
                                        .contains_key(sp_csv::profile::Layout::ATTRIBUTE)
                                        .then_some(Message::ExportDataset(dataset.id)),
                                ),
                        ]
                        .align_y(Alignment::Center),
                    );
                    if collapsed {
                        continue;
                    }

                    for train in trains {
                        let folded = self.collapsed_trains.contains(&train.id);
                        let (group_count, records) = index.train_extent(train);
                        let noun = if train.is_pulse_train() {
                            "pulse"
                        } else {
                            "signal"
                        };
                        list = list.push(
                            button(
                                row![
                                    Space::with_width(Length::Fixed(12.0)),
                                    text(if folded { "▸" } else { "▾" }).size(12),
                                    text(train.display_name()).size(13),
                                    Space::with_width(Length::Fill),
                                    // The counts are the train's, across every
                                    // group in it: that is the capture (§6.6).
                                    text(format!(
                                        "{group_count} group{} · {records} {noun}{}",
                                        if group_count == 1 { "" } else { "s" },
                                        if records == 1 { "" } else { "s" },
                                    ))
                                    .size(11)
                                    .style(text::secondary),
                                ]
                                .spacing(5)
                                .align_y(Alignment::Center),
                            )
                            .width(Length::Fill)
                            .padding([4, 8])
                            .style(button::text)
                            .on_press(Message::ToggleTrain(train.id)),
                        );
                        if folded {
                            continue;
                        }

                        for group in index.groups.get(&train.id).into_iter().flatten() {
                            let active = self.selected == Some(group.id);
                            let kind = if group.is_pulse_group() {
                                format!("{} pulses", group.actual_count)
                            } else {
                                format!("{} signals", group.actual_count)
                            };
                            list = list.push(
                                button(
                                    row![
                                        Space::with_width(Length::Fixed(28.0)),
                                        text(group.display_name()).size(13),
                                        Space::with_width(Length::Fill),
                                        text(kind).size(11).style(if active {
                                            text::base
                                        } else {
                                            text::secondary
                                        }),
                                    ]
                                    .align_y(Alignment::Center),
                                )
                                .width(Length::Fill)
                                .padding([4, 8])
                                .style(if active {
                                    button::primary
                                } else {
                                    button::text
                                })
                                .on_press(Message::SelectGroup(group.id)),
                            );
                        }
                    }
                }
            }
        }

        column![
            container(search_row).padding([10, 10]),
            scrollable(container(list).padding([0, 6])).height(Length::Fill),
        ]
        .into()
    }

    fn detail_pane(&self) -> Element<'_, Message> {
        let Some(index) = &self.index else {
            return Space::new(Length::Fill, Length::Fill).into();
        };
        let Some(selected) = self.selected else {
            return container(
                text("Select a group to see its signals or pulse fields.")
                    .size(14)
                    .style(text::secondary),
            )
            .into();
        };
        let Some((dataset, train, group)) = index.group(selected) else {
            return Space::new(Length::Fill, Length::Fill).into();
        };

        let (train_groups, train_records) = index.train_extent(train);
        let mut header = column![
            text(group.display_name()).size(22),
            // The group is a segment, so the line says which capture of.
            text(format!(
                "{} · {} · block {} of {train_groups} · declared {} · read {}{}",
                dataset.name,
                train.display_name(),
                group.ordinal,
                group.declared_count,
                group.actual_count,
                group
                    .toa_unit
                    .map_or_else(String::new, |unit| format!(" · time of arrival in {unit}")),
            ))
            .size(12)
            .style(text::secondary),
            text(format!(
                "The train holds {train_records} record{} across {train_groups} group{}.",
                if train_records == 1 { "" } else { "s" },
                if train_groups == 1 { "" } else { "s" },
            ))
            .size(11)
            .style(text::secondary),
        ]
        .spacing(4);

        if !group.count_matches() {
            header = header.push(
                text("Declared count differs from rows read (tolerant import).")
                    .size(12)
                    .style(text::danger),
            );
        }
        if !group.attributes.is_empty() {
            let attrs = group
                .attributes
                .iter()
                .map(|(k, v)| format!("{k} = {}", value_text(v)))
                .collect::<Vec<_>>()
                .join("   ");
            header = header.push(text(attrs).size(12));
        }

        let body: Element<'_, Message> = if let Some(error) = &self.detail_error {
            text(error).size(13).style(text::danger).into()
        } else {
            match &self.detail {
                Some((id, detail)) if *id == selected => match detail {
                    GroupDetail::Signals(signals) => signal_table(signals),
                    GroupDetail::Pulses {
                        fields,
                        toa_s,
                        rows,
                        total,
                    } => pulse_table(group, fields, toa_s, rows, *total),
                },
                _ => text("Loading…").size(13).style(text::secondary).into(),
            }
        };

        column![
            header,
            Space::with_height(Length::Fixed(12.0)),
            horizontal_rule(1),
            Space::with_height(Length::Fixed(8.0)),
            scrollable(body).height(Length::Fill),
        ]
        .into()
    }
}

fn load_index(conn: &sp_store::Connection) -> sp_store::Result<LibraryIndex> {
    let datasets = library::list_datasets(conn)?;
    let mut index_trains = HashMap::with_capacity(datasets.len());
    let mut groups = HashMap::new();
    for dataset in &datasets {
        let list = trains::list_trains(conn, dataset.id)?;
        for train in &list {
            groups.insert(train.id, library::list_groups(conn, train.id)?);
        }
        index_trains.insert(dataset.id, list);
    }
    Ok(LibraryIndex {
        datasets,
        trains: index_trains,
        groups,
        summary: library::summary(conn)?,
        tags: library::tag_counts(conn)?,
    })
}

fn load_detail(
    conn: &sp_store::Connection,
    id: GroupId,
    is_pulse: bool,
) -> sp_store::Result<GroupDetail> {
    if !is_pulse {
        return Ok(GroupDetail::Signals(library::list_signals(conn, id)?));
    }
    let group = library::get_group(conn, id)?;
    let fields = pulses::list_fields(conn, id)?;
    let range = SampleRange::first(PULSE_PREVIEW_ROWS);
    let toa_s = pulses::read_toa(conn, id, range)?;
    let columns: Vec<Vec<f64>> = fields
        .iter()
        .map(|field| pulses::read_field(conn, id, field.ordinal, range).map(|b| b.to_f64()))
        .collect::<sp_store::Result<_>>()?;
    let rows = (0..toa_s.len())
        .map(|i| {
            columns
                .iter()
                .map(|col| col.get(i).copied().unwrap_or(f64::NAN))
                .collect()
        })
        .collect();
    Ok(GroupDetail::Pulses {
        fields,
        toa_s,
        rows,
        total: group.actual_count,
    })
}

const COLUMN_WIDTHS: [f32; 9] = [180.0, 110.0, 60.0, 110.0, 100.0, 90.0, 90.0, 90.0, 90.0];

fn cell<'a>(content: impl ToString, width: f32) -> Element<'a, Message> {
    container(text(content.to_string()).size(12))
        .width(Length::Fixed(width))
        .padding([3, 6])
        .into()
}

fn heading<'a>(content: impl ToString, width: f32) -> Element<'a, Message> {
    container(text(content.to_string()).size(11).style(text::secondary))
        .width(Length::Fixed(width))
        .padding([3, 6])
        .into()
}

fn signal_table<'a>(signals: &'a [Signal]) -> Element<'a, Message> {
    if signals.is_empty() {
        return text("This group has no signals.")
            .size(13)
            .style(text::secondary)
            .into();
    }
    let labels = [
        "Name", "Domain", "Type", "Rate", "Samples", "Min", "Max", "Mean", "RMS",
    ];
    let mut table = column![].spacing(0);
    let mut head = row![];
    for (label, width) in labels.iter().zip(COLUMN_WIDTHS) {
        head = head.push(heading(label, width));
    }
    head = head.push(heading("", 80.0));
    table = table.push(head).push(horizontal_rule(1));
    for signal in signals {
        let stats = signal.stats;
        let rate = signal
            .timebase
            .sample_rate_hz
            .map_or_else(|| "irregular".to_owned(), fmt_rate);
        let cells: [String; 9] = [
            signal.name.clone(),
            signal.domain.label().to_owned(),
            signal.dtype.to_string(),
            rate,
            signal.sample_count.to_string(),
            fmt_opt(stats.and_then(|s| s.min())),
            fmt_opt(stats.and_then(|s| s.max())),
            fmt_opt(stats.and_then(|s| s.mean())),
            fmt_opt(stats.and_then(|s| s.rms())),
        ];
        let mut line = row![].align_y(Alignment::Center);
        for (value, width) in cells.into_iter().zip(COLUMN_WIDTHS) {
            line = line.push(cell(value, width));
        }
        line = line.push(
            button(text("Inspect").size(11))
                .padding([2, 8])
                .style(button::text)
                .on_press(Message::Inspect(Target::Signal(signal.id))),
        );
        table = table.push(line);
    }
    table.into()
}

fn pulse_table<'a>(
    group: &'a SignalGroup,
    fields: &'a [PulseField],
    toa_s: &'a [f64],
    rows: &'a [Vec<f64>],
    total: u32,
) -> Element<'a, Message> {
    let unit = group.toa_unit.unwrap_or_default();
    let field_width = 120.0;

    let mut summary = column![text("Fields").size(14)].spacing(2);
    let mut head = row![
        heading("Field", 180.0),
        heading("Type", 60.0),
        heading("Min", 100.0),
        heading("Max", 100.0),
        heading("Mean", 100.0),
        heading("RMS", 100.0),
        heading("Missing", 80.0),
    ];
    head = head.push(Space::with_width(Length::Fill));
    summary = summary.push(head).push(horizontal_rule(1));
    for field in fields {
        let stats = field.stats;
        let name = match &field.unit {
            Some(unit) => format!("{} ({unit})", field.name),
            None => field.name.clone(),
        };
        summary = summary.push(
            row![
                cell(name, 180.0),
                cell(field.dtype, 60.0),
                cell(fmt_opt(stats.and_then(|s| s.min())), 100.0),
                cell(fmt_opt(stats.and_then(|s| s.max())), 100.0),
                cell(fmt_opt(stats.and_then(|s| s.mean())), 100.0),
                cell(fmt_opt(stats.and_then(|s| s.rms())), 100.0),
                cell(stats.map_or(0, |s| s.non_finite()), 80.0),
                button(text("Inspect").size(11))
                    .padding([2, 8])
                    .style(button::text)
                    .on_press(Message::Inspect(Target::PulseField {
                        group: group.id,
                        ordinal: field.ordinal,
                    })),
            ]
            .align_y(Alignment::Center),
        );
    }

    let shown = rows.len();
    let mut records =
        column![text(format!("Pulses — first {shown} of {total}")).size(14)].spacing(2);
    let mut head = row![heading("#", 60.0), heading(format!("TOA ({unit})"), 120.0)];
    for field in fields {
        head = head.push(heading(&field.name, field_width));
    }
    records = records.push(head).push(horizontal_rule(1));
    for (i, (toa, values)) in toa_s.iter().zip(rows).enumerate() {
        let mut line = row![cell(i, 60.0), cell(fmt_num(unit.from_seconds(*toa)), 120.0)];
        for value in values {
            line = line.push(cell(fmt_num(*value), field_width));
        }
        records = records.push(line);
    }

    column![summary, Space::with_height(Length::Fixed(18.0)), records].into()
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Six significant digits, trimmed; NaN shown as a gap.
pub fn fmt_num(value: f64) -> String {
    if value.is_nan() {
        return "—".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    let magnitude = value.abs();
    if !(1e-3..1e7).contains(&magnitude) {
        return format!("{value:.4e}");
    }
    let decimals = (5 - magnitude.log10().floor() as i32).clamp(0, 6) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

fn fmt_opt(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), fmt_num)
}

/// A sample rate with an SI prefix.
pub fn fmt_rate(hz: f64) -> String {
    if hz >= 1e9 {
        format!("{} GHz", fmt_num(hz / 1e9))
    } else if hz >= 1e6 {
        format!("{} MHz", fmt_num(hz / 1e6))
    } else if hz >= 1e3 {
        format!("{} kHz", fmt_num(hz / 1e3))
    } else {
        format!("{} Hz", fmt_num(hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_format_compactly() {
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(100.0), "100");
        assert_eq!(fmt_num(1.5), "1.5");
        assert_eq!(fmt_num(-0.25), "-0.25");
        assert_eq!(fmt_num(123_456.789), "123457");
        assert_eq!(fmt_num(1e-9), "1.0000e-9");
        assert_eq!(fmt_num(f64::NAN), "—");
        assert_eq!(fmt_rate(48_000.0), "48 kHz");
        assert_eq!(fmt_rate(1.0e6), "1 MHz");
        assert_eq!(fmt_rate(10.0), "10 Hz");
    }

    #[test]
    fn toggling_a_dataset_collapses_and_expands_it() {
        let mut state = State::default();
        let id = DatasetId::new(3);
        let _ = state.update(None, Message::ToggleDataset(id));
        assert!(state.collapsed.contains(&id));
        let _ = state.update(None, Message::ToggleDataset(id));
        assert!(!state.collapsed.contains(&id));
    }

    #[test]
    fn a_stale_detail_is_ignored() {
        let mut state = State::default();
        let _ = state.update(None, Message::SelectGroup(GroupId::new(1)));
        let _ = state.update(None, Message::SelectGroup(GroupId::new(2)));
        let _ = state.update(
            None,
            Message::DetailLoaded(GroupId::new(1), Ok(GroupDetail::Signals(Vec::new()))),
        );
        assert!(state.detail.is_none());
        let _ = state.update(
            None,
            Message::DetailLoaded(GroupId::new(2), Ok(GroupDetail::Signals(Vec::new()))),
        );
        assert!(matches!(state.detail, Some((id, _)) if id == GroupId::new(2)));
    }

    #[test]
    fn toggling_a_tag_narrows_and_widens_the_filter() {
        let mut state = State::default();
        let _ = state.update(None, Message::ToggleTag("golden".into()));
        let _ = state.update(None, Message::ToggleTag("noisy".into()));
        assert_eq!(state.tag_filter, ["golden", "noisy"]);
        let _ = state.update(None, Message::ToggleTag("golden".into()));
        assert_eq!(state.tag_filter, ["noisy"]);
    }

    #[test]
    fn clearing_drops_the_query_and_the_tag_filter_together() {
        let mut state = State {
            hits: Some(Vec::new()),
            search: "rf".into(),
            tag_filter: vec!["golden".into()],
            ..State::default()
        };
        let _ = state.update(None, Message::ClearSearch);
        assert!(state.hits.is_none());
        assert!(state.search.is_empty());
        assert!(state.tag_filter.is_empty());
    }

    #[test]
    fn a_tag_filter_survives_an_emptied_query() {
        let mut state = State {
            hits: Some(Vec::new()),
            tag_filter: vec!["golden".into()],
            ..State::default()
        };
        // Deleting the text leaves the tag filter — and its hits — in place.
        let _ = state.update(None, Message::SearchChanged(String::new()));
        assert!(state.hits.is_some());
    }

    #[test]
    fn inspect_is_handed_to_the_root_once() {
        let mut state = State::default();
        let target = Target::Signal(SignalId::new(7));
        let _ = state.update(None, Message::Inspect(target));
        assert_eq!(state.take_inspect_request(), Some(target));
        assert_eq!(state.take_inspect_request(), None);
    }

    #[test]
    fn a_cancelled_export_dialog_writes_nothing() {
        let mut state = State::default();
        let _ = state.update(None, Message::ExportTo(DatasetId::new(1), None));
        assert!(state.notice.is_none());
        assert!(state.error.is_none());
    }

    #[test]
    fn clearing_the_query_drops_the_hits() {
        let mut state = State {
            hits: Some(Vec::new()),
            ..State::default()
        };
        let _ = state.update(None, Message::SearchChanged("  ".into()));
        assert!(state.hits.is_none());
    }
}
