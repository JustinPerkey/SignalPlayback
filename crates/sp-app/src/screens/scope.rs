//! The Scope screen (`docs/DESIGN.md` §12.1): playback of stored signals.
//!
//! The screen owns the transport, the clock and the viewport; the engine turns
//! a column plus a viewport into at most one `(min, max)` pair per pixel, and
//! the canvas draws that. No sample ever reaches the UI thread, which is what
//! keeps a 100 M-sample signal at 60 fps (G2).
//!
//! Reduction runs off the UI thread against a pooled reader and comes back as
//! a message tagged with the generation it was asked for, so a stale answer to
//! an old viewport is dropped rather than drawn.

use std::collections::HashMap;
use std::time::Instant;

use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, slider, text, text_input,
    Space,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme};
use sp_core::stats::MinMax;
use sp_core::{GroupId, Signal, SignalId, TimeRange};
use sp_engine::reduce::{Quality, TraceDescriptor, TraceSnapshot, TraceStyle};
use sp_engine::source::{self, ColumnSource};
use sp_engine::viewport::Amplitude;
use sp_engine::{
    reduce, Clock, FollowMode, LoopMode, Reduction, Transport, TransportState, Viewport,
};
use sp_store::{library, Store};

use crate::jobs;
use crate::typography;
use crate::ui;
use crate::widgets::scope::{self as canvas_scope, Action, Caches, Layout, TraceView, PALETTE};

/// Traces the scope will hold at once. Past this the palette repeats and the
/// canvas stops being readable; the playlist (M8) is the answer to more.
const MAX_TRACES: usize = 8;
/// Playback rates the transport offers.
const RATES: [f64; 7] = [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0];
/// Fraction of the amplitude span left as air above and below a fit.
const FIT_MARGIN: f64 = 0.08;

/// One signal on the scope.
#[derive(Debug, Clone)]
pub struct Trace {
    pub signal: Signal,
    pub group: String,
    pub visible: bool,
    pub colour_index: usize,
    pub style: TraceStyle,
    /// Set once the pyramid for this column is built; until then the trace
    /// reduces from raw samples, which is correct but reads more.
    pub pyramid_ready: bool,
    pub snapshot: Option<TraceSnapshot>,
}

impl Trace {
    fn descriptor(&self) -> TraceDescriptor {
        TraceDescriptor {
            timebase: self.signal.timebase,
            count: self.signal.sample_count,
            domain: self.signal.domain,
        }
    }

    /// Where a logic trace slices high from low: the middle of its own range,
    /// which is right for both a 0/1 bitstream and a 0–3.3 V capture.
    fn logic_threshold(&self) -> f64 {
        match &self.signal.stats {
            Some(stats) => match (stats.min(), stats.max()) {
                (Some(min), Some(max)) => self.style.apply((min + max) / 2.0),
                _ => 0.5,
            },
            None => 0.5,
        }
    }

    fn time_range(&self) -> Option<TimeRange> {
        self.signal.time_range().map(|range| {
            TimeRange::new(
                range.start_s + self.style.t_offset_s,
                range.end_s + self.style.t_offset_s,
            )
        })
    }
}

/// A signal the picker offers.
///
/// Metadata only: which blob holds the samples is looked up when a signal is
/// put on the scope, not for all ten thousand of them at load (§13).
#[derive(Debug, Clone)]
pub struct Entry {
    pub signal: Signal,
    pub group: String,
}

#[derive(Debug)]
pub struct State {
    entries: Vec<Entry>,
    loading: bool,
    error: Option<String>,
    filter: String,

    traces: Vec<Trace>,
    selected: Option<usize>,

    transport: Transport,
    clock: Clock,
    viewport: Viewport,
    follow: FollowMode,
    layout: Layout,

    caches: Caches,
    /// Bumped whenever the viewport or the trace set changes; a reduction
    /// tagged with an older generation is stale and dropped.
    generation: u64,
    reducing: bool,
    hover_s: Option<f64>,
    status: Option<String>,
    /// The user is typing in the filter, so the space bar is a space rather
    /// than the transport. Cleared by the next interaction with anything else.
    typing: bool,
    /// How hard the reducer works per frame, from Settings.
    quality: Quality,
}

impl Default for State {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            loading: false,
            error: None,
            filter: String::new(),
            traces: Vec::new(),
            selected: None,
            transport: Transport::default(),
            clock: Clock::new(),
            viewport: Viewport::default(),
            follow: FollowMode::default(),
            layout: Layout::default(),
            caches: Caches::default(),
            generation: 0,
            reducing: false,
            hover_s: None,
            status: None,
            typing: false,
            quality: Quality::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Loaded(Result<Vec<Entry>, String>),
    FilterChanged(String),

    Add(SignalId),
    Remove(usize),
    Select(usize),
    ToggleVisible(usize),
    GainChanged(usize, f32),
    OffsetChanged(usize, f32),
    TimeOffsetChanged(usize, f32),
    ResetStyle(usize),
    PyramidBuilt(SignalId, Result<(), String>),

    Canvas(Action),
    Reduced(u64, Result<Vec<(usize, TraceSnapshot)>, String>),

    Play,
    Pause,
    Stop,
    Toggle,
    SeekFraction(f32),
    RateChanged(f64),
    LoopModeChanged(LoopMode),
    FollowChanged(FollowMode),
    LayoutChanged(Layout),
    SetLoopStart,
    SetLoopEnd,
    ClearLoop,
    FitAmplitude,
    FitAll,
    Tick(Instant),
}

