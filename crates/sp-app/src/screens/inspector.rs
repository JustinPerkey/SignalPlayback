//! The Inspector screen (`docs/DESIGN.md` §12.1): everything about one signal
//! or one pulse field.
//!
//! This is where the library's metadata becomes editable — the name, the
//! property values, the tags — and where a column's actual numbers are
//! visible: the statistics computed from the samples themselves rather than
//! the cached row, the distribution behind them, and a table of the values.
//!
//! Two things keep the screen honest about size. The statistics and the
//! histogram come from one streaming pass in the store (`sp_store::stats`), so
//! a 100 M-sample signal costs a read, not a copy. The value table is
//! virtualised: it holds one page of rows, and paging asks the store for the
//! next span rather than materialising the column.
//!
//! A pulse group's table is the pulse records themselves — one row per pulse
//! across every field of the group (§6.6) — because a single field's values
//! out of the context of the record they came from are rarely what the user
//! is after.

use std::collections::BTreeMap;

use iced::widget::canvas::Cache;
use iced::widget::{button, canvas, column, container, row, scrollable, text, text_input, Space};
use iced::{Alignment, Element, Length, Task, Theme};
use sp_core::props::{PropertyDef, PropertyValue};
use sp_core::{
    Attributes, Dataset, GroupId, PropScope, PulseField, SampleRange, Signal, SignalGroup,
    SignalId, SignalTrain, TimeUnit,
};
use sp_store::stats::ColumnProfile;
use sp_store::{blob, library, props, pulses, stats, trains, BlobInfo, Store};

use crate::jobs;
use crate::screens::library::{fmt_num, fmt_rate};
use crate::typography;
use crate::ui;
use crate::widgets::histogram::HistogramView;

/// Rows of the value table shown at once. The table is a window on the
/// column, not a copy of it.
const PAGE: u64 = 200;

/// What the Inspector is looking at (§12.2).
///
/// A pulse is addressed as a row of its group's record table rather than as a
/// target of its own: an unannotated pulse has no row to point at (§6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Signal(SignalId),
    PulseField { group: GroupId, ordinal: u32 },
}

/// The metadata behind the target, loaded in one read.
#[derive(Debug, Clone)]
pub enum Detail {
    Signal(Box<SignalDetail>),
    Pulses(Box<PulseDetail>),
}

#[derive(Debug, Clone)]
pub struct SignalDetail {
    pub signal: Signal,
    pub group: SignalGroup,
    pub train: SignalTrain,
    pub dataset: Dataset,
    pub tags: Vec<String>,
    pub defs: Vec<PropertyDef>,
    pub blob: Option<BlobInfo>,
}

#[derive(Debug, Clone)]
pub struct PulseDetail {
    pub group: SignalGroup,
    pub train: SignalTrain,
    pub dataset: Dataset,
    /// Every field of the group, in column order; the table shows them all.
    pub fields: Vec<PulseField>,
    /// Which of them the Inspector is focused on.
    pub ordinal: u32,
}

impl PulseDetail {
    fn focused(&self) -> Option<&PulseField> {
        self.fields.iter().find(|f| f.ordinal == self.ordinal)
    }
}

/// One page of the value table.
#[derive(Debug, Clone)]
pub struct ValuePage {
    pub start: u64,
    /// Time of each row, in seconds — the signal's timeline, or the group's
    /// time of arrival.
    pub times: Vec<f64>,
    /// One row per value; a signal has a single column, a pulse group one per
    /// field.
    pub rows: Vec<Vec<f64>>,
}

#[derive(Debug)]
pub struct State {
    target: Option<Target>,
    detail: Option<Detail>,
    profile: Option<ColumnProfile>,
    profiling: bool,
    values: Option<ValuePage>,
    page_start: u64,
    jump_draft: String,
    name_draft: String,
    tag_draft: String,
    /// Raw text per property key, so a half-typed value stays as typed.
    prop_drafts: BTreeMap<String, String>,
    prop_error: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    bins: usize,
    histogram_cache: Cache,
}

