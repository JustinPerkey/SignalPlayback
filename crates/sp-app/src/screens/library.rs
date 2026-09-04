//! The Library screen (`docs/DESIGN.md` §12.1): a tree of dataset → group,
//! a detail table for the selected group, and full-text search over signals.
//!
//! Everything shown is metadata read from the store; a pulse group's table
//! previews the first rows of its columns and nothing more is materialised.

use std::collections::{HashMap, HashSet};

use iced::widget::{
    button, column, container, horizontal_rule, row, scrollable, text, text_input, Space,
};
use iced::{Alignment, Element, Length, Task, Theme};
use sp_core::group::DatasetId;
use sp_core::{Dataset, GroupId, PulseField, SampleRange, Signal, SignalGroup};
use sp_store::{library, pulses, LibrarySummary, Store};

use crate::jobs;

/// Pulse rows shown in the detail table before the user needs the Inspector.
const PULSE_PREVIEW_ROWS: u64 = 200;

/// Cached metadata for the whole library. Samples stay on disk.
#[derive(Debug, Clone, Default)]
pub struct LibraryIndex {
    pub datasets: Vec<Dataset>,
    pub groups: HashMap<DatasetId, Vec<SignalGroup>>,
    pub summary: LibrarySummary,
}

impl LibraryIndex {
    fn group(&self, id: GroupId) -> Option<(&Dataset, &SignalGroup)> {
        self.datasets.iter().find_map(|dataset| {
            self.groups
                .get(&dataset.id)?
                .iter()
                .find(|group| group.id == id)
                .map(|group| (dataset, group))
        })
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
    selected: Option<GroupId>,
    detail: Option<(GroupId, GroupDetail)>,
    detail_error: Option<String>,
    search: String,
    hits: Option<Vec<Signal>>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Loaded(Result<LibraryIndex, String>),
    ToggleDataset(DatasetId),
    SelectGroup(GroupId),
    DetailLoaded(GroupId, Result<GroupDetail, String>),
    SearchChanged(String),
    Search,
    SearchDone(Result<Vec<Signal>, String>),
    ClearSearch,
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
                    .is_some_and(|(_, group)| group.is_pulse_group());
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
                if self.search.trim().is_empty() {
                    self.hits = None;
                }
                Task::none()
            }
            Message::Search => {
                let query = self.search.trim().to_owned();
                match store {
                    Some(store) if !query.is_empty() => Task::perform(
                        jobs::read(store.clone(), move |conn| {
                            library::search_signals(conn, &query)?
                                .into_iter()
                                .map(|id| library::get_signal(conn, id))
                                .collect()
                        }),
                        Message::SearchDone,
                    ),
                    _ => Task::none(),
                }
            }
            Message::SearchDone(result) => {
                match result {
                    Ok(hits) => self.hits = Some(hits),
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::ClearSearch => {
                self.search.clear();
                self.hits = None;
                Task::none()
            }
        }
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

        if let Some(hits) = &self.hits {
            list = list.push(
                row![
                    text(format!("{} search result(s)", hits.len()))
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
                    let groups = index.groups.get(&dataset.id);
                    let collapsed = self.collapsed.contains(&dataset.id);
                    let arrow = if collapsed { "▸" } else { "▾" };
                    let group_count = groups.map_or(0, Vec::len);
                    list = list.push(
                        button(
                            row![
                                text(arrow).size(13),
                                text(&dataset.name).size(14),
                                Space::with_width(Length::Fill),
                                text(format!(
                                    "{} · {group_count} group{}",
                                    dataset.source_kind.label(),
                                    if group_count == 1 { "" } else { "s" }
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
                    );
                    if collapsed {
                        continue;
                    }
                    for group in groups.into_iter().flatten() {
                        let active = self.selected == Some(group.id);
                        let kind = if group.is_pulse_group() {
                            format!("{} pulses", group.actual_count)
                        } else {
                            format!("{} signals", group.actual_count)
                        };
                        list = list.push(
                            button(
                                row![
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
        let Some((dataset, group)) = index.group(selected) else {
            return Space::new(Length::Fill, Length::Fill).into();
        };

        let mut header = column![
            text(group.display_name()).size(22),
            text(format!(
                "{} · block {} · declared {} · read {}{}",
                dataset.name,
                group.ordinal,
                group.declared_count,
                group.actual_count,
                group
                    .toa_unit
                    .map_or_else(String::new, |unit| format!(" · time of arrival in {unit}")),
            ))
            .size(12)
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
    let mut groups = HashMap::with_capacity(datasets.len());
    for dataset in &datasets {
        groups.insert(dataset.id, library::list_groups(conn, dataset.id)?);
    }
    Ok(LibraryIndex {
        datasets,
        groups,
        summary: library::summary(conn)?,
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
        let mut line = row![];
        for (value, width) in cells.into_iter().zip(COLUMN_WIDTHS) {
            line = line.push(cell(value, width));
        }
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
        summary = summary.push(row![
            cell(name, 180.0),
            cell(field.dtype, 60.0),
            cell(fmt_opt(stats.and_then(|s| s.min())), 100.0),
            cell(fmt_opt(stats.and_then(|s| s.max())), 100.0),
            cell(fmt_opt(stats.and_then(|s| s.mean())), 100.0),
            cell(fmt_opt(stats.and_then(|s| s.rms())), 100.0),
            cell(stats.map_or(0, |s| s.non_finite()), 80.0),
        ]);
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
    fn clearing_the_query_drops_the_hits() {
        let mut state = State {
            hits: Some(Vec::new()),
            ..State::default()
        };
        let _ = state.update(None, Message::SearchChanged("  ".into()));
        assert!(state.hits.is_none());
    }
}