impl State {
    /// Sets how hard the reducer works per frame (Settings, §12.1). Existing
    /// traces adopt it on their next reduction, which the caller triggers by
    /// touching the viewport; a fresh trace picks it up when it is added.
    pub fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
        for trace in &mut self.traces {
            trace.style.quality = quality;
        }
    }

    /// Loads the signals the picker offers.
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        self.loading = true;
        Task::perform(jobs::read(store.clone(), load_entries), Message::Loaded)
    }

    /// Whether playback is running, which is what the root subscribes frames
    /// for.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.transport.state().is_playing()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // Ticks are driven by redraw requests, so the engine never runs faster
        // than the display and idles at zero CPU when paused (§11.2).
        if self.is_playing() {
            iced::window::frames().map(Message::Tick)
        } else {
            Subscription::none()
        }
    }

    /// Whether the space bar should reach the transport (§11.4). It should
    /// not while the filter has the user's attention.
    #[must_use]
    pub fn accepts_transport_keys(&self) -> bool {
        !self.typing
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        // Anything the user does other than typing in the filter hands the
        // keyboard back to the transport. Background messages — a tick, a
        // finished job — are not the user doing anything.
        self.typing = match message {
            Message::FilterChanged(_) => true,
            Message::Tick(_)
            | Message::Loaded(_)
            | Message::Reduced(..)
            | Message::PyramidBuilt(..) => self.typing,
            _ => false,
        };

        match message {
            Message::Refresh => match store {
                Some(store) => self.load(store),
                None => Task::none(),
            },
            Message::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(entries) => {
                        self.error = None;
                        self.entries = entries;
                    }
                    Err(error) => {
                        tracing::error!(%error, "scope could not list signals");
                        self.error = Some(error);
                    }
                }
                Task::none()
            }
            Message::FilterChanged(filter) => {
                self.filter = filter;
                Task::none()
            }

            Message::Add(id) => self.add_trace(store, id),
            Message::Remove(index) => {
                if index < self.traces.len() {
                    self.traces.remove(index);
                    self.selected = self.selected.and_then(|selected| match selected {
                        s if s == index => None,
                        s if s > index => Some(s - 1),
                        s => Some(s),
                    });
                    self.retime();
                    return self.invalidate(store);
                }
                Task::none()
            }
            Message::Select(index) => {
                self.selected = (index < self.traces.len()).then_some(index);
                Task::none()
            }
            Message::ToggleVisible(index) => {
                if let Some(trace) = self.traces.get_mut(index) {
                    trace.visible = !trace.visible;
                }
                self.caches.clear();
                Task::none()
            }
            Message::GainChanged(index, gain) => {
                if let Some(trace) = self.traces.get_mut(index) {
                    trace.style.gain = f64::from(gain);
                }
                self.invalidate(store)
            }
            Message::OffsetChanged(index, offset) => {
                if let Some(trace) = self.traces.get_mut(index) {
                    trace.style.offset_v = f64::from(offset);
                }
                self.invalidate(store)
            }
            Message::TimeOffsetChanged(index, offset) => {
                if let Some(trace) = self.traces.get_mut(index) {
                    trace.style.t_offset_s = f64::from(offset);
                }
                self.retime();
                self.invalidate(store)
            }
            Message::ResetStyle(index) => {
                if let Some(trace) = self.traces.get_mut(index) {
                    trace.style = TraceStyle {
                        quality: self.quality,
                        ..TraceStyle::default()
                    };
                }
                self.retime();
                self.invalidate(store)
            }
            Message::PyramidBuilt(id, result) => {
                match result {
                    Ok(()) => {
                        if let Some(trace) = self.traces.iter_mut().find(|t| t.signal.id == id) {
                            trace.pyramid_ready = true;
                        }
                        return self.invalidate(store);
                    }
                    Err(error) => {
                        tracing::warn!(%error, signal = %id, "pyramid build failed");
                        self.status = Some(format!("Pyramid build failed: {error}"));
                    }
                }
                Task::none()
            }

            Message::Canvas(action) => self.on_canvas(store, action),
            Message::Reduced(generation, result) => {
                if generation != self.generation {
                    // An answer to a viewport the user has already left.
                    return Task::none();
                }
                self.reducing = false;
                match result {
                    Ok(snapshots) => {
                        for trace in &mut self.traces {
                            trace.snapshot = None;
                        }
                        for (index, snapshot) in snapshots {
                            if let Some(trace) = self.traces.get_mut(index) {
                                trace.snapshot = Some(snapshot);
                            }
                        }
                        self.status = None;
                    }
                    Err(error) => {
                        tracing::error!(%error, "viewport reduction failed");
                        self.status = Some(error);
                    }
                }
                self.caches.clear();
                Task::none()
            }

            Message::Play => {
                self.transport.play();
                self.clock.reset();
                Task::none()
            }
            Message::Pause => {
                self.transport.pause();
                Task::none()
            }
            Message::Stop => {
                self.transport.stop();
                self.clock.reset();
                Task::none()
            }
            Message::Toggle => {
                self.transport.toggle();
                self.clock.reset();
                Task::none()
            }
            Message::SeekFraction(fraction) => {
                self.transport.seek_fraction(f64::from(fraction));
                self.clock.reset();
                self.follow_playhead(store)
            }
            Message::RateChanged(rate) => {
                self.transport.set_rate(rate);
                Task::none()
            }
            Message::LoopModeChanged(mode) => {
                self.transport.set_loop_mode(mode);
                Task::none()
            }
            Message::FollowChanged(mode) => {
                self.follow = mode;
                self.follow_playhead(store)
            }
            Message::LayoutChanged(layout) => {
                self.layout = layout;
                self.caches.clear();
                Task::none()
            }
            Message::SetLoopStart => {
                self.transport.set_loop_start(self.transport.playhead_s());
                Task::none()
            }
            Message::SetLoopEnd => {
                self.transport.set_loop_end(self.transport.playhead_s());
                Task::none()
            }
            Message::ClearLoop => {
                self.retime();
                Task::none()
            }
            Message::FitAmplitude => {
                self.fit_amplitude();
                self.caches.clear();
                Task::none()
            }
            Message::FitAll => {
                let range = self.content_range();
                self.viewport.fit_time(range);
                self.fit_amplitude();
                self.invalidate(store)
            }
            Message::Tick(now) => {
                let Some(dt) = self.clock.tick(now) else {
                    return Task::none();
                };
                self.transport.advance(dt);
                if self.follow == FollowMode::Scrolling {
                    return self.follow_playhead(store);
                }
                Task::none()
            }
        }
    }

    /// Handles a gesture from the canvas (§11.4).
    fn on_canvas(&mut self, store: Option<&Store>, action: Action) -> Task<Message> {
        match action {
            Action::ZoomTime { at_s, factor } => {
                self.viewport.zoom_time_about(at_s, factor);
                self.invalidate(store)
            }
            Action::Pan(dx) => {
                self.viewport.pan_pixels(dx);
                self.invalidate(store)
            }
            Action::ZoomAmplitude(factor) => {
                self.viewport.zoom_amplitude(factor);
                self.caches.clear();
                Task::none()
            }
            Action::BoxZoom(span) => {
                self.viewport.fit_time(span);
                self.invalidate(store)
            }
            Action::Fit => {
                let range = self.content_range();
                self.viewport.fit_time(range);
                self.fit_amplitude();
                self.invalidate(store)
            }
            Action::Seek(t_s) => {
                self.transport.seek(t_s);
                self.clock.reset();
                Task::none()
            }
            Action::Hover(t_s) => {
                self.hover_s = t_s;
                Task::none()
            }
            Action::Resized(width) => {
                self.viewport.set_width(width);
                self.invalidate(store)
            }
        }
    }

    /// Keeps the window under the playhead in scrolling mode, and reduces
    /// again when it moved (§11.4).
    fn follow_playhead(&mut self, store: Option<&Store>) -> Task<Message> {
        if self.follow != FollowMode::Scrolling {
            return Task::none();
        }
        let before = self.viewport.time();
        self.viewport
            .follow(self.transport.playhead_s(), sp_engine::SCROLL_ANCHOR);
        if self.viewport.time() == before {
            return Task::none();
        }
        self.invalidate(store)
    }

    fn add_trace(&mut self, store: Option<&Store>, id: SignalId) -> Task<Message> {
        if self.traces.len() >= MAX_TRACES {
            self.status = Some(format!("The scope holds {MAX_TRACES} traces at once."));
            return Task::none();
        }
        if self.traces.iter().any(|trace| trace.signal.id == id) {
            return Task::none();
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.signal.id == id)
            .cloned()
        else {
            return Task::none();
        };

        let first = self.traces.is_empty();
        let colour_index = self.traces.len() % PALETTE.len();
        self.traces.push(Trace {
            signal: entry.signal,
            group: entry.group,
            visible: true,
            colour_index,
            style: TraceStyle {
                quality: self.quality,
                ..TraceStyle::default()
            },
            pyramid_ready: false,
            snapshot: None,
        });
        self.selected = Some(self.traces.len() - 1);
        self.retime();
        if first {
            // The first trace decides what the scope is looking at.
            self.viewport.fit_time(self.content_range());
            self.fit_amplitude();
        }

        let build = match store.cloned() {
            // Building the pyramid is what makes the next zoom cheap; it is a
            // background job, and the trace draws from raw samples until it
            // lands (§5.4).
            Some(store) => Task::perform(
                jobs::blocking(move || {
                    let blob = store
                        .read(move |conn| library::signal_blob(conn, id))
                        .map_err(|error| error.to_string())?;
                    source::ensure(&store, blob)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                }),
                move |result| Message::PyramidBuilt(id, result),
            ),
            None => Task::none(),
        };
        Task::batch([build, self.invalidate(store)])
    }

    /// Re-reduces every trace against the current viewport.
    fn invalidate(&mut self, store: Option<&Store>) -> Task<Message> {
        self.caches.clear();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;

        let Some(store) = store.cloned() else {
            return Task::none();
        };
        let requests: Vec<(usize, SignalId, TraceDescriptor, TraceStyle)> = self
            .traces
            .iter()
            .enumerate()
            .filter(|(_, trace)| trace.visible)
            .map(|(index, trace)| (index, trace.signal.id, trace.descriptor(), trace.style))
            .collect();
        if requests.is_empty() {
            for trace in &mut self.traces {
                trace.snapshot = None;
            }
            return Task::none();
        }

        self.reducing = true;
        let viewport = self.viewport;
        Task::perform(
            // The store's job signature is its own error type, so an engine
            // error is carried back as the inner result rather than dressed up
            // as a storage fault.
            jobs::read(store, move |conn| {
                let mut out = Vec::with_capacity(requests.len());
                for (index, signal, descriptor, style) in requests {
                    let blob = library::signal_blob(conn, signal)?;
                    let reduced = ColumnSource::open(conn, blob)
                        .and_then(|source| reduce::trace(&source, descriptor, &viewport, style));
                    match reduced {
                        Ok(snapshot) => out.push((index, snapshot)),
                        Err(error) => return Ok(Err(error.to_string())),
                    }
                }
                Ok(Ok(out))
            }),
            move |result| Message::Reduced(generation, result.and_then(|inner| inner)),
        )
    }

    /// Sets the playable span to the union of what is loaded (§11.3).
    fn retime(&mut self) {
        self.transport.set_range(self.content_range());
    }

    /// The timeline every loaded trace occupies.
    fn content_range(&self) -> TimeRange {
        self.traces
            .iter()
            .filter_map(Trace::time_range)
            .reduce(TimeRange::union)
            .unwrap_or(TimeRange::new(0.0, 0.0))
    }

    /// Fits the amplitude window to the loaded signals' own statistics, which
    /// the library already holds — no samples are read to do it.
    fn fit_amplitude(&mut self) {
        let extent =
            self.traces
                .iter()
                .filter(|trace| trace.visible)
                .fold(MinMax::EMPTY, |acc, trace| {
                    let stats = trace.signal.stats.as_ref();
                    match stats.and_then(|s| s.min().zip(s.max())) {
                        Some((min, max)) => {
                            let (a, b) = (trace.style.apply(min), trace.style.apply(max));
                            acc.merge(MinMax::new(a.min(b) as f32, a.max(b) as f32))
                        }
                        None => acc.merge(
                            trace
                                .snapshot
                                .as_ref()
                                .map_or(MinMax::EMPTY, |s| s.geometry.extent()),
                        ),
                    }
                });
        if !extent.is_empty() {
            self.viewport.amplitude =
                Amplitude::around(f64::from(extent.min), f64::from(extent.max), FIT_MARGIN);
        }
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        row![
            self.picker(),
            column![self.canvas(), self.transport_bar(), self.status_line()]
                .spacing(6)
                .padding(8)
                .width(Length::Fill),
            self.trace_panel(),
        ]
        .height(Length::Fill)
        .into()
    }

    fn canvas(&self) -> Element<'_, Message> {
        let views: Vec<TraceView<'_>> = self
            .traces
            .iter()
            .map(|trace| TraceView {
                name: &trace.signal.name,
                colour: PALETTE[trace.colour_index % PALETTE.len()],
                snapshot: trace.visible.then_some(trace.snapshot.as_ref()).flatten(),
                logic_threshold: trace.logic_threshold(),
            })
            .collect();

        let program = canvas_scope::Scope {
            traces: views,
            // Overlay artifacts belong to a run; the Results screen is where
            // one is loaded (§10.3).
            overlays: Vec::new(),
            viewport: &self.viewport,
            playhead_s: self.transport.playhead_s(),
            loop_range: self.transport.range(),
            layout: self.layout,
            caches: &self.caches,
            show_playhead: !self.traces.is_empty(),
        };

        let canvas: Element<'_, Action> = iced::widget::canvas(program)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        container(canvas.map(Message::Canvas))
            .width(Length::Fill)
            .height(Length::Fill)
            // The plot is the subject of this screen: a hairline and square
            // corners, not a rounded card that would frame it as one widget
            // among several.
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.into()),
                    border: iced::Border {
                        color: crate::theme::tokens(theme).rule,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..container::Style::default()
                }
            })
            .into()
    }

    /// The signal picker: everything in the library, filtered by name.
    fn picker(&self) -> Element<'_, Message> {
        let mut list = column![].spacing(2);
        let needle = self.filter.trim().to_lowercase();
        let mut shown = 0usize;
        for entry in &self.entries {
            let matches = needle.is_empty()
                || entry.signal.name.to_lowercase().contains(&needle)
                || entry.group.to_lowercase().contains(&needle);
            if !matches {
                continue;
            }
            shown += 1;
            let loaded = self.traces.iter().any(|t| t.signal.id == entry.signal.id);
            // A signal already on the scope is a state, not an action, so it
            // takes the same band a selected row takes anywhere else. The
            // group, the domain and the sample count under it are all machine
            // output and are set as one reading.
            let label = column![
                text(&entry.signal.name)
                    .size(typography::BODY_SIZE)
                    .font(if loaded {
                        typography::BODY_STRONG
                    } else {
                        typography::BODY
                    }),
                text(format!(
                    "{} · {} · {}",
                    entry.group,
                    entry.signal.domain.label(),
                    format_count(entry.signal.sample_count),
                ))
                .size(typography::LABEL_SIZE)
                .font(typography::READOUT)
                .style(ui::dim),
            ]
            .spacing(1);
            list = list.push(
                button(label)
                    .width(Length::Fill)
                    .padding([5.0, 8.0])
                    .style(ui::selectable(loaded))
                    .on_press(Message::Add(entry.signal.id)),
            );
        }

        let body: Element<'_, Message> = if self.loading {
            ui::empty(
                "Reading the library…",
                "Only the metadata is read; samples stay on disk.",
            )
        } else if let Some(error) = &self.error {
            text(error)
                .size(typography::BODY_SIZE)
                .style(text::danger)
                .into()
        } else if shown == 0 {
            if self.entries.is_empty() {
                ui::empty(
                    "The library holds no signals yet.",
                    "Import a CSV on the Import screen, or synthesise one on Generate.",
                )
            } else {
                ui::empty(
                    "No signal matches the filter.",
                    "The filter reads both the signal name and the group it came from.",
                )
            }
        } else {
            scrollable(list).height(Length::Fill).into()
        };

        container(
            column![
                row![
                    ui::caption("Signals"),
                    Space::with_width(Length::Fill),
                    button(
                        text("Refresh")
                            .size(typography::LABEL_SIZE)
                            .font(typography::LABEL)
                            .style(ui::dim),
                    )
                    .padding([3.0, 8.0])
                    .style(button::text)
                    .on_press(Message::Refresh),
                ]
                .align_y(Alignment::Center),
                text_input("Filter…", &self.filter)
                    .on_input(Message::FilterChanged)
                    .size(typography::BODY_SIZE)
                    .padding(5),
                ui::rule(),
                body,
            ]
            .spacing(8),
        )
        .padding([10, 10])
        .width(Length::Fixed(250.0))
        .height(Length::Fill)
        .style(ui::panel)
        .into()
    }

    /// Per-trace controls (§11.4): visibility, colour, gain, offset and
    /// alignment.
    fn trace_panel(&self) -> Element<'_, Message> {
        let mut list = column![].spacing(4);
        for (index, trace) in self.traces.iter().enumerate() {
            let selected = self.selected == Some(index);
            let colour = PALETTE[trace.colour_index % PALETTE.len()];
            let header =
                row![
                    container(
                        text("■")
                            .size(typography::BODY_SIZE)
                            .style(move |_: &Theme| text::Style {
                                color: Some(colour)
                            })
                    ),
                    button(text(&trace.signal.name).size(typography::BODY_SIZE).font(
                        if selected {
                            typography::BODY_STRONG
                        } else {
                            typography::BODY
                        }
                    ),)
                    .padding(0)
                    .style(button::text)
                    .on_press(Message::Select(index)),
                    Space::with_width(Length::Fill),
                    checkbox("", trace.visible)
                        .size(14)
                        .on_toggle(move |_| Message::ToggleVisible(index)),
                    button(text("✕").size(typography::LABEL_SIZE))
                        .padding([1.0, 5.0])
                        .style(button::text)
                        .on_press(Message::Remove(index)),
                ]
                .spacing(5)
                .align_y(Alignment::Center);

            let mut entry = column![header].spacing(3);
            entry = entry.push(
                text(format!(
                    "{} · {}",
                    trace.group,
                    describe_reduction(trace.snapshot.as_ref(), trace.pyramid_ready)
                ))
                .size(typography::LABEL_SIZE)
                .font(typography::READOUT)
                .style(ui::dim),
            );
            if selected {
                entry = entry.push(slider_row(
                    "Gain",
                    trace.style.gain as f32,
                    -4.0..=4.0,
                    0.05,
                    move |v| Message::GainChanged(index, v),
                ));
                entry = entry.push(slider_row(
                    "Offset",
                    trace.style.offset_v as f32,
                    -5.0..=5.0,
                    0.05,
                    move |v| Message::OffsetChanged(index, v),
                ));
                let span = self.content_range().duration_s().max(1e-6) as f32;
                entry = entry.push(slider_row(
                    "Align",
                    trace.style.t_offset_s as f32,
                    -span..=span,
                    span / 500.0,
                    move |v| Message::TimeOffsetChanged(index, v),
                ));
                entry = entry.push(
                    button(
                        text("Reset")
                            .size(typography::LABEL_SIZE)
                            .font(typography::LABEL),
                    )
                    .padding([2.0, 8.0])
                    .style(button::text)
                    .on_press(Message::ResetStyle(index)),
                );
            }
            // The open trace is the one whose controls are showing, which is
            // a selection, so it takes the selection band rather than a step
            // of the neutral ladder that reads as an unrelated surface.
            list = list.push(container(entry).padding([6, 8]).width(Length::Fill).style(
                move |theme: &Theme| container::Style {
                    background: selected.then(|| crate::theme::tokens(theme).selection.into()),
                    border: iced::border::rounded(2),
                    ..container::Style::default()
                },
            ));
        }

        let body: Element<'_, Message> = if self.traces.is_empty() {
            ui::empty(
                "No traces yet.",
                "Pick a signal on the left. Each one added gets the next colour of the                  palette and its own gain, offset and alignment.",
            )
        } else {
            scrollable(list).height(Length::Fill).into()
        };

        container(
            column![
                row![
                    ui::caption("Traces"),
                    Space::with_width(Length::Fill),
                    text(format!("{}/{MAX_TRACES}", self.traces.len()))
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT)
                        .style(ui::dim),
                ]
                .align_y(Alignment::Center),
                pick_list(Layout::ALL, Some(self.layout), Message::LayoutChanged)
                    .text_size(typography::BODY_SIZE)
                    .padding(4)
                    .width(Length::Fill),
                ui::rule(),
                body,
            ]
            .spacing(8),
        )
        .padding([10, 10])
        .width(Length::Fixed(260.0))
        .height(Length::Fill)
        .style(ui::panel)
        .into()
    }

    fn transport_bar(&self) -> Element<'_, Message> {
        let playing = self.is_playing();
        let has_content = !self.transport.range().is_empty();

        let play: Element<'_, Message> =
            button(text(if playing { "❚❚ Pause" } else { "▶ Play" }).size(typography::BODY_SIZE))
                .padding([4.0, 12.0])
                .style(button::primary)
                .on_press_maybe(has_content.then_some(if playing {
                    Message::Pause
                } else {
                    Message::Play
                }))
                .into();

        let scrub = slider(
            0.0..=1.0,
            self.transport.progress() as f32,
            Message::SeekFraction,
        )
        .step(0.0005_f32)
        .width(Length::Fill);

        container(
            column![
                row![
                    play,
                    button(text("■ Stop").size(typography::BODY_SIZE))
                        .padding([4.0, 10.0])
                        .style(button::secondary)
                        .on_press_maybe(has_content.then_some(Message::Stop)),
                    Space::with_width(Length::Fixed(8.0)),
                    // The clock is the one number the transport exists to
                    // show, so it is set as one: monospaced, so a digit
                    // ticking over never shifts the ones beside it, and at
                    // display size, because a playhead position read from
                    // across a bench is worth more than the space it costs.
                    text(canvas_scope::format_time(self.transport.playhead_s()))
                        .size(typography::DISPLAY_SIZE)
                        .font(typography::READOUT),
                    Space::with_width(Length::Fixed(8.0)),
                    scrub,
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                row![
                    labelled(
                        "Rate",
                        pick_list(
                            RATES,
                            Some(self.transport.rate().abs()),
                            Message::RateChanged
                        )
                        .text_size(typography::BODY_SIZE)
                        .padding(3)
                        .into(),
                    ),
                    labelled(
                        "Loop",
                        pick_list(
                            LoopMode::ALL,
                            Some(self.transport.loop_mode()),
                            Message::LoopModeChanged,
                        )
                        .text_size(typography::BODY_SIZE)
                        .padding(3)
                        .into(),
                    ),
                    labelled(
                        "Follow",
                        pick_list(FollowMode::ALL, Some(self.follow), Message::FollowChanged)
                            .text_size(typography::BODY_SIZE)
                            .padding(3)
                            .into(),
                    ),
                    Space::with_width(Length::Fill),
                    command("[ In", has_content.then_some(Message::SetLoopStart)),
                    command("] Out", has_content.then_some(Message::SetLoopEnd)),
                    command("Clear loop", has_content.then_some(Message::ClearLoop)),
                    command("Fit", has_content.then_some(Message::FitAll)),
                    command("Fit Y", has_content.then_some(Message::FitAmplitude)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            ]
            .spacing(6),
        )
        .padding([6, 8])
        .width(Length::Fill)
        .into()
    }

    fn status_line(&self) -> Element<'_, Message> {
        let window = format!(
            "Window {} – {}  ({} / px)",
            canvas_scope::format_time(self.viewport.time().start_s),
            canvas_scope::format_time(self.viewport.time().end_s),
            canvas_scope::format_time(self.viewport.seconds_per_pixel()),
        );
        let hover = match self.hover_s {
            Some(t) => format!("Cursor {}", canvas_scope::format_time(t)),
            None => String::new(),
        };
        let state = match self.transport.state() {
            TransportState::Playing => format!("Playing ×{:.2}", self.transport.rate()),
            other => other.label().to_owned(),
        };
        let note = self
            .status
            .clone()
            .or_else(|| {
                self.traces
                    .iter()
                    .find_map(|trace| trace.snapshot.as_ref().and_then(|s| s.note))
                    .map(str::to_owned)
            })
            .unwrap_or_default();

        container(
            // Every field here is a reading, so every field is monospaced and
            // the line does not reflow while the transport runs.
            row![
                text(state)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT),
                Space::with_width(Length::Fixed(16.0)),
                text(window)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
                Space::with_width(Length::Fixed(16.0)),
                text(hover)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
                Space::with_width(Length::Fill),
                text(if self.reducing { "reducing…" } else { "" })
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
                text(note).size(typography::LABEL_SIZE).style(text::danger),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        )
        .padding([2, 8])
        .width(Length::Fill)
        .into()
    }
}