impl Default for State {
    fn default() -> Self {
        Self {
            target: None,
            detail: None,
            profile: None,
            profiling: false,
            values: None,
            page_start: 0,
            jump_draft: String::new(),
            name_draft: String::new(),
            tag_draft: String::new(),
            prop_drafts: BTreeMap::new(),
            prop_error: None,
            error: None,
            notice: None,
            bins: crate::settings::DEFAULT_BINS,
            histogram_cache: Cache::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Show(Target),
    Refresh,
    Loaded(Target, Result<Detail, String>),
    Profiled(Target, Result<Box<ColumnProfile>, String>),
    ValuesLoaded(Target, Result<ValuePage, String>),
    NameChanged(String),
    Rename,
    TagDraftChanged(String),
    AddTag,
    RemoveTag(String),
    PropChanged(String, String),
    SaveProperties,
    Saved(Result<(), String>),
    Page(i64),
    JumpChanged(String),
    Jump,
}

impl State {
    /// Points the Inspector at something and starts loading it.
    pub fn show(&mut self, store: Option<&Store>, target: Target) -> Task<Message> {
        self.target = Some(target);
        self.detail = None;
        self.profile = None;
        self.values = None;
        self.page_start = 0;
        self.jump_draft.clear();
        self.tag_draft.clear();
        self.prop_drafts.clear();
        self.prop_error = None;
        self.error = None;
        self.notice = None;
        self.histogram_cache.clear();
        self.reload(store)
    }

    /// Histogram resolution, from Settings.
    pub fn set_bins(&mut self, bins: usize, store: Option<&Store>) -> Task<Message> {
        if self.bins == bins {
            return Task::none();
        }
        self.bins = bins;
        self.histogram_cache.clear();
        match (self.target, store) {
            (Some(target), Some(store)) => self.profile_task(store, target),
            _ => Task::none(),
        }
    }

    /// Re-reads everything about the current target.
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        self.reload(Some(store))
    }

    fn reload(&mut self, store: Option<&Store>) -> Task<Message> {
        let (Some(store), Some(target)) = (store, self.target) else {
            return Task::none();
        };
        self.profiling = true;
        Task::batch([
            self.detail_task(store, target),
            self.profile_task(store, target),
            self.values_task(store, target, 0),
        ])
    }

    /// Re-reads the metadata alone.
    ///
    /// Renaming, tagging or editing a property changes a row, never a sample,
    /// so nothing about the statistics or the values can have moved — and
    /// re-profiling would re-read the whole column to learn that.
    fn reload_detail(&self, store: Option<&Store>) -> Task<Message> {
        match (store, self.target) {
            (Some(store), Some(target)) => self.detail_task(store, target),
            _ => Task::none(),
        }
    }

    fn detail_task(&self, store: &Store, target: Target) -> Task<Message> {
        Task::perform(
            jobs::read(store.clone(), move |conn| load_detail(conn, target)),
            move |result| Message::Loaded(target, result),
        )
    }

    fn profile_task(&self, store: &Store, target: Target) -> Task<Message> {
        let bins = self.bins;
        Task::perform(
            jobs::read(store.clone(), move |conn| {
                let profile = match target {
                    Target::Signal(id) => stats::profile_signal(conn, id, bins)?,
                    Target::PulseField { group, ordinal } => {
                        stats::profile_pulse_field(conn, group, ordinal, bins)?
                    }
                };
                Ok(Box::new(profile))
            }),
            move |result| Message::Profiled(target, result),
        )
    }

    fn values_task(&self, store: &Store, target: Target, start: u64) -> Task<Message> {
        Task::perform(
            jobs::read(store.clone(), move |conn| load_values(conn, target, start)),
            move |result| Message::ValuesLoaded(target, result),
        )
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Show(target) => self.show(store, target),
            Message::Refresh => self.reload(store),
            Message::Loaded(target, result) => {
                if self.target != Some(target) {
                    return Task::none();
                }
                match result {
                    Ok(detail) => {
                        self.name_draft = match &detail {
                            Detail::Signal(detail) => detail.signal.name.clone(),
                            Detail::Pulses(detail) => detail
                                .focused()
                                .map(|field| field.name.clone())
                                .unwrap_or_default(),
                        };
                        self.prop_drafts = property_drafts(&detail);
                        self.detail = Some(detail);
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::Profiled(target, result) => {
                if self.target != Some(target) {
                    return Task::none();
                }
                self.profiling = false;
                self.histogram_cache.clear();
                match result {
                    Ok(profile) => self.profile = Some(*profile),
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::ValuesLoaded(target, result) => {
                if self.target != Some(target) {
                    return Task::none();
                }
                match result {
                    Ok(page) => {
                        self.page_start = page.start;
                        self.values = Some(page);
                    }
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::NameChanged(name) => {
                self.name_draft = name;
                Task::none()
            }
            Message::Rename => {
                let (Some(store), Some(Target::Signal(id))) = (store, self.target) else {
                    // A pulse field's name is the source file's header text and
                    // is not the user's to change (§6.6).
                    return Task::none();
                };
                let name = self.name_draft.trim().to_owned();
                if name.is_empty() {
                    self.error = Some("A signal needs a name.".to_owned());
                    return Task::none();
                }
                self.notice = Some(format!("Renamed to '{name}'."));
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        library::rename_signal(conn, id, &name)
                    }),
                    Message::Saved,
                )
            }
            Message::TagDraftChanged(tag) => {
                self.tag_draft = tag;
                Task::none()
            }
            Message::AddTag => {
                let (Some(store), Some(Target::Signal(id))) = (store, self.target) else {
                    return Task::none();
                };
                let tag = self.tag_draft.trim().to_owned();
                if tag.is_empty() {
                    return Task::none();
                }
                self.tag_draft.clear();
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        library::tag_signal(conn, id, &tag)
                    }),
                    Message::Saved,
                )
            }
            Message::RemoveTag(tag) => {
                let (Some(store), Some(Target::Signal(id))) = (store, self.target) else {
                    return Task::none();
                };
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        library::untag_signal(conn, id, &tag)
                    }),
                    Message::Saved,
                )
            }
            Message::PropChanged(key, raw) => {
                self.prop_drafts.insert(key, raw);
                Task::none()
            }
            Message::SaveProperties => {
                let (Some(store), Some(Detail::Signal(detail))) = (store, self.detail.as_ref())
                else {
                    return Task::none();
                };
                let attributes = match self.edited_attributes(detail) {
                    Ok(attributes) => attributes,
                    Err(error) => {
                        self.prop_error = Some(error);
                        return Task::none();
                    }
                };
                self.prop_error = None;
                self.notice = Some("Properties saved.".to_owned());
                let id = detail.signal.id;
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        library::update_signal_attributes(conn, id, &attributes)
                    }),
                    Message::Saved,
                )
            }
            Message::Saved(result) => {
                match result {
                    Ok(()) => self.error = None,
                    Err(error) => {
                        self.notice = None;
                        self.error = Some(error);
                    }
                }
                // Whatever changed, the row it changed is what the screen is
                // showing, so read it back rather than patching it in place —
                // the row only, since none of these edits touches a sample.
                self.reload_detail(store)
            }
            Message::Page(delta) => {
                let start = self.page_start as i64 + delta * PAGE as i64;
                self.go_to(store, start.max(0) as u64)
            }
            Message::JumpChanged(raw) => {
                self.jump_draft = raw;
                Task::none()
            }
            Message::Jump => match self.jump_draft.trim().parse::<u64>() {
                Ok(index) => {
                    // Land on the page holding that index rather than starting
                    // the table at it, so the row keeps its neighbours.
                    self.go_to(store, index / PAGE * PAGE)
                }
                Err(_) => {
                    self.error = Some("Type a sample index to jump to.".to_owned());
                    Task::none()
                }
            },
        }
    }

    fn go_to(&mut self, store: Option<&Store>, start: u64) -> Task<Message> {
        let start = start.min(self.last_page_start());
        let (Some(store), Some(target)) = (store, self.target) else {
            return Task::none();
        };
        self.values_task(store, target, start)
    }

    /// Rows in the column being shown.
    fn row_count(&self) -> u64 {
        match &self.detail {
            Some(Detail::Signal(detail)) => detail.signal.sample_count,
            Some(Detail::Pulses(detail)) => u64::from(detail.group.actual_count),
            None => 0,
        }
    }

    fn last_page_start(&self) -> u64 {
        self.row_count().saturating_sub(1) / PAGE * PAGE
    }

    /// The attributes the property form describes, or the first parse error.
    fn edited_attributes(&self, detail: &SignalDetail) -> Result<Attributes, String> {
        let mut attributes = detail.signal.attributes.clone();
        for def in &detail.defs {
            let Some(raw) = self.prop_drafts.get(&def.key) else {
                continue;
            };
            if raw.trim().is_empty() {
                attributes.remove(&def.key);
                continue;
            }
            let value = def.parse(raw).map_err(|error| error.to_string())?;
            if value == PropertyValue::Null {
                attributes.remove(&def.key);
            } else {
                attributes.insert(def.key.clone(), value);
            }
        }
        Ok(attributes)
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let Some(detail) = &self.detail else {
            let state = if self.target.is_some() {
                ui::empty(
                    "Reading the column…",
                    "The samples are read from the blob on disk, not from memory.",
                )
            } else {
                ui::empty(
                    "Nothing is being inspected.",
                    "Pick a signal or a pulse field in the Library and choose Inspect. \
                     This screen reads the values themselves, a page at a time.",
                )
            };
            return container(state)
                .padding(24)
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        };

        let left = column![self.header(detail), self.statistics(), self.value_table()]
            .spacing(24)
            .width(Length::Fill);

        let panel = container(scrollable(self.side_panel(detail)))
            .width(Length::Fixed(320.0))
            .height(Length::Fill)
            .padding([16, 16])
            .style(ui::panel);

        row![
            container(scrollable(left))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding([16, 20]),
            panel,
        ]
        .height(Length::Fill)
        .into()
    }

    fn header<'a>(&'a self, detail: &'a Detail) -> Element<'a, Message> {
        let (title, context, rows) = match detail {
            Detail::Signal(detail) => {
                let signal = &detail.signal;
                let rate = signal
                    .timebase
                    .sample_rate_hz
                    .map_or_else(|| "irregular".to_owned(), fmt_rate);
                let duration = signal
                    .duration_s()
                    .map_or_else(|| "—".to_owned(), |s| format!("{} s", fmt_num(s)));
                let rows = vec![
                    ("Signal id", signal.id.get().to_string()),
                    ("Type", signal.dtype.to_string()),
                    ("Domain", signal.domain.label().to_owned()),
                    ("Provenance", signal.provenance.as_str().to_owned()),
                    ("Units", signal.units.clone().unwrap_or_else(|| "—".into())),
                    ("Sample rate", rate),
                    ("Start time", format!("{} s", fmt_num(signal.timebase.t0_s))),
                    ("Duration", duration),
                    ("Samples", signal.sample_count.to_string()),
                    (
                        "Payload",
                        crate::screens::settings::fmt_bytes(signal.payload_bytes()),
                    ),
                    (
                        "Blob",
                        detail.blob.as_ref().map_or_else(
                            || "—".to_owned(),
                            |blob| {
                                format!(
                                    "{} · {}",
                                    &blob.checksum[..blob.checksum.len().min(12)],
                                    crate::screens::settings::fmt_bytes(blob.byte_len)
                                )
                            },
                        ),
                    ),
                ];
                (
                    signal.name.clone(),
                    format!(
                        "{} · {} · {}",
                        detail.dataset.name,
                        detail.train.display_name(),
                        detail.group.display_name()
                    ),
                    rows,
                )
            }
            Detail::Pulses(detail) => {
                let field = detail.focused();
                let name = field.map_or_else(
                    || format!("field {}", detail.ordinal),
                    |field| match &field.unit {
                        Some(unit) => format!("{} ({unit})", field.name),
                        None => field.name.clone(),
                    },
                );
                let rows = vec![
                    ("Group id", detail.group.id.get().to_string()),
                    ("Column", detail.ordinal.to_string()),
                    (
                        "Property key",
                        field.map(|f| f.key.clone()).unwrap_or_default(),
                    ),
                    (
                        "Type",
                        field.map(|f| f.dtype.to_string()).unwrap_or_default(),
                    ),
                    (
                        "Unit",
                        field
                            .and_then(|f| f.unit.clone())
                            .unwrap_or_else(|| "—".into()),
                    ),
                    ("Pulses", detail.group.actual_count.to_string()),
                    ("Declared", detail.group.declared_count.to_string()),
                    (
                        "Time of arrival",
                        detail
                            .group
                            .toa_unit
                            .map_or_else(|| "—".to_owned(), |unit| unit.to_string()),
                    ),
                    ("Fields in the group", detail.fields.len().to_string()),
                ];
                (
                    name,
                    format!(
                        "{} · {} · {}",
                        detail.dataset.name,
                        detail.train.display_name(),
                        detail.group.display_name()
                    ),
                    rows,
                )
            }
        };

        let mut header = column![
            row![
                text(title)
                    .size(typography::TITLE_SIZE)
                    .font(typography::TITLE),
                Space::with_width(Length::Fill),
                button(
                    text("Reload")
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL)
                        .style(ui::dim),
                )
                .padding([3, 8])
                .style(button::text)
                .on_press(Message::Refresh),
            ]
            .align_y(Alignment::Center),
            // Where in the library this came from, in the library's own terms.
            text(context)
                .size(typography::BODY_SIZE)
                .font(typography::READOUT)
                .style(ui::dim),
        ]
        .spacing(3);

        if let Some(error) = &self.error {
            header = header.push(text(error).size(typography::BODY_SIZE).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            header = header.push(
                text(notice)
                    .size(typography::BODY_SIZE)
                    .style(text::success),
            );
        }

        // Two columns of label/value, so the metadata reads as a block rather
        // than a long ladder.
        let half = rows.len().div_ceil(2);
        let mut left = column![].spacing(2);
        let mut right = column![].spacing(2);
        for (index, (label, value)) in rows.into_iter().enumerate() {
            // Caption then reading, in two fixed columns, so the whole block
            // can be read down either side without tracking across.
            let line = row![
                container(ui::caption(label)).width(Length::Fixed(130.0)),
                text(value)
                    .size(typography::BODY_SIZE)
                    .font(typography::READOUT),
            ]
            .align_y(Alignment::Center);
            if index < half {
                left = left.push(line);
            } else {
                right = right.push(line);
            }
        }

        header
            .push(Space::with_height(Length::Fixed(12.0)))
            .push(row![left, right].spacing(32))
            .into()
    }

    fn statistics(&self) -> Element<'_, Message> {
        let section = column![row![
            text("Statistics")
                .size(typography::HEADING_SIZE)
                .font(typography::HEADING),
            Space::with_width(Length::Fixed(10.0)),
            text(if self.profiling {
                "reading the column…"
            } else {
                "computed from the samples, not from the cache"
            })
            .size(typography::LABEL_SIZE)
            .style(ui::dim),
        ]
        .align_y(Alignment::Center)]
        .spacing(6);

        let Some(profile) = &self.profile else {
            return section.into();
        };
        let mut section = section;
        let stats = profile.stats();
        let figures = [
            ("Samples read", profile.samples.to_string()),
            ("Finite", stats.count().to_string()),
            ("Missing", stats.non_finite().to_string()),
            ("Min", opt(stats.min())),
            ("Max", opt(stats.max())),
            ("Peak to peak", fmt_num(stats.peak_to_peak())),
            ("Mean", opt(stats.mean())),
            ("RMS", opt(stats.rms())),
            ("Std dev", opt(stats.std_dev())),
            ("Zero crossings", profile.profile.zero_crossings.to_string()),
        ];
        let mut grid = row![].spacing(20);
        for chunk in figures.chunks(4) {
            let mut cell = column![].spacing(2);
            for (label, value) in chunk {
                cell = cell.push(
                    row![
                        container(ui::caption(*label)).width(Length::Fixed(110.0)),
                        text(value.clone())
                            .size(typography::BODY_SIZE)
                            .font(typography::READOUT),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            grid = grid.push(cell);
        }
        section = section.push(grid);

        let (below, above, missing) = profile.histogram().outside();
        section = section
            .push(Space::with_height(Length::Fixed(4.0)))
            .push(
                container(
                    canvas(HistogramView {
                        histogram: profile.histogram(),
                        cache: &self.histogram_cache,
                        format: fmt_num,
                    })
                    .width(Length::Fill)
                    .height(Length::Fixed(140.0)),
                )
                .width(Length::Fill),
            )
            // What fell outside the histogram is the part a reader can be
            // misled by, so it is four facts rather than one sentence — and
            // anything that fell out at all is worth a colour.
            .push(
                row![
                    ui::fact("Bins", profile.histogram().bins()),
                    ui::spec(
                        "Below",
                        below,
                        if below == 0 { text::base } else { ui::warned },
                    ),
                    ui::spec(
                        "Above",
                        above,
                        if above == 0 { text::base } else { ui::warned },
                    ),
                    ui::spec(
                        "Missing",
                        missing,
                        if missing == 0 { text::base } else { ui::warned },
                    ),
                ]
                .spacing(24)
                .wrap(),
            );
        section.into()
    }

    fn value_table(&self) -> Element<'_, Message> {
        let total = self.row_count();
        let section = column![row![
            text("Values")
                .size(typography::HEADING_SIZE)
                .font(typography::HEADING),
            Space::with_width(Length::Fixed(10.0)),
            text(format!(
                "rows {}–{} of {total}",
                if total == 0 { 0 } else { self.page_start + 1 },
                (self.page_start + PAGE).min(total),
            ))
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT)
            .style(ui::dim),
            Space::with_width(Length::Fill),
            // Paging is movement through the same table, not an action on it,
            // so the arrows are quiet and the index box beside them is where
            // the reader actually goes when they know the row they want.
            button(text("◀").size(typography::BODY_SIZE))
                .padding([3, 10])
                .style(button::text)
                .on_press_maybe((self.page_start > 0).then_some(Message::Page(-1))),
            button(text("▶").size(typography::BODY_SIZE))
                .padding([3, 10])
                .style(button::text)
                .on_press_maybe(
                    (self.page_start < self.last_page_start()).then_some(Message::Page(1))
                ),
            text_input("index", &self.jump_draft)
                .on_input(Message::JumpChanged)
                .on_submit(Message::Jump)
                .size(typography::BODY_SIZE)
                .font(typography::READOUT)
                .width(Length::Fixed(90.0)),
            button(
                text("Go")
                    .size(typography::LABEL_SIZE)
                    .font(typography::LABEL),
            )
            .padding([3, 10])
            .style(button::text)
            .on_press(Message::Jump),
        ]
        .spacing(6)
        .align_y(Alignment::Center)]
        .spacing(6);

        let Some(page) = &self.values else {
            return section
                .push(ui::empty(
                    "Reading this page…",
                    "Only the rows on screen are read; the rest stay on disk.",
                ))
                .into();
        };

        let headings: Vec<String> = match &self.detail {
            Some(Detail::Pulses(detail)) => {
                let unit = detail.group.toa_unit.unwrap_or(TimeUnit::Seconds);
                std::iter::once(format!("TOA ({unit})"))
                    .chain(detail.fields.iter().map(|field| field.name.clone()))
                    .collect()
            }
            _ => vec!["Time (s)".to_owned(), "Value".to_owned()],
        };

        let index_width = 90.0;
        let column_width = 130.0;
        // Every column of this table is a number, so every heading is over a
        // number and every heading is flush right with it.
        let mut head = row![ui::heading_aligned("#", index_width, Alignment::End)];
        for heading in &headings {
            head = head.push(ui::heading_aligned(
                heading.clone(),
                column_width,
                Alignment::End,
            ));
        }
        let mut table = column![head, ui::rule()].spacing(0);

        let toa_unit = match &self.detail {
            Some(Detail::Pulses(detail)) => detail.group.toa_unit,
            _ => None,
        };
        for (offset, values) in page.rows.iter().enumerate() {
            let index = page.start + offset as u64;
            let time = page.times.get(offset).copied().unwrap_or(f64::NAN);
            // A pulse table reads in the file's own time unit; a signal reads
            // in seconds, which is the timeline everything else shares.
            let time = toa_unit.map_or(time, |unit| unit.from_seconds(time));
            let mut line = row![ui::value(index, index_width)];
            line = line.push(ui::value(fmt_num(time), column_width));
            for value in values {
                line = line.push(ui::value(fmt_num(*value), column_width));
            }
            table = table.push(line);
        }

        section
            .push(
                container(scrollable(table))
                    .height(Length::Fixed(320.0))
                    .style(ui::panel),
            )
            .into()
    }

    fn side_panel<'a>(&'a self, detail: &'a Detail) -> Element<'a, Message> {
        match detail {
            Detail::Signal(detail) => self.signal_panel(detail),
            Detail::Pulses(detail) => pulse_panel(detail),
        }
    }

    fn signal_panel<'a>(&'a self, detail: &'a SignalDetail) -> Element<'a, Message> {
        let mut panel = column![
            ui::caption("Name"),
            row![
                text_input("name", &self.name_draft)
                    .on_input(Message::NameChanged)
                    .on_submit(Message::Rename)
                    .size(typography::BODY_SIZE),
                button(
                    text("Rename")
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL),
                )
                .padding([5, 10])
                .style(button::text)
                .on_press(Message::Rename),
            ]
            .spacing(6),
            Space::with_height(Length::Fixed(10.0)),
            text("Tags").size(typography::BODY_SIZE).style(ui::dim),
        ]
        .spacing(4);

        if detail.tags.is_empty() {
            panel = panel.push(text("None yet.").size(typography::BODY_SIZE).style(ui::dim));
        } else {
            let mut chips = column![].spacing(3);
            for tag in &detail.tags {
                chips = chips.push(
                    row![
                        container(
                            text(tag.to_uppercase())
                                .size(typography::LABEL_SIZE)
                                .font(typography::LABEL),
                        )
                        .padding([2, 8])
                        .style(|theme: &Theme| container::Style {
                            background: Some(crate::theme::tokens(theme).selection.into()),
                            border: iced::border::rounded(2),
                            ..container::Style::default()
                        }),
                        button(text("×").size(typography::BODY_SIZE))
                            .padding([1, 6])
                            .style(button::text)
                            .on_press(Message::RemoveTag(tag.clone())),
                    ]
                    .spacing(4)
                    .align_y(Alignment::Center),
                );
            }
            panel = panel.push(chips);
        }

        panel = panel.push(
            row![
                text_input("add a tag", &self.tag_draft)
                    .on_input(Message::TagDraftChanged)
                    .on_submit(Message::AddTag)
                    .size(typography::BODY_SIZE),
                button(
                    text("Add")
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL),
                )
                .padding([4, 10])
                .style(button::text)
                .on_press(Message::AddTag),
            ]
            .spacing(6),
        );

        panel = panel
            .push(Space::with_height(Length::Fixed(12.0)))
            .push(ui::caption("Properties"));

        if let Some(error) = &self.prop_error {
            panel = panel.push(text(error).size(typography::LABEL_SIZE).style(text::danger));
        }

        if detail.defs.is_empty() {
            panel = panel.push(ui::empty(
                "No signal properties are declared.",
                "A property is declared once on the Properties screen and then applies to \
                 every signal in the library.",
            ));
        }
        for def in &detail.defs {
            let raw = self.prop_drafts.get(&def.key).cloned().unwrap_or_default();
            let label = match &def.unit {
                Some(unit) => format!("{} ({unit})", def.label),
                None => def.label.clone(),
            };
            let key = def.key.clone();
            panel = panel.push(
                column![
                    ui::caption(label),
                    text_input(def.kind.type_name(), &raw)
                        .on_input(move |value| Message::PropChanged(key.clone(), value))
                        .on_submit(Message::SaveProperties)
                        .size(typography::BODY_SIZE),
                ]
                .spacing(3),
            );
        }
        if !detail.defs.is_empty() {
            panel = panel.push(
                button(text("Save properties").size(typography::BODY_SIZE))
                    .padding([5, 10])
                    .style(button::primary)
                    .on_press(Message::SaveProperties),
            );
        }

        // Values stored under no definition are still shown: an import never
        // discards a column it did not recognise (§6.5).
        let unrecognised = detail.signal.attributes.unrecognised(&detail.defs);
        if !unrecognised.is_empty() {
            panel = panel
                .push(Space::with_height(Length::Fixed(14.0)))
                .push(ui::caption("Unrecognised attributes"));
            for key in unrecognised {
                let value = detail
                    .signal
                    .attributes
                    .get(key)
                    .map(value_text)
                    .unwrap_or_default();
                panel = panel.push(
                    row![
                        container(ui::caption(key.to_owned())).width(Length::Fixed(120.0)),
                        text(value)
                            .size(typography::LABEL_SIZE)
                            .font(typography::READOUT),
                    ]
                    .align_y(Alignment::Center),
                );
            }
        }

        panel.into()
    }
}

