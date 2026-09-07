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
//! Search, the tag filter and the property filter narrow together: a query
//! plus two tags plus `prf_hz >= 1000` asks for the signals that satisfy all
//! of them. Any one on its own works as you would expect, and clearing them
//! puts the tree back.
//! A row's `Inspect` button hands the Inspector its target (§12.1), and a
//! dataset exports back to the format it was imported from (§7.5, G1).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use iced::widget::{button, column, container, row, scrollable, text, text_input, Space};
use iced::{Alignment, Element, Length, Task, Theme};
use sp_core::group::DatasetId;
use sp_core::{
    Dataset, GroupId, PulseField, SampleRange, Signal, SignalGroup, SignalId, SignalTrain, TrainId,
};
use sp_store::{library, props, pulses, trains, LibrarySummary, PropertyQuery, Store};

use crate::jobs;
use crate::screens::inspector::Target;
use crate::typography;
use crate::ui::{
    cell, column_label, dim, heading, heading_aligned, rule, selectable, spec, value, warned,
};

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
    /// Property key the list is narrowed on, and the test its value must
    /// pass, as typed (§6.3).
    prop_key: String,
    prop_value: String,
    /// How the group's signal table is ordered: column index and direction.
    sort: Option<(usize, bool)>,
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
    PropKeyChanged(String),
    PropValueChanged(String),
    SortBy(usize),
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
            Message::PropKeyChanged(key) => {
                self.prop_key = key;
                if self.prop_key.trim().is_empty() && self.is_unfiltered() {
                    self.hits = None;
                }
                Task::none()
            }
            Message::PropValueChanged(value) => {
                self.prop_value = value;
                Task::none()
            }
            Message::SortBy(column) => {
                // Clicking the column that is already sorted turns it over.
                self.sort = match self.sort {
                    Some((current, ascending)) if current == column => Some((column, !ascending)),
                    _ => Some((column, true)),
                };
                Task::none()
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

    /// Whether nothing is narrowing the list.
    fn is_unfiltered(&self) -> bool {
        self.search.trim().is_empty()
            && self.tag_filter.is_empty()
            && self.prop_key.trim().is_empty()
    }

    /// Runs the text query, the tag filter and the property filter together.
    /// Every one of them narrows: with a query, two tags and a property test,
    /// a signal has to satisfy all of them.
    fn query(&mut self, store: Option<&Store>) -> Task<Message> {
        if self.is_unfiltered() {
            self.hits = None;
            return Task::none();
        }
        let Some(store) = store else {
            return Task::none();
        };
        let query = self.search.trim().to_owned();
        let tags = self.tag_filter.clone();
        let property = match self.prop_key.trim().is_empty() {
            true => None,
            false => Some((
                self.prop_key.trim().to_owned(),
                parse_property_query(&self.prop_value),
            )),
        };

        Task::perform(
            jobs::read(store.clone(), move |conn| {
                // Each filter is an intersection with what the ones before it
                // left, so they narrow rather than compete.
                let mut narrowed: Option<Vec<SignalId>> = None;
                let mut narrow = |ids: Vec<SignalId>| {
                    narrowed = Some(match narrowed.take() {
                        Some(kept) => ids.into_iter().filter(|id| kept.contains(id)).collect(),
                        None => ids,
                    });
                };
                if !query.is_empty() {
                    narrow(library::search_signals(conn, &query)?);
                }
                if !tags.is_empty() {
                    narrow(library::signals_with_all_tags(conn, &tags)?);
                }
                if let Some((key, test)) = &property {
                    narrow(props::find_signals(conn, key, test)?);
                }
                narrowed
                    .unwrap_or_default()
                    .into_iter()
                    .map(|id| library::get_signal(conn, id))
                    .collect()
            }),
            Message::SearchDone,
        )
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        // The rail sits on its own surface and is divided from the pane by a
        // hairline rather than by a shadow: the two are beside each other, not
        // one floating over the other.
        let tree = container(self.tree())
            .width(Length::Fixed(340.0))
            .height(Length::Fill)
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                let tokens = crate::theme::tokens(theme);
                container::Style {
                    background: Some(palette.background.weak.color.into()),
                    border: iced::Border {
                        color: tokens.rule,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..container::Style::default()
                }
            });

        let detail = container(self.detail_pane())
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([16, 20]);

        row![tree, detail].height(Length::Fill).into()
    }

    fn tree(&self) -> Element<'_, Message> {
        let search_row = row![
            text_input("Search signals…", &self.search)
                .on_input(Message::SearchChanged)
                .on_submit(Message::Search)
                .size(typography::BODY_SIZE),
            button(text("Go").size(typography::BODY_SIZE))
                .padding([5, 9])
                .style(button::secondary)
                .on_press(Message::Search),
            button(text("Refresh").size(typography::BODY_SIZE))
                .padding([5, 9])
                .style(button::text)
                .on_press(Message::Refresh),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        // A property filter is a key and a test over its value: `prf_hz` with
        // `>= 1000`, or `coding` with `nrz` (§6.3).
        let property_row = row![
            text_input("property", &self.prop_key)
                .on_input(Message::PropKeyChanged)
                .on_submit(Message::Search)
                .size(typography::BODY_SIZE)
                .width(Length::FillPortion(2)),
            text_input("any value", &self.prop_value)
                .on_input(Message::PropValueChanged)
                .on_submit(Message::Search)
                .size(typography::BODY_SIZE)
                .width(Length::FillPortion(3)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let mut list = column![].spacing(2).width(Length::Fill);

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

        if let Some(index) = &self.index {
            if !index.tags.is_empty() {
                // A tag and how many signals carry it are two different kinds
                // of thing, so they are set as two: the name in the label face,
                // the count monospaced beside it. A wall of solid pills said
                // neither, and spent the accent on every tag in the library
                // rather than on the ones that are on.
                let mut chips = row![].spacing(4);
                for (tag, count) in &index.tags {
                    let active = self.tag_filter.contains(tag);
                    chips = chips.push(
                        button(
                            row![
                                text(tag.to_uppercase()).size(typography::LABEL_SIZE).font(
                                    if active {
                                        typography::BODY_STRONG
                                    } else {
                                        typography::LABEL
                                    }
                                ),
                                text(count.to_string())
                                    .size(typography::LABEL_SIZE)
                                    .font(typography::READOUT)
                                    .style(if active { text::base } else { dim }),
                            ]
                            .spacing(6)
                            .align_y(Alignment::Center),
                        )
                        .padding([2, 8])
                        .style(selectable(active))
                        .on_press(Message::ToggleTag(tag.clone())),
                    );
                }
                list = list.push(container(chips.wrap()).padding([4, 0]));
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
                    .size(typography::LABEL_SIZE)
                    .font(typography::LABEL)
                    .style(dim),
                    Space::with_width(Length::Fill),
                    button(
                        text("Clear")
                            .size(typography::LABEL_SIZE)
                            .font(typography::LABEL)
                            .style(dim),
                    )
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
                                text(&hit.name).size(typography::BODY_SIZE),
                                text(format!(
                                    "{} · {} samples · group {}",
                                    hit.domain.label(),
                                    hit.sample_count,
                                    hit.group_id.get()
                                ))
                                .size(typography::LABEL_SIZE)
                                .font(typography::READOUT)
                                .style(dim),
                            ]
                            .spacing(1),
                        )
                        .width(Length::Fill)
                        .padding([4, 10])
                        .style(button::text)
                        .on_press(Message::SelectGroup(hit.group_id)),
                        button(text("Inspect").size(typography::LABEL_SIZE))
                            .padding([2, 8])
                            .style(button::text)
                            .on_press(Message::Inspect(Target::Signal(hit.id))),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            list = list.push(Space::with_height(Length::Fixed(6.0)));
            list = list.push(rule());
            list = list.push(Space::with_height(Length::Fixed(6.0)));
        }

        match &self.index {
            None if self.loading => {
                list = list.push(
                    text("Loading library…")
                        .size(typography::BODY_SIZE)
                        .style(dim),
                );
            }
            None => {
                list = list.push(
                    column![
                        text("No library open.").size(typography::BODY_SIZE),
                        text("Settings names the file this opens from.")
                            .size(typography::BODY_SIZE)
                            .style(dim),
                    ]
                    .spacing(4)
                    .padding([8, 4]),
                );
            }
            Some(index) if index.datasets.is_empty() => {
                list = list.push(
                    column![
                        text("The library is empty.").size(typography::BODY_SIZE),
                        text("Import a CSV file or generate a signal to add a dataset.")
                            .size(typography::BODY_SIZE)
                            .style(dim),
                    ]
                    .spacing(4)
                    .padding([8, 4]),
                );
            }
            Some(index) => {
                for (ordinal, dataset) in index.datasets.iter().enumerate() {
                    let trains = index.trains.get(&dataset.id).map_or(&[][..], Vec::as_slice);
                    let collapsed = self.collapsed.contains(&dataset.id);
                    // A dataset is the top of a branch, so it is set as a
                    // heading rather than as one more row at one less indent:
                    // small capitals, and air above it that the rows inside it
                    // do not get. The depth is legible from the type before it
                    // is legible from the indent.
                    if ordinal > 0 {
                        list = list.push(Space::with_height(Length::Fixed(14.0)));
                    }
                    list = list.push(
                        row![
                            button(
                                row![
                                    text(if collapsed { "▸" } else { "▾" })
                                        .size(typography::LABEL_SIZE)
                                        .style(dim),
                                    text(dataset.name.to_uppercase())
                                        .size(typography::LABEL_SIZE)
                                        .font(typography::LABEL),
                                    Space::with_width(Length::Fill),
                                    text(format!(
                                        "{} · {} train{}",
                                        dataset.source_kind.label(),
                                        trains.len(),
                                        if trains.len() == 1 { "" } else { "s" }
                                    ))
                                    .size(typography::LABEL_SIZE)
                                    .style(dim),
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
                            button(
                                text("Export")
                                    .size(typography::LABEL_SIZE)
                                    .font(typography::LABEL)
                                    .style(dim),
                            )
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
                                    Space::with_width(Length::Fixed(10.0)),
                                    text(if folded { "▸" } else { "▾" })
                                        .size(typography::LABEL_SIZE)
                                        .style(dim),
                                    text(train.display_name()).size(typography::BODY_SIZE),
                                    Space::with_width(Length::Fill),
                                    // The counts are the train's, across every
                                    // group in it: that is the capture (§6.6).
                                    text(format!(
                                        "{group_count} group{} · {records} {noun}{}",
                                        if group_count == 1 { "" } else { "s" },
                                        if records == 1 { "" } else { "s" },
                                    ))
                                    .size(typography::LABEL_SIZE)
                                    .font(typography::READOUT)
                                    .style(dim),
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
                            // A group is the only level of the tree that is
                            // picked rather than opened, so it is the only one
                            // that carries a selection at all. The name takes
                            // the medium weight when it is the one on show,
                            // which says so even where the tint does not carry.
                            list = list.push(
                                button(
                                    row![
                                        Space::with_width(Length::Fixed(24.0)),
                                        text(group.display_name())
                                            .size(typography::BODY_SIZE)
                                            .font(if active {
                                                typography::BODY_STRONG
                                            } else {
                                                typography::BODY
                                            }),
                                        Space::with_width(Length::Fill),
                                        text(kind)
                                            .size(typography::LABEL_SIZE)
                                            .font(typography::READOUT)
                                            .style(if active { text::base } else { dim }),
                                    ]
                                    .align_y(Alignment::Center),
                                )
                                .width(Length::Fill)
                                .padding([4, 8])
                                .style(selectable(active))
                                .on_press(Message::SelectGroup(group.id)),
                            );
                        }
                    }
                }
            }
        }

        column![
            container(column![search_row, property_row].spacing(6)).padding([10, 10]),
            scrollable(container(list).padding([0, 6])).height(Length::Fill),
        ]
        .into()
    }

    fn detail_pane(&self) -> Element<'_, Message> {
        let Some(index) = &self.index else {
            return Space::new(Length::Fill, Length::Fill).into();
        };
        let Some(selected) = self.selected else {
            // An empty state that names the shape of the tree is worth more
            // than one that says nothing is here: the reason a group is two
            // levels down is the reason the tree has three.
            return container(
                column![
                    text("Pick a group on the left.").size(typography::BODY_SIZE),
                    text(
                        "A dataset holds the trains that were captured from it,                          and a train holds the groups one capture resolved to."
                    )
                    .size(typography::BODY_SIZE)
                    .style(dim),
                ]
                .spacing(4)
                .max_width(460),
            )
            .into();
        };
        let Some((dataset, train, group)) = index.group(selected) else {
            return Space::new(Length::Fill, Length::Fill).into();
        };

        let (train_groups, train_records) = index.train_extent(train);

        // What the group is, as a panel states it: a caption over a reading,
        // one per fact. The line this replaced ran the same six facts together
        // with interpuncts and then said the last two again in a sentence, so
        // the reader had to parse a paragraph to find a number that was
        // already on screen.
        let mut strip = row![
            spec("DATASET", dataset.name.clone(), text::base),
            spec("TRAIN", train.display_name().to_owned(), text::base),
            spec(
                "BLOCK",
                format!("{} / {train_groups}", group.ordinal),
                text::base,
            ),
            spec("DECLARED", group.declared_count.to_string(), text::base),
            // A count that disagrees with what was declared is the one thing
            // on this strip that can be wrong, so it is the one thing that can
            // carry a colour.
            spec(
                "READ",
                group.actual_count.to_string(),
                if group.count_matches() {
                    text::base
                } else {
                    warned
                },
            ),
            spec(
                "IN TRAIN",
                format!("{train_records} across {train_groups}"),
                text::base,
            ),
        ]
        .spacing(24);

        if let Some(unit) = group.toa_unit {
            strip = strip.push(spec("TIME OF ARRIVAL", unit.to_string(), text::base));
        }

        let mut header = column![
            text(group.display_name())
                .size(typography::TITLE_SIZE)
                .font(typography::TITLE),
            strip.wrap(),
        ]
        .spacing(10);

        if !group.count_matches() {
            header = header.push(
                text("Read in tolerant mode: the file declared one count and held another.")
                    .size(typography::LABEL_SIZE)
                    .style(warned),
            );
        }
        if !group.attributes.is_empty() {
            // The attributes came out of the file and go back into it
            // unchanged (G1), so they are shown as they are stored: the key as
            // a caption, the value as a reading.
            let mut attributes = row![].spacing(20);
            for (key, held) in group.attributes.iter() {
                attributes = attributes.push(spec(key, value_text(held), text::base));
            }
            header = header.push(attributes.wrap());
        }

        let body: Element<'_, Message> = if let Some(error) = &self.detail_error {
            text(error)
                .size(typography::BODY_SIZE)
                .style(text::danger)
                .into()
        } else {
            match &self.detail {
                Some((id, detail)) if *id == selected => match detail {
                    GroupDetail::Signals(signals) => signal_table(signals, self.sort),
                    GroupDetail::Pulses {
                        fields,
                        toa_s,
                        rows,
                        total,
                    } => pulse_table(group, fields, toa_s, rows, *total),
                },
                _ => text("Loading…")
                    .size(typography::BODY_SIZE)
                    .style(dim)
                    .into(),
            }
        };

        // The header is what the group is; below the rule is what is in it.
        // The gap above the rule is wider than the gap below it, so the rule
        // belongs to the table rather than floating between two halves.
        column![
            header,
            Space::with_height(Length::Fixed(20.0)),
            rule(),
            Space::with_height(Length::Fixed(12.0)),
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

/// Which of those columns hold a reading rather than a word.
///
/// Name, domain and type are read as language and stay left, where the eye
/// starts. Everything from the sample rate rightwards is a quantity, and a
/// quantity is set monospaced and flush right so the column can be scanned
/// down instead of read across.
const NUMERIC_COLUMNS: [usize; 6] = [3, 4, 5, 6, 7, 8];

/// Orders a group's signals by the column the user clicked.
///
/// The sort is over the metadata already loaded — nine columns of one group —
/// so it never goes back to the store. A column of numbers sorts as numbers,
/// which is the whole point of sorting on `Samples` or `RMS`; the rest sort as
/// text. A signal with no cached statistics sorts last either way, because
/// "unknown" is not "zero".
fn sorted_signals(signals: &[Signal], sort: Option<(usize, bool)>) -> Vec<&Signal> {
    let mut ordered: Vec<&Signal> = signals.iter().collect();
    let Some((column, ascending)) = sort else {
        return ordered;
    };
    let number = |signal: &Signal| -> Option<f64> {
        let stats = signal.stats;
        match column {
            3 => signal.timebase.sample_rate_hz,
            4 => Some(signal.sample_count as f64),
            5 => stats.and_then(|s| s.min()),
            6 => stats.and_then(|s| s.max()),
            7 => stats.and_then(|s| s.mean()),
            8 => stats.and_then(|s| s.rms()),
            _ => None,
        }
    };
    let text_of = |signal: &Signal| -> String {
        match column {
            1 => signal.domain.label().to_lowercase(),
            2 => signal.dtype.to_string(),
            _ => signal.name.to_lowercase(),
        }
    };

    if (3..=8).contains(&column) {
        ordered.sort_by(|a, b| match (number(a), number(b)) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
    } else {
        ordered.sort_by_key(|signal| text_of(signal));
    }
    if !ascending {
        ordered.reverse();
    }
    ordered
}

fn signal_table(signals: &[Signal], sort: Option<(usize, bool)>) -> Element<'_, Message> {
    if signals.is_empty() {
        return text("This group has no signals.")
            .size(typography::BODY_SIZE)
            .style(dim)
            .into();
    }
    let labels = [
        "Name", "Domain", "Type", "Rate", "Samples", "Min", "Max", "Mean", "RMS",
    ];
    let mut table = column![].spacing(0);
    let mut head = row![];
    for (index, (label, width)) in labels.iter().zip(COLUMN_WIDTHS).enumerate() {
        // The heading is the control: clicking it sorts, clicking it again
        // turns the order over.
        let marker = match sort {
            Some((column, true)) if column == index => " ▲",
            Some((column, false)) if column == index => " ▼",
            _ => "",
        };
        head = head.push(
            button(
                container(column_label(
                    format!("{label}{marker}"),
                    matches!(sort, Some((column, _)) if column == index),
                ))
                .width(Length::Fill)
                .align_x(if NUMERIC_COLUMNS.contains(&index) {
                    Alignment::End
                } else {
                    Alignment::Start
                }),
            )
            .width(Length::Fixed(width))
            .padding([3, 6])
            .style(button::text)
            .on_press(Message::SortBy(index)),
        );
    }
    head = head.push(heading("", 80.0));
    table = table.push(head).push(rule());
    for signal in sorted_signals(signals, sort) {
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
        for (index, (content, width)) in cells.into_iter().zip(COLUMN_WIDTHS).enumerate() {
            line = line.push(if NUMERIC_COLUMNS.contains(&index) {
                value(content, width)
            } else {
                cell(content, width)
            });
        }
        line = line.push(
            button(text("Inspect").size(typography::LABEL_SIZE))
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

    let mut summary = column![text("FIELDS")
        .size(typography::LABEL_SIZE)
        .font(typography::LABEL)
        .style(dim)]
    .spacing(4);
    let mut head = row![
        heading("Field", 180.0),
        heading("Type", 60.0),
        heading_aligned("Min", 100.0, Alignment::End),
        heading_aligned("Max", 100.0, Alignment::End),
        heading_aligned("Mean", 100.0, Alignment::End),
        heading_aligned("RMS", 100.0, Alignment::End),
        heading_aligned("Missing", 80.0, Alignment::End),
    ];
    head = head.push(Space::with_width(Length::Fill));
    summary = summary.push(head).push(rule());
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
                value(fmt_opt(stats.and_then(|s| s.min())), 100.0),
                value(fmt_opt(stats.and_then(|s| s.max())), 100.0),
                value(fmt_opt(stats.and_then(|s| s.mean())), 100.0),
                value(fmt_opt(stats.and_then(|s| s.rms())), 100.0),
                value(stats.map_or(0, |s| s.non_finite()), 80.0),
                button(text("Inspect").size(typography::LABEL_SIZE))
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
    let mut records = column![row![
        text("PULSES")
            .size(typography::LABEL_SIZE)
            .font(typography::LABEL)
            .style(dim),
        text(format!("first {shown} of {total}"))
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT)
            .style(dim),
    ]
    .spacing(8)]
    .spacing(4);
    let mut head = row![
        heading_aligned("#", 60.0, Alignment::End),
        heading_aligned(format!("TOA ({unit})"), 120.0, Alignment::End),
    ];
    for field in fields {
        head = head.push(heading_aligned(&field.name, field_width, Alignment::End));
    }
    records = records.push(head).push(rule());
    for (i, (toa, values)) in toa_s.iter().zip(rows).enumerate() {
        let mut line = row![
            value(i, 60.0),
            value(fmt_num(unit.from_seconds(*toa)), 120.0)
        ];
        for reading in values {
            line = line.push(value(fmt_num(*reading), field_width));
        }
        records = records.push(line);
    }

    column![summary, Space::with_height(Length::Fixed(18.0)), records].into()
}

/// Turns what the user typed into a property test (§6.3).
///
/// The forms are the ones people write in a filter box, and anything else is
/// taken as text to match exactly, because a property's value is as often a
/// word as a number:
///
/// | Typed | Means |
/// |-------|-------|
/// | (empty) | the property is present, whatever its value |
/// | `1000` | exactly that number |
/// | `>= 1000`, `> 1000` | at least that |
/// | `<= 1000`, `< 1000` | at most that |
/// | `100..2000` | between the two, inclusive |
/// | `nrz` | that text |
///
/// The strict forms `>` and `<` are read as their inclusive counterparts: the
/// index is a range index, and a filter box is not the place to lose a row to
/// a boundary the user did not think about.
pub fn parse_property_query(raw: &str) -> PropertyQuery {
    let raw = raw.trim();
    if raw.is_empty() {
        return PropertyQuery::Exists;
    }
    let number = |text: &str| text.trim().parse::<f64>().ok();

    if let Some((low, high)) = raw.split_once("..") {
        let min = number(low);
        let max = number(high);
        if min.is_some() || max.is_some() {
            return PropertyQuery::Between { min, max };
        }
    }
    for prefix in [">=", ">"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            if let Some(min) = number(rest) {
                return PropertyQuery::Between {
                    min: Some(min),
                    max: None,
                };
            }
        }
    }
    for prefix in ["<=", "<"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            if let Some(max) = number(rest) {
                return PropertyQuery::Between {
                    min: None,
                    max: Some(max),
                };
            }
        }
    }
    let exact = raw.strip_prefix('=').unwrap_or(raw);
    match number(exact) {
        Some(value) => PropertyQuery::Between {
            min: Some(value),
            max: Some(value),
        },
        None => PropertyQuery::Equals(exact.trim().to_owned()),
    }
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
    fn a_property_filter_reads_the_forms_people_type() {
        assert_eq!(parse_property_query(""), PropertyQuery::Exists);
        assert_eq!(parse_property_query("   "), PropertyQuery::Exists);
        assert_eq!(
            parse_property_query("1000"),
            PropertyQuery::Between {
                min: Some(1000.0),
                max: Some(1000.0)
            }
        );
        assert_eq!(
            parse_property_query(">= 1000"),
            PropertyQuery::Between {
                min: Some(1000.0),
                max: None
            }
        );
        // A strict bound is read as its inclusive counterpart, on purpose.
        assert_eq!(
            parse_property_query("> 1000"),
            parse_property_query(">=1000")
        );
        assert_eq!(
            parse_property_query("<= 1e6"),
            PropertyQuery::Between {
                min: None,
                max: Some(1e6)
            }
        );
        assert_eq!(
            parse_property_query("100..2000"),
            PropertyQuery::Between {
                min: Some(100.0),
                max: Some(2000.0)
            }
        );
        assert_eq!(
            parse_property_query("..2000"),
            PropertyQuery::Between {
                min: None,
                max: Some(2000.0)
            }
        );
        // Anything that is not a number is text to match.
        assert_eq!(
            parse_property_query("nrz"),
            PropertyQuery::Equals("nrz".into())
        );
        assert_eq!(
            parse_property_query("= nrz"),
            PropertyQuery::Equals("nrz".into())
        );
    }

    #[test]
    fn a_property_key_alone_is_a_filter() {
        let mut state = State::default();
        assert!(state.is_unfiltered());
        let _ = state.update(None, Message::PropKeyChanged("prf_hz".into()));
        assert!(!state.is_unfiltered());
        let _ = state.update(None, Message::PropKeyChanged(String::new()));
        assert!(state.is_unfiltered());
    }

    #[test]
    fn a_column_sorts_and_then_reverses() {
        let mut state = State::default();
        let _ = state.update(None, Message::SortBy(4));
        assert_eq!(state.sort, Some((4, true)));
        let _ = state.update(None, Message::SortBy(4));
        assert_eq!(state.sort, Some((4, false)), "the same column turns over");
        let _ = state.update(None, Message::SortBy(0));
        assert_eq!(
            state.sort,
            Some((0, true)),
            "another column starts ascending"
        );
    }

    #[test]
    fn sorting_orders_numbers_as_numbers_and_puts_the_unknown_last() {
        use sp_core::{DType, Domain, Provenance, Timebase};

        let signal = |name: &str, samples: u64, stats: Option<sp_core::SignalStats>| Signal {
            id: SignalId::new(samples as i64 + 1),
            group_id: GroupId::new(1),
            ordinal: 0,
            name: name.to_owned(),
            units: None,
            dtype: DType::F32,
            domain: Domain::Analog,
            provenance: Provenance::Imported,
            timebase: Timebase::regular(1_000.0, 0.0),
            sample_count: samples,
            stats,
            attributes: sp_core::Attributes::new(),
        };
        let stats = |values: &[f64]| Some(values.iter().copied().collect::<sp_core::SignalStats>());
        let signals = vec![
            signal("b", 100, stats(&[5.0])),
            signal("a", 9, stats(&[1.0])),
            signal("c", 50, None),
        ];

        // 9 before 100: the column is numeric, not text.
        let by_samples = sorted_signals(&signals, Some((4, true)));
        assert_eq!(
            by_samples
                .iter()
                .map(|s| s.sample_count)
                .collect::<Vec<_>>(),
            [9, 50, 100]
        );
        // A signal with no statistics has no maximum, so it sorts last either
        // way rather than pretending to be zero.
        let by_max = sorted_signals(&signals, Some((6, true)));
        assert_eq!(by_max.last().unwrap().name, "c");
        let by_max_desc = sorted_signals(&signals, Some((6, false)));
        assert_eq!(by_max_desc.first().unwrap().name, "c");
        // And the name column is text.
        let by_name = sorted_signals(&signals, Some((0, true)));
        assert_eq!(
            by_name.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        // Unsorted is the order the store returned.
        assert_eq!(sorted_signals(&signals, None)[0].name, "b");
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