/// The signals the picker offers, newest group last.
fn load_entries(conn: &sp_store::Connection) -> sp_store::Result<Vec<Entry>> {
    let signals = library::list_all_signals(conn)?;
    let mut names: HashMap<GroupId, String> = HashMap::new();
    let mut entries = Vec::with_capacity(signals.len());
    for signal in signals {
        let group = match names.get(&signal.group_id) {
            Some(name) => name.clone(),
            None => {
                let group = library::get_group(conn, signal.group_id)?;
                let name = group
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("Group {}", group.ordinal));
                names.insert(signal.group_id, name.clone());
                name
            }
        };
        entries.push(Entry { signal, group });
    }
    Ok(entries)
}

/// A command that acts on the view rather than on the data: fit, loop, clear.
///
/// These are quiet by design. The transport has exactly one loud control and
/// it is Play; a row of five filled buttons beside it would say all six are
/// the thing to press.
fn command<'a>(label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    button(
        text(label)
            .size(typography::LABEL_SIZE)
            .font(typography::LABEL),
    )
    .padding([3.0, 8.0])
    .style(button::text)
    .on_press_maybe(on_press)
    .into()
}

fn slider_row<'a>(
    label: &'a str,
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    on_change: impl Fn(f32) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        container(ui::caption(label)).width(Length::Fixed(44.0)),
        slider(range, value, on_change)
            .step(step)
            .width(Length::Fill),
        // The number a slider is currently at is a reading, and a reading that
        // changes as it is dragged has to be monospaced or the row twitches.
        container(
            text(format!("{value:.2}"))
                .size(typography::LABEL_SIZE)
                .font(typography::READOUT),
        )
        .width(Length::Fixed(40.0)),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