fn pulse_panel(detail: &PulseDetail) -> Element<'_, Message> {
    let mut panel = column![
        ui::caption("Fields in this group"),
        text(
            "A pulse field's name and unit come from the source file's header, so they are read \
             only; the group's own properties are edited where the group is."
        )
        .size(typography::LABEL_SIZE)
        .style(ui::dim),
        Space::with_height(Length::Fixed(8.0)),
    ]
    .spacing(4);

    for field in &detail.fields {
        let active = field.ordinal == detail.ordinal;
        panel = panel.push(
            button(
                column![
                    text(&field.name)
                        .size(typography::BODY_SIZE)
                        .font(if active {
                            typography::BODY_STRONG
                        } else {
                            typography::BODY
                        }),
                    // The key, the unit and the stored type are all the file's
                    // own vocabulary, so the line is set as one reading.
                    text(format!(
                        "{}{} · {}",
                        field.key,
                        field
                            .unit
                            .as_ref()
                            .map_or_else(String::new, |unit| format!(" ({unit})")),
                        field.dtype,
                    ))
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
                ]
                .spacing(1),
            )
            .width(Length::Fill)
            .padding([4, 8])
            .style(ui::selectable(active))
            .on_press(Message::Show(Target::PulseField {
                group: detail.group.id,
                ordinal: field.ordinal,
            })),
        );
    }
    panel.into()
}