fn labelled<'a>(label: &'a str, control: Element<'a, Message>) -> Element<'a, Message> {
    row![ui::caption(label), control]
        .spacing(5)
        .align_y(Alignment::Center)
        .into()
}

/// What a trace is currently reading, for the trace panel.
fn describe_reduction(snapshot: Option<&TraceSnapshot>, pyramid_ready: bool) -> String {
    let Some(snapshot) = snapshot else {
        return if pyramid_ready {
            "off screen".to_owned()
        } else {
            "building pyramid…".to_owned()
        };
    };
    match snapshot.reduction {
        Reduction::Raw if snapshot.samples_per_pixel < 1.0 => "raw samples".to_owned(),
        Reduction::Raw => format!("raw · {:.0} samples/px", snapshot.samples_per_pixel),
        Reduction::Level(level) => {
            format!(
                "pyramid L{level} · {:.0} samples/px",
                snapshot.samples_per_pixel
            )
        }
    }
}

fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1} M samples", count as f64 / 1e6)
    } else if count >= 1_000 {
        format!("{:.1} k samples", count as f64 / 1e3)
    } else {
        format!("{count} samples")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::{Domain, Provenance, SignalStats, Timebase};

    fn signal(id: i64, rate: f64, count: u64, domain: Domain) -> Signal {
        Signal {
            id: SignalId::new(id),
            group_id: GroupId::new(1),
            ordinal: 0,
            name: format!("signal {id}"),
            units: None,
            dtype: sp_core::DType::F32,
            domain,
            provenance: Provenance::Generated,
            timebase: Timebase::regular(rate, 0.0),
            sample_count: count,
            stats: Some(SignalStats::from_stored(-1.0, 1.0, 0.0, 0.7, count, 0)),
            attributes: sp_core::Attributes::new(),
        }
    }

    fn entry(id: i64, rate: f64, count: u64) -> Entry {
        Entry {
            signal: signal(id, rate, count, Domain::Analog),
            group: "Group 0".to_owned(),
        }
    }

    fn state_with(entries: Vec<Entry>) -> State {
        State {
            entries,
            ..State::default()
        }
    }

    #[test]
    fn adding_a_signal_fits_the_view_to_it() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        assert_eq!(state.traces.len(), 1);
        // 10 000 samples at 1 kHz is 10 s.
        assert!((state.viewport.duration_s() - 10.0).abs() < 1e-9);
        assert_eq!(state.transport.range(), TimeRange::new(0.0, 10.0));
        // The amplitude fit comes from stored statistics, not from samples.
        assert!(state.viewport.amplitude.min < -1.0);
        assert!(state.viewport.amplitude.max > 1.0);
    }

    #[test]
    fn a_signal_is_only_added_once_and_the_scope_is_bounded() {
        let entries = (1..=10).map(|id| entry(id, 1_000.0, 1_000)).collect();
        let mut state = state_with(entries);
        for id in 1..=10 {
            let _ = state.update(None, Message::Add(SignalId::new(id)));
        }
        assert_eq!(state.traces.len(), MAX_TRACES);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        assert_eq!(state.traces.len(), MAX_TRACES);
    }

    #[test]
    fn the_timeline_is_the_union_of_the_traces() {
        let mut state = state_with(vec![entry(1, 1_000.0, 1_000), entry(2, 100.0, 1_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::Add(SignalId::new(2)));
        // 1 s and 10 s, from the same t0.
        assert_eq!(state.transport.range(), TimeRange::new(0.0, 10.0));

        let _ = state.update(None, Message::Remove(1));
        assert_eq!(state.transport.range(), TimeRange::new(0.0, 1.0));
    }

    #[test]
    fn an_alignment_offset_moves_the_timeline() {
        let mut state = state_with(vec![entry(1, 1_000.0, 1_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::TimeOffsetChanged(0, 2.0));
        assert_eq!(state.transport.range(), TimeRange::new(2.0, 3.0));
    }

    #[test]
    fn playback_advances_only_while_playing() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let start = Instant::now();

        // A tick while stopped moves nothing.
        let _ = state.update(None, Message::Tick(start));
        let _ = state.update(
            None,
            Message::Tick(start + std::time::Duration::from_millis(16)),
        );
        assert_eq!(state.transport.playhead_s(), 0.0);

        let _ = state.update(None, Message::Play);
        let _ = state.update(None, Message::Tick(start));
        let _ = state.update(
            None,
            Message::Tick(start + std::time::Duration::from_millis(100)),
        );
        assert!((state.transport.playhead_s() - 0.1).abs() < 1e-9);
        assert!(state.is_playing());
    }

    #[test]
    fn a_stall_does_not_teleport_the_playhead() {
        let mut state = state_with(vec![entry(1, 1_000.0, 100_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::Play);
        let start = Instant::now();
        let _ = state.update(None, Message::Tick(start));
        let _ = state.update(
            None,
            Message::Tick(start + std::time::Duration::from_secs(5)),
        );
        assert_eq!(state.transport.playhead_s(), 0.0);
    }

    #[test]
    fn scrolling_mode_pins_the_playhead_and_moves_the_window() {
        let mut state = state_with(vec![entry(1, 1_000.0, 100_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(
            None,
            Message::Canvas(Action::BoxZoom(TimeRange::new(0.0, 1.0))),
        );
        let _ = state.update(None, Message::FollowChanged(FollowMode::Scrolling));
        let _ = state.update(None, Message::Play);

        let start = Instant::now();
        let _ = state.update(None, Message::Tick(start));
        let _ = state.update(
            None,
            Message::Tick(start + std::time::Duration::from_millis(200)),
        );
        let x = state.viewport.x_of(state.transport.playhead_s());
        assert!(
            (x - state.viewport.width_px() * 0.8).abs() < 1.0,
            "playhead sat at {x} of {}",
            state.viewport.width_px()
        );
    }

    #[test]
    fn canvas_gestures_move_the_viewport() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let before = state.viewport.duration_s();

        let _ = state.update(
            None,
            Message::Canvas(Action::ZoomTime {
                at_s: 5.0,
                factor: 0.5,
            }),
        );
        assert!((state.viewport.duration_s() - before / 2.0).abs() < 1e-9);

        let start = state.viewport.time().start_s;
        let _ = state.update(None, Message::Canvas(Action::Pan(100.0)));
        assert!(state.viewport.time().start_s > start);

        let _ = state.update(None, Message::Canvas(Action::Fit));
        assert!((state.viewport.duration_s() - before).abs() < 1e-9);
    }

    #[test]
    fn a_click_seeks_without_starting_playback() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::Canvas(Action::Seek(4.0)));
        assert!((state.transport.playhead_s() - 4.0).abs() < 1e-9);
        assert_eq!(state.transport.state(), TransportState::Stopped);
    }

    #[test]
    fn loop_points_come_from_the_playhead() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::Canvas(Action::Seek(2.0)));
        let _ = state.update(None, Message::SetLoopStart);
        let _ = state.update(None, Message::Canvas(Action::Seek(6.0)));
        let _ = state.update(None, Message::SetLoopEnd);
        assert_eq!(state.transport.range(), TimeRange::new(2.0, 6.0));

        let _ = state.update(None, Message::ClearLoop);
        assert_eq!(state.transport.range(), TimeRange::new(0.0, 10.0));
    }

    #[test]
    fn a_stale_reduction_is_dropped() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let stale = state.generation.wrapping_sub(1);
        let _ = state.update(None, Message::Reduced(stale, Err("boom".to_owned())));
        assert!(state.status.is_none());

        let current = state.generation;
        let _ = state.update(None, Message::Reduced(current, Err("boom".to_owned())));
        assert_eq!(state.status.as_deref(), Some("boom"));
    }

    #[test]
    fn hiding_a_trace_keeps_it_loaded() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::ToggleVisible(0));
        assert!(!state.traces[0].visible);
        assert_eq!(state.traces.len(), 1);
    }

    #[test]
    fn the_reduction_description_says_what_was_read() {
        assert_eq!(describe_reduction(None, false), "building pyramid…");
        assert_eq!(describe_reduction(None, true), "off screen");
        let snapshot = TraceSnapshot {
            geometry: sp_engine::TraceGeometry::Points(Vec::new()),
            form: sp_engine::TraceForm::Analog,
            reduction: Reduction::Level(3),
            samples_per_pixel: 900.0,
            note: None,
        };
        assert_eq!(
            describe_reduction(Some(&snapshot), true),
            "pyramid L3 · 900 samples/px"
        );
    }

    #[test]
    fn the_screen_builds_a_view_in_every_state_it_can_be_in() {
        // Empty, loading, failed, populated, playing, stacked and selected:
        // the view must not panic in any of them.
        let mut state = State::default();
        let _ = state.view();
        state.loading = true;
        let _ = state.view();
        state.loading = false;
        state.error = Some("no library".to_owned());
        let _ = state.view();

        let mut state = state_with(vec![entry(1, 1_000.0, 10_000), entry(2, 48_000.0, 96_000)]);
        let _ = state.update(None, Message::Add(SignalId::new(1)));
        let _ = state.update(None, Message::Add(SignalId::new(2)));
        let _ = state.update(None, Message::Select(1));
        let _ = state.view();
        let _ = state.update(None, Message::LayoutChanged(Layout::Stacked));
        let _ = state.update(None, Message::Play);
        let _ = state.update(None, Message::FilterChanged("tone".to_owned()));
        let _ = state.view();
        let _ = state.update(None, Message::ToggleVisible(0));
        let _ = state.view();
    }

    #[test]
    fn typing_in_the_filter_takes_the_space_bar_back_from_the_transport() {
        let mut state = state_with(vec![entry(1, 1_000.0, 10_000)]);
        assert!(state.accepts_transport_keys());
        let _ = state.update(None, Message::FilterChanged("to".to_owned()));
        assert!(!state.accepts_transport_keys());
        // A tick is not the user doing anything, so it does not steal focus back.
        let _ = state.update(None, Message::Tick(Instant::now()));
        assert!(!state.accepts_transport_keys());
        // Touching the scope does.
        let _ = state.update(None, Message::Canvas(Action::Hover(Some(1.0))));
        assert!(state.accepts_transport_keys());
    }

    #[test]
    fn sample_counts_read_at_a_glance() {
        assert_eq!(format_count(500), "500 samples");
        assert_eq!(format_count(12_500), "12.5 k samples");
        assert_eq!(format_count(100_000_000), "100.0 M samples");
    }
}