/// The text the property form starts with for each declared property.
fn property_drafts(detail: &Detail) -> BTreeMap<String, String> {
    let Detail::Signal(detail) = detail else {
        return BTreeMap::new();
    };
    detail
        .defs
        .iter()
        .map(|def| {
            let raw = detail
                .signal
                .attributes
                .get(&def.key)
                .map(value_text)
                .unwrap_or_default();
            (def.key.clone(), raw)
        })
        .collect()
}

fn value_text(value: &PropertyValue) -> String {
    match value {
        PropertyValue::String(s) => s.clone(),
        PropertyValue::Null => String::new(),
        other => other.to_string(),
    }
}

fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), fmt_num)
}

fn load_detail(conn: &sp_store::Connection, target: Target) -> sp_store::Result<Detail> {
    match target {
        Target::Signal(id) => {
            let signal = library::get_signal(conn, id)?;
            let group = library::get_group(conn, signal.group_id)?;
            let train = trains::get_train(conn, group.train_id)?;
            let dataset = library::get_dataset(conn, train.dataset_id)?;
            let blob = library::signal_blob(conn, id)
                .and_then(|blob_id| blob::info(conn, blob_id))
                .ok();
            Ok(Detail::Signal(Box::new(SignalDetail {
                signal,
                group,
                train,
                dataset,
                tags: library::signal_tags(conn, id)?,
                defs: props::list_property_defs(conn, Some(PropScope::Signal))?,
                blob,
            })))
        }
        Target::PulseField { group, ordinal } => {
            let group_row = library::get_group(conn, group)?;
            let train = trains::get_train(conn, group_row.train_id)?;
            let dataset = library::get_dataset(conn, train.dataset_id)?;
            Ok(Detail::Pulses(Box::new(PulseDetail {
                group: group_row,
                train,
                dataset,
                fields: pulses::list_fields(conn, group)?,
                ordinal,
            })))
        }
    }
}

/// One page of the value table, read straight from the column.
fn load_values(
    conn: &sp_store::Connection,
    target: Target,
    start: u64,
) -> sp_store::Result<ValuePage> {
    match target {
        Target::Signal(id) => {
            let signal = library::get_signal(conn, id)?;
            let range = SampleRange::new(start, start + PAGE);
            let buffer = library::read_samples(conn, id, range)?;
            let values = buffer.to_f64();
            let times = (0..values.len())
                .map(|offset| {
                    signal
                        .timebase
                        .time_of(start + offset as u64)
                        .unwrap_or(f64::NAN)
                })
                .collect();
            Ok(ValuePage {
                start,
                times,
                rows: values.into_iter().map(|value| vec![value]).collect(),
            })
        }
        Target::PulseField { group, .. } => {
            let fields = pulses::list_fields(conn, group)?;
            let range = SampleRange::new(start, start + PAGE);
            let times = pulses::read_toa(conn, group, range)?;
            let columns: Vec<Vec<f64>> = fields
                .iter()
                .map(|field| {
                    pulses::read_field(conn, group, field.ordinal, range).map(|b| b.to_f64())
                })
                .collect::<sp_store::Result<_>>()?;
            let rows = (0..times.len())
                .map(|index| {
                    columns
                        .iter()
                        .map(|column| column.get(index).copied().unwrap_or(f64::NAN))
                        .collect()
                })
                .collect();
            Ok(ValuePage { start, times, rows })
        }
    }
}

#[cfg(test)]
mod tests {
    use sp_core::{DType, Domain, Provenance, SampleBuffer, SourceKind, Timebase};
    use sp_store::library::{NewDataset, NewGroup, NewSignal};
    use sp_store::NewTrain;

    use super::*;

    fn library(values: &[f64]) -> (tempfile::TempDir, Store, SignalId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let values = values.to_vec();
        let id = store
            .write(move |conn| {
                let dataset =
                    library::insert_dataset(conn, &NewDataset::new("d", SourceKind::Generated))?;
                let train = trains::insert_train(conn, &NewTrain::new(dataset, 0))?;
                let group = library::insert_group(conn, &NewGroup::new(train, 0, 1))?;
                library::insert_signal(
                    conn,
                    &NewSignal::new(
                        group,
                        0,
                        "wave",
                        Timebase::regular(1_000.0, 0.0),
                        SampleBuffer::from_f64(DType::F64, &values),
                    )
                    .with_domain(Domain::Analog)
                    .with_provenance(Provenance::Generated),
                )
            })
            .unwrap();
        (dir, store, id)
    }

    fn loaded(store: &Store, id: SignalId) -> State {
        let mut state = State {
            target: Some(Target::Signal(id)),
            ..State::default()
        };
        let detail = store
            .read(move |conn| load_detail(conn, Target::Signal(id)))
            .unwrap();
        let _ = state.update(None, Message::Loaded(Target::Signal(id), Ok(detail)));
        state
    }

    #[test]
    fn a_signal_loads_with_its_metadata_and_property_drafts() {
        let (_dir, store, id) = library(&[1.0, 2.0, 3.0]);
        let state = loaded(&store, id);
        let Some(Detail::Signal(detail)) = &state.detail else {
            panic!("a signal target loads a signal detail");
        };
        assert_eq!(detail.signal.name, "wave");
        assert_eq!(state.name_draft, "wave");
        assert_eq!(state.row_count(), 3);
    }

    #[test]
    fn a_stale_load_is_ignored() {
        let (_dir, store, id) = library(&[1.0]);
        let mut state = loaded(&store, id);
        let other = Target::Signal(SignalId::new(id.get() + 1));
        let detail = store
            .read(move |conn| load_detail(conn, Target::Signal(id)))
            .unwrap();
        let _ = state.update(None, Message::Loaded(other, Ok(detail)));
        // Still the signal the screen is actually showing.
        assert!(matches!(state.detail, Some(Detail::Signal(_))));
        assert_eq!(state.target, Some(Target::Signal(id)));
    }

    #[test]
    fn paging_stays_inside_the_column() {
        let values: Vec<f64> = (0..(PAGE * 2 + 10) as usize).map(|i| i as f64).collect();
        let (_dir, store, id) = library(&values);
        let mut state = loaded(&store, id);

        // Back from the first page is a no-op, not a negative index.
        let _ = state.update(None, Message::Page(-1));
        assert_eq!(state.page_start, 0);
        assert_eq!(state.last_page_start(), PAGE * 2);

        // A jump past the end lands on the last page.
        state.jump_draft = "999999".into();
        let _ = state.update(Some(&store), Message::Jump);
        assert_eq!(state.last_page_start(), PAGE * 2);
    }

    #[test]
    fn a_jump_lands_on_the_page_holding_the_index() {
        let values: Vec<f64> = (0..(PAGE * 3) as usize).map(|i| i as f64).collect();
        let (_dir, store, id) = library(&values);
        let state = loaded(&store, id);
        let page = store
            .read(move |conn| load_values(conn, Target::Signal(id), PAGE))
            .unwrap();
        assert_eq!(page.start, PAGE);
        assert_eq!(page.rows.len(), PAGE as usize);
        assert_eq!(page.rows[0][0], PAGE as f64);
        // The time column is the signal's own timeline.
        assert!((page.times[0] - PAGE as f64 / 1_000.0).abs() < 1e-9);
        assert_eq!(state.row_count(), PAGE * 3);
    }

    #[test]
    fn an_edited_property_parses_before_it_is_saved() {
        let (_dir, store, id) = library(&[1.0]);
        store
            .write(|conn| {
                props::insert_property_def(
                    conn,
                    &PropertyDef::new(
                        "prf_hz",
                        PropScope::Signal,
                        sp_core::props::PropKind::FreqHz {
                            min: None,
                            max: None,
                        },
                    ),
                )?;
                Ok(())
            })
            .unwrap();
        let mut state = loaded(&store, id);
        let Some(Detail::Signal(detail)) = state.detail.clone() else {
            panic!("signal detail");
        };

        let _ = state.update(
            None,
            Message::PropChanged("prf_hz".into(), "not a number".into()),
        );
        assert!(state.edited_attributes(&detail).is_err());

        let _ = state.update(None, Message::PropChanged("prf_hz".into(), "1000".into()));
        let attributes = state.edited_attributes(&detail).unwrap();
        assert_eq!(attributes.get_f64("prf_hz"), Some(1_000.0));

        // Clearing the field removes the value rather than storing an empty one.
        let _ = state.update(None, Message::PropChanged("prf_hz".into(), "   ".into()));
        assert!(!state
            .edited_attributes(&detail)
            .unwrap()
            .contains_key("prf_hz"));
    }

    #[test]
    fn tags_are_added_and_removed_through_the_store() {
        let (_dir, store, id) = library(&[1.0]);
        let mut state = loaded(&store, id);
        state.tag_draft = "golden".into();
        let _ = state.update(Some(&store), Message::AddTag);
        // The task runs off-thread in the app; here the call is made directly
        // to prove the round trip the message performs.
        store
            .write(move |conn| library::tag_signal(conn, id, "golden"))
            .unwrap();
        let tags = store
            .read(move |conn| library::signal_tags(conn, id))
            .unwrap();
        assert_eq!(tags, ["golden"]);
        assert!(state.tag_draft.is_empty(), "the draft clears on submit");
    }

    #[test]
    fn an_empty_name_is_refused() {
        let (_dir, store, id) = library(&[1.0]);
        let mut state = loaded(&store, id);
        let _ = state.update(None, Message::NameChanged("   ".into()));
        let _ = state.update(Some(&store), Message::Rename);
        assert!(state.error.is_some());
        let name = store
            .read(move |conn| library::get_signal(conn, id))
            .unwrap()
            .name;
        assert_eq!(name, "wave");
    }
}
