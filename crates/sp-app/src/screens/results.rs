//! The Results screen (`docs/DESIGN.md` §10.2, §10.3, §12.1).
//!
//! Four regions driven by one selection — a run, a group and a stage:
//!
//! * the **run and group list** on the left, with per-group status, wall time
//!   and the selected stage's metrics, which is where a failing case is found
//!   across a large dataset;
//! * the **stage rail** across the top, one chip per stage in pipeline order,
//!   showing what each produced; clicking walks the algorithm, shift-clicking
//!   pins a stage to compare against;
//! * the **scope**, drawing the selected stage's signals with overlay
//!   artifacts on the same time axis, plus the pinned stage and the residual
//!   between them;
//! * the **artifact panes**, one per non-overlay artifact, each drawn by its
//!   own `ViewHint`, plus the metric chart across groups, this group's
//!   assertion outcomes and — when one is chosen — the diff against another
//!   run (§9.7, §10.4, §10.5).
//!
//! The playhead is global: it belongs to the screen, not to a pane, so
//! scrubbing moves the scope cursor and the highlighted table row together.
//! Switching stages keeps the playhead *and* the viewport, so a signal appears
//! to transform in place rather than the view jumping (§10.3).

use std::collections::BTreeMap;
use std::time::Instant;

use iced::widget::{
    button, column, container, pick_list, row, scrollable, slider, text, text_input, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task, Theme};
use sp_core::artifact::{self, ArtifactData, ArtifactRegistry, FieldDiff, ViewHint};
use sp_core::stats::MinMax;
use sp_core::{
    ArtifactSchema, AssertStatus, FieldKind, FieldSpec, GroupId, PipelineId, RunId, RunStatus,
    StageStatus, TimeRange, Tolerances,
};
use sp_engine::compare;
use sp_engine::reduce::{Quality, TraceDescriptor, TraceSnapshot, TraceStyle};
use sp_engine::source::{self, ColumnSource};
use sp_engine::viewport::Amplitude;
use sp_engine::{reduce, Clock, Transport, TransportState, Viewport};
use sp_proc::compare::{self as runcompare, DiffOptions, RunDiff};
use sp_proc::StageRegistry;
use sp_store::regress::{self, AssertionResultRow, BaselineRow};
use sp_store::runs::{self, RunGroupRow, RunSignalRow, RunStageRow, SOURCE_STAGE};
use sp_store::{library, Store};

use crate::jobs;
use crate::typography;
use crate::ui;
use crate::widgets::panes::{self, Pane, Sort};
use crate::widgets::scope::{
    self as canvas_scope, Action, Caches, Layout, OverlayItem, OverlayView, TraceView, PALETTE,
};

/// Fraction of the amplitude span left as air above and below a fit.
const FIT_MARGIN: f64 = 0.08;
/// Signals drawn from one stage at once. A stage with more is still
/// inspectable — the rest are listed, not drawn — and keeps the canvas
/// readable.
const MAX_DRAWN: usize = 6;

/// A run in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunChoice {
    pub id: RunId,
    pub pipeline_id: PipelineId,
    pub pipeline: String,
    pub status: RunStatus,
    pub when: String,
}

impl std::fmt::Display for RunChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{} {} · {}", self.id.get(), self.pipeline, self.when)
    }
}

/// One chip of the stage rail.
#[derive(Debug, Clone, PartialEq)]
pub struct StageChip {
    /// Pipeline ordinal, or [`SOURCE_STAGE`] for the signals as they entered.
    pub ordinal: i32,
    /// The name the user gave the stage, when they gave it one.
    pub label: Option<String>,
    pub kind: String,
}

/// A group in the list, with the outcome the run recorded for it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupEntry {
    pub id: GroupId,
    pub name: String,
    pub outcome: RunGroupRow,
}

/// Everything about a run that does not depend on which group is selected.
#[derive(Debug, Clone, PartialEq)]
pub struct RunDetail {
    pub run: RunId,
    pub stages: Vec<StageChip>,
    pub groups: Vec<GroupEntry>,
    /// Every recorded stage row, by `(group, stage ordinal)`.
    pub stage_rows: BTreeMap<(i64, i32), RunStageRow>,
    /// How each group's assertions turned out (§9.7), by group.
    pub assertions: BTreeMap<i64, Vec<AssertionResultRow>>,
}

impl RunDetail {
    fn stage_row(&self, group: GroupId, stage: i32) -> Option<&RunStageRow> {
        self.stage_rows.get(&(group.get(), stage))
    }

    fn group_assertions(&self, group: GroupId) -> &[AssertionResultRow] {
        self.assertions.get(&group.get()).map_or(&[], Vec::as_slice)
    }

    /// How many of a group's assertions failed, for the group list.
    fn failed_assertions(&self, group: GroupId) -> usize {
        self.group_assertions(group)
            .iter()
            .filter(|row| row.status.is_failure())
            .count()
    }

    /// Every metric name any stage recorded, for the metrics chart (§10.5).
    fn metric_names(&self, stage: i32) -> Vec<String> {
        let mut names: Vec<String> = self
            .stage_rows
            .iter()
            .filter(|((_, ordinal), _)| *ordinal == stage)
            .flat_map(|(_, row)| row.metrics.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// One signal of the selected — or pinned — stage, on the scope.
#[derive(Debug, Clone)]
struct Trace {
    name: String,
    /// The name the canvas shows, which says which side of a comparison the
    /// trace is.
    display: String,
    signal: RunSignalRow,
    role: Role,
    colour_index: usize,
    snapshot: Option<TraceSnapshot>,
}

/// Which of the two compared stages a trace belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Selected,
    Pinned,
    /// The difference between a matched pair (§10.4). Its snapshot is computed
    /// from the other two rather than read.
    Residual,
}

impl Role {
    const fn suffix(self) -> &'static str {
        match self {
            Self::Selected => "",
            Self::Pinned => " (pinned)",
            Self::Residual => " (A−B)",
        }
    }
}

/// One artifact of a stage, decoded and ready to draw.
#[derive(Debug, Clone)]
pub struct LoadedArtifact {
    pub port: String,
    pub kind: String,
    pub summary: Option<String>,
    pub data: ArtifactData,
}

/// What one (group, stage) selection loads.
#[derive(Debug, Clone, Default)]
pub struct StageView {
    pub signals: Vec<RunSignalRow>,
    pub artifacts: Vec<LoadedArtifact>,
}

#[derive(Debug)]
pub struct State {
    artifacts: ArtifactRegistry,
    stages: StageRegistry,

    runs: Vec<RunChoice>,
    run: Option<RunId>,
    detail: Option<RunDetail>,
    group: Option<GroupId>,
    stage: i32,
    pinned: Option<i32>,

    view: StageView,
    pinned_view: StageView,
    traces: Vec<Trace>,
    residuals: Vec<compare::Residual>,

    transport: Transport,
    clock: Clock,
    viewport: Viewport,
    layout: Layout,
    caches: Caches,
    generation: u64,
    /// Set when the next load should fit the window to what it finds — a new
    /// run or group. A stage change leaves it clear, which is what makes the
    /// signal transform in place (§10.3).
    refit: bool,
    loading: bool,
    reducing: bool,
    error: Option<String>,
    status: Option<String>,
    hover_s: Option<f64>,

    /// Per-artifact table ordering, keyed by port.
    sorts: BTreeMap<String, Sort>,
    /// Field-level differences against the pinned stage, keyed by port
    /// (§10.4). Recomputed when either side loads.
    diffs: BTreeMap<String, Vec<FieldDiff>>,
    /// Which metric the across-groups chart shows (§10.5), and that metric
    /// shaped as an artifact so the same viewers draw it.
    metric: Option<String>,
    metric_data: Option<ArtifactData>,

    /// The library's baselines, the name a promotion would use, and the
    /// tolerance it would be stored with, as a percentage (§10.4).
    baselines: Vec<BaselineRow>,
    promote_as: String,
    promote_tolerance: String,
    /// The run this one is being diffed against, and the diff itself.
    against: Option<RunId>,
    diff: Option<RunDiff>,
    diffing: bool,
    /// How hard the reducer works per frame, from Settings.
    quality: Quality,
    /// A run the Runs screen asked for, waiting for the run list to arrive,
    /// and the run it should be diffed against once it has opened.
    pending_open: Option<RunId>,
    pending_against: Option<RunId>,
}

impl Default for State {
    fn default() -> Self {
        let (artifacts, stages) = registries(&[]);
        Self {
            artifacts,
            stages,
            runs: Vec::new(),
            run: None,
            detail: None,
            group: None,
            stage: SOURCE_STAGE,
            pinned: None,
            view: StageView::default(),
            pinned_view: StageView::default(),
            traces: Vec::new(),
            residuals: Vec::new(),
            transport: Transport::default(),
            clock: Clock::new(),
            viewport: Viewport::default(),
            layout: Layout::default(),
            caches: Caches::default(),
            generation: 0,
            refit: true,
            loading: false,
            reducing: false,
            error: None,
            status: None,
            hover_s: None,
            sorts: BTreeMap::new(),
            diffs: BTreeMap::new(),
            metric: None,
            metric_data: None,
            baselines: Vec::new(),
            promote_as: String::new(),
            promote_tolerance: String::new(),
            against: None,
            diff: None,
            diffing: false,
            quality: Quality::default(),
            pending_open: None,
            pending_against: None,
        }
    }
}

/// The registries the screen reads schemas and stage labels out of. A
/// mis-declared built-in is a bug in `sp-dsp`, so it is logged and the screen
/// falls back to showing kinds rather than labels.
fn registries(libraries: &[std::path::PathBuf]) -> (ArtifactRegistry, StageRegistry) {
    let artifacts = sp_dsp::artifact_registry().unwrap_or_else(|error| {
        tracing::error!(%error, "a built-in artifact is mis-declared");
        ArtifactRegistry::new()
    });
    // A library that will not load is reported on the Settings screen; here
    // it only means a recorded external stage shows by kind rather than by
    // label, which is the right degradation for a screen about old runs.
    let (stages, _) = crate::stages::registry(libraries);
    (artifacts, stages)
}

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    Runs(Result<(Vec<RunChoice>, Vec<BaselineRow>), String>),
    SelectRun(RunId),
    Detail(Result<RunDetail, String>),
    SelectGroup(GroupId),
    SelectStage(i32),
    StepStage(i32),
    Pin(i32),
    Unpin,
    Loaded(u64, Result<(StageView, StageView), String>),
    PyramidsBuilt(u64),
    Reduced(u64, Result<Vec<(usize, TraceSnapshot)>, String>),

    Canvas(Action),
    SortBy(String, String),
    MetricSelected(String),

    PromoteAs(String),
    PromoteTolerance(String),
    Promote,
    Promoted(Result<String, String>),
    CompareWith(RunChoice),
    Compared(Result<Box<RunDiff>, String>),
    ClearComparison,

    Play,
    Pause,
    Toggle,
    Stop,
    SeekFraction(f32),
    FitAll,
    LayoutChanged(Layout),
    Tick(Instant),
}

impl State {
    /// Loads the runs the picker offers, and the baselines they can be
    /// promoted to or measured against.
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        self.loading = true;
        Task::perform(
            jobs::read(store.clone(), |conn| {
                Ok((load_runs(conn)?, regress::list_baselines(conn)?))
            }),
            Message::Runs,
        )
    }

    /// Sets how hard the reducer works per frame (Settings, §12.1); the next
    /// reduction uses it.
    pub fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
    }

    /// Adopts the external stage libraries from Settings, so a run that used
    /// one shows its stage by label rather than by kind (§9.9).
    pub fn set_libraries(&mut self, libraries: &[std::path::PathBuf]) {
        let (artifacts, stages) = registries(libraries);
        self.artifacts = artifacts;
        self.stages = stages;
    }

    /// Opens `run`, and diffs it against `against` when one is given — what
    /// the Runs screen hands over so there is one place a diff is drawn
    /// (§10.4, §12.1).
    ///
    /// A run this screen has not listed yet is opened once the list arrives,
    /// so a run created a moment ago on another screen still opens.
    pub fn open_run(
        &mut self,
        store: Option<&Store>,
        run: RunId,
        against: Option<RunId>,
    ) -> Task<Message> {
        self.pending_against = against;
        self.against = None;
        self.diff = None;
        if self.runs.iter().any(|choice| choice.id == run) {
            return self.select_run(store, run);
        }
        self.pending_open = Some(run);
        store.map_or_else(Task::none, |store| self.load(store))
    }

    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.transport.state().is_playing()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        if self.is_playing() {
            iced::window::frames().map(Message::Tick)
        } else {
            Subscription::none()
        }
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Refresh => match store {
                Some(store) => self.load(store),
                None => Task::none(),
            },
            Message::Runs(result) => {
                self.loading = false;
                match result {
                    Ok((runs, baselines)) => {
                        self.error = None;
                        let newest = runs.first().map(|choice| choice.id);
                        self.runs = runs;
                        self.baselines = baselines;
                        // A run asked for by name wins over the newest one.
                        match (self.pending_open.take(), self.run, newest) {
                            (Some(run), _, _) => self.select_run(store, run),
                            // The run just finished is the one worth looking at.
                            (None, None, Some(run)) => self.select_run(store, run),
                            _ => Task::none(),
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "results could not list runs");
                        self.error = Some(error);
                        Task::none()
                    }
                }
            }
            Message::SelectRun(run) => self.select_run(store, run),
            Message::Detail(result) => {
                self.loading = false;
                match result {
                    Ok(detail) => {
                        self.error = None;
                        // A run is opened at its last stage: the output is
                        // what the user came to see, and the rail walks back.
                        self.stage = detail.stages.last().map_or(SOURCE_STAGE, |s| s.ordinal);
                        self.group = detail.groups.first().map(|group| group.id);
                        self.metric = detail.metric_names(self.stage).first().cloned();
                        self.pinned = None;
                        self.detail = Some(detail);
                        self.rebuild_metric();
                        let reload = self.reload(store, true);
                        match self.pending_against.take() {
                            Some(other) => Task::batch([reload, self.compare_with(store, other)]),
                            None => reload,
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "results could not open the run");
                        self.error = Some(error);
                        Task::none()
                    }
                }
            }
            Message::SelectGroup(group) => {
                if self.group == Some(group) {
                    return Task::none();
                }
                self.group = Some(group);
                // A different group is different signals, so the window is
                // fitted to them; the stage rail is what preserves a view.
                self.reload(store, true)
            }
            Message::SelectStage(ordinal) => {
                if self.stage == ordinal {
                    return Task::none();
                }
                self.stage = ordinal;
                self.metric = self
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.metric_names(ordinal).first().cloned());
                self.rebuild_metric();
                self.reload(store, false)
            }
            Message::StepStage(delta) => {
                let Some(detail) = &self.detail else {
                    return Task::none();
                };
                let ordinals: Vec<i32> = detail.stages.iter().map(|chip| chip.ordinal).collect();
                let Some(current) = ordinals.iter().position(|o| *o == self.stage) else {
                    return Task::none();
                };
                let next = (current as i32 + delta).clamp(0, ordinals.len() as i32 - 1) as usize;
                if ordinals[next] == self.stage {
                    return Task::none();
                }
                self.stage = ordinals[next];
                self.metric = self
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.metric_names(self.stage).first().cloned());
                self.rebuild_metric();
                self.reload(store, false)
            }
            Message::Pin(ordinal) => {
                self.pinned =
                    (self.pinned != Some(ordinal) && ordinal != self.stage).then_some(ordinal);
                self.reload(store, false)
            }
            Message::Unpin => {
                self.pinned = None;
                self.reload(store, false)
            }
            Message::Loaded(generation, result) => {
                if generation != self.generation {
                    return Task::none();
                }
                self.loading = false;
                match result {
                    Ok((view, pinned)) => {
                        self.error = None;
                        self.view = view;
                        self.pinned_view = pinned;
                        // Each table opens ordered the way its schema reads.
                        for artifact in &self.view.artifacts {
                            if let Some(sort) = panes::default_sort(artifact.data.schema()) {
                                self.sorts.insert(artifact.port.clone(), sort);
                            }
                        }
                        self.rebuild_diffs();
                        self.rebuild_traces();
                        self.retime();
                        return self.build_pyramids(store);
                    }
                    Err(error) => {
                        tracing::error!(%error, "results could not read the stage output");
                        self.error = Some(error);
                    }
                }
                Task::none()
            }
            Message::PyramidsBuilt(generation) => {
                if generation != self.generation {
                    return Task::none();
                }
                self.invalidate(store)
            }
            Message::Reduced(generation, result) => {
                if generation != self.generation {
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
                        self.compute_residuals();
                        self.status = None;
                    }
                    Err(error) => {
                        tracing::error!(%error, "results could not reduce a trace");
                        self.status = Some(error);
                    }
                }
                self.caches.clear();
                Task::none()
            }

            Message::Canvas(action) => self.on_canvas(store, action),
            Message::SortBy(port, field) => {
                let sort = self
                    .sorts
                    .get(&port)
                    .map_or_else(|| Sort::new(&field), |sort| sort.toggled(&field));
                self.sorts.insert(port, sort);
                Task::none()
            }
            Message::MetricSelected(metric) => {
                self.metric = Some(metric);
                self.rebuild_metric();
                Task::none()
            }

            Message::PromoteAs(name) => {
                self.promote_as = name;
                Task::none()
            }
            Message::PromoteTolerance(text) => {
                self.promote_tolerance = text;
                Task::none()
            }
            Message::Promote => self.promote(store),
            Message::Promoted(Ok(name)) => {
                self.status = Some(format!("Promoted this run to the baseline '{name}'."));
                match store {
                    Some(store) => self.load(store),
                    None => Task::none(),
                }
            }
            Message::Promoted(Err(error)) => {
                self.error = Some(error);
                Task::none()
            }
            Message::CompareWith(choice) => self.compare_with(store, choice.id),
            Message::Compared(result) => {
                self.diffing = false;
                match result {
                    Ok(diff) => {
                        self.error = None;
                        self.diff = Some(*diff);
                    }
                    Err(error) => {
                        self.against = None;
                        self.error = Some(error);
                    }
                }
                Task::none()
            }
            Message::ClearComparison => {
                self.against = None;
                self.diff = None;
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
            Message::Toggle => {
                self.transport.toggle();
                self.clock.reset();
                Task::none()
            }
            Message::Stop => {
                self.transport.stop();
                self.clock.reset();
                self.caches.clear();
                Task::none()
            }
            Message::SeekFraction(fraction) => {
                self.transport.seek_fraction(f64::from(fraction));
                self.clock.reset();
                Task::none()
            }
            Message::FitAll => {
                self.viewport.fit_time(self.content_range());
                self.fit_amplitude();
                self.invalidate(store)
            }
            Message::LayoutChanged(layout) => {
                self.layout = layout;
                self.caches.clear();
                Task::none()
            }
            Message::Tick(now) => {
                let Some(dt) = self.clock.tick(now) else {
                    return Task::none();
                };
                self.transport.advance(dt);
                Task::none()
            }
        }
    }

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
                self.viewport.fit_time(self.content_range());
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

    fn select_run(&mut self, store: Option<&Store>, run: RunId) -> Task<Message> {
        let Some(choice) = self.runs.iter().find(|choice| choice.id == run).cloned() else {
            return Task::none();
        };
        self.run = Some(run);
        self.detail = None;
        self.view = StageView::default();
        self.pinned_view = StageView::default();
        self.traces.clear();
        self.residuals.clear();
        let Some(store) = store.cloned() else {
            return Task::none();
        };
        self.loading = true;
        Task::perform(
            jobs::read(store, move |conn| load_detail(conn, &choice)),
            Message::Detail,
        )
    }

    /// Promotes the open run to a named baseline (§10.4).
    ///
    /// The tolerance box is a percentage because that is how drift is
    /// discussed; left empty it promotes bit-exact, which is what G8 claims
    /// the same pipeline over the same inputs produces.
    fn promote(&mut self, store: Option<&Store>) -> Task<Message> {
        let Some(run) = self.run else {
            return Task::none();
        };
        let name = self.promote_as.trim().to_owned();
        if name.is_empty() {
            self.error = Some("Give the baseline a name first.".into());
            return Task::none();
        }
        let tolerances = match self.tolerances() {
            Ok(tolerances) => tolerances,
            Err(error) => {
                self.error = Some(error);
                return Task::none();
            }
        };
        let Some(store) = store else {
            self.error = Some("No library is open.".into());
            return Task::none();
        };

        self.error = None;
        let reported = name.clone();
        Task::perform(
            jobs::write(store.clone(), move |conn| {
                regress::promote(conn, &name, run, &tolerances)?;
                Ok(name)
            }),
            move |result| Message::Promoted(result.map_err(|error| format!("{reported}: {error}"))),
        )
    }

    /// The tolerances the promote box describes: one percentage, applied to
    /// samples and metrics alike.
    fn tolerances(&self) -> Result<Tolerances, String> {
        let text = self.promote_tolerance.trim();
        if text.is_empty() {
            return Ok(Tolerances::EXACT);
        }
        let percent: f64 = text
            .trim_end_matches('%')
            .trim()
            .parse()
            .map_err(|_| format!("'{text}' is not a percentage"))?;
        if !percent.is_finite() || percent < 0.0 {
            return Err(format!("'{text}' is not a percentage"));
        }
        Ok(Tolerances {
            sample_rel: percent / 100.0,
            metric_rel: percent / 100.0,
            ..Tolerances::EXACT
        })
    }

    /// Diffs the open run against another one (§10.4). The comparison reads
    /// both runs' samples, so it runs off the UI thread like any other job.
    fn compare_with(&mut self, store: Option<&Store>, other: RunId) -> Task<Message> {
        let Some(run) = self.run else {
            return Task::none();
        };
        if other == run {
            self.error = Some("A run does not differ from itself.".into());
            return Task::none();
        }
        let Some(store) = store else {
            self.error = Some("No library is open.".into());
            return Task::none();
        };
        self.against = Some(other);
        self.diff = None;
        self.diffing = true;
        self.error = None;

        // The baseline the other run is promoted to, when it is one, brings
        // its tolerances with it: comparing against a baseline should mean
        // the same thing here as it does in CI.
        let tolerances = self
            .baselines
            .iter()
            .find(|row| row.run_id == other)
            .map_or(Tolerances::EXACT, |row| row.tolerances);
        let store = store.clone();
        Task::perform(
            jobs::blocking(move || {
                runcompare::diff_runs(&store, other, run, &DiffOptions::within(tolerances))
                    .map(Box::new)
                    .map_err(|error| error.to_string())
            }),
            Message::Compared,
        )
    }

    /// Reads the selected — and pinned — stage's output.
    fn reload(&mut self, store: Option<&Store>, refit: bool) -> Task<Message> {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.sorts.clear();
        self.refit |= refit;

        let (Some(store), Some(run), Some(group)) = (store.cloned(), self.run, self.group) else {
            return Task::none();
        };
        let (stage, pinned) = (self.stage, self.pinned);
        let registry = self.artifacts.clone();
        self.loading = true;

        Task::perform(
            jobs::read(store, move |conn| {
                let view = load_stage(conn, &registry, run, group, stage)?;
                let pinned = match pinned {
                    Some(ordinal) => load_stage(conn, &registry, run, group, ordinal)?,
                    None => StageView::default(),
                };
                Ok((view, pinned))
            }),
            move |result| Message::Loaded(generation, result),
        )
    }

    /// Turns the loaded signals into traces, selected first so the palette
    /// keeps a signal's colour when a stage is pinned beside it.
    fn rebuild_traces(&mut self) {
        let mut traces = Vec::new();
        for (index, signal) in self.view.signals.iter().take(MAX_DRAWN).enumerate() {
            traces.push(Trace {
                name: signal.name.clone(),
                display: signal.name.clone(),
                signal: signal.clone(),
                role: Role::Selected,
                colour_index: index,
                snapshot: None,
            });
        }
        for signal in self.pinned_view.signals.iter().take(MAX_DRAWN) {
            let colour_index = traces
                .iter()
                .position(|trace| trace.name == signal.name)
                .unwrap_or(traces.len());
            traces.push(Trace {
                name: signal.name.clone(),
                display: format!("{}{}", signal.name, Role::Pinned.suffix()),
                signal: signal.clone(),
                role: Role::Pinned,
                colour_index,
                snapshot: None,
            });
        }
        // One residual per matched pair: same name, both drawn.
        let pairs: Vec<(String, usize)> = self
            .view
            .signals
            .iter()
            .take(MAX_DRAWN)
            .enumerate()
            .filter(|(_, signal)| {
                self.pinned_view
                    .signals
                    .iter()
                    .take(MAX_DRAWN)
                    .any(|other| other.name == signal.name)
            })
            .map(|(index, signal)| (signal.name.clone(), index))
            .collect();
        for (name, colour_index) in pairs {
            let signal = self
                .view
                .signals
                .iter()
                .find(|signal| signal.name == name)
                .expect("the pair came from this list")
                .clone();
            traces.push(Trace {
                display: format!("{name}{}", Role::Residual.suffix()),
                name,
                signal,
                role: Role::Residual,
                colour_index,
                snapshot: None,
            });
        }
        self.traces = traces;
        self.residuals.clear();
    }

    /// Builds the pyramids the loaded signals need, then re-reduces. Until
    /// they land the traces read raw, which is correct but reads more (§5.4).
    fn build_pyramids(&mut self, store: Option<&Store>) -> Task<Message> {
        let generation = self.generation;
        let Some(store) = store.cloned() else {
            return Task::none();
        };
        let building = store.clone();
        let blobs: Vec<_> = self
            .traces
            .iter()
            .filter(|trace| trace.role != Role::Residual)
            .filter_map(|trace| trace.signal.blob_id)
            .collect();
        let build: Task<Message> = Task::perform(
            jobs::blocking(move || {
                for blob in blobs {
                    if let Err(error) = source::ensure(&building, blob) {
                        tracing::warn!(%error, "pyramid build failed");
                    }
                }
            }),
            move |()| Message::PyramidsBuilt(generation),
        );
        Task::batch([build, self.invalidate(Some(&store))])
    }

    /// Re-reduces every drawn trace against the current viewport.
    fn invalidate(&mut self, store: Option<&Store>) -> Task<Message> {
        self.caches.clear();
        let generation = self.generation;
        let Some(store) = store.cloned() else {
            return Task::none();
        };
        let requests: Vec<(usize, sp_store::BlobId, TraceDescriptor)> = self
            .traces
            .iter()
            .enumerate()
            .filter(|(_, trace)| trace.role != Role::Residual)
            .filter_map(|(index, trace)| {
                Some((index, trace.signal.blob_id?, descriptor_of(&trace.signal)))
            })
            .collect();
        if requests.is_empty() {
            for trace in &mut self.traces {
                trace.snapshot = None;
            }
            return Task::none();
        }

        self.reducing = true;
        let viewport = self.viewport;
        let style = TraceStyle {
            quality: self.quality,
            ..TraceStyle::default()
        };
        Task::perform(
            jobs::read(store, move |conn| {
                let mut out = Vec::with_capacity(requests.len());
                for (index, blob, descriptor) in requests {
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

    /// Differences the matched pairs, from the reductions already in hand
    /// (§10.4).
    fn compute_residuals(&mut self) {
        self.residuals.clear();
        let mut computed: Vec<(usize, TraceSnapshot, compare::Residual)> = Vec::new();
        for (index, trace) in self.traces.iter().enumerate() {
            if trace.role != Role::Residual {
                continue;
            }
            let selected = self
                .traces
                .iter()
                .find(|other| other.role == Role::Selected && other.name == trace.name)
                .and_then(|other| other.snapshot.as_ref());
            let pinned = self
                .traces
                .iter()
                .find(|other| other.role == Role::Pinned && other.name == trace.name)
                .and_then(|other| other.snapshot.as_ref());
            let (Some(a), Some(b)) = (selected, pinned) else {
                continue;
            };
            let residual = compare::residual(a, b, &self.viewport);
            computed.push((index, residual.snapshot.clone(), residual));
        }
        for (index, snapshot, residual) in computed {
            if let Some(trace) = self.traces.get_mut(index) {
                trace.snapshot = Some(snapshot);
            }
            self.residuals.push(residual);
        }
    }

    /// Sets the playable span to what this stage holds, and fits the window
    /// to it when the selection moved to different signals.
    fn retime(&mut self) {
        let range = self.content_range();
        self.transport.set_range(range);
        if std::mem::take(&mut self.refit) && !range.is_empty() {
            self.viewport.fit_time(range);
            self.fit_amplitude();
        }
    }

    fn content_range(&self) -> TimeRange {
        self.traces
            .iter()
            .filter(|trace| trace.role != Role::Residual)
            .filter_map(|trace| time_range_of(&trace.signal))
            .reduce(TimeRange::union)
            .unwrap_or(TimeRange::new(0.0, 0.0))
    }

    fn fit_amplitude(&mut self) {
        let extent = self
            .traces
            .iter()
            .filter(|trace| trace.role != Role::Residual)
            .fold(MinMax::EMPTY, |acc, trace| {
                match trace.signal.stats.min().zip(trace.signal.stats.max()) {
                    Some((min, max)) => acc.merge(MinMax::new(min as f32, max as f32)),
                    None => acc,
                }
            });
        if !extent.is_empty() {
            self.viewport.amplitude =
                Amplitude::around(f64::from(extent.min), f64::from(extent.max), FIT_MARGIN);
        }
    }

    /// The artifacts drawn on the scope rather than in a pane (§10.1).
    ///
    /// Rebuilt per frame so the emphasised row follows the playhead; an
    /// artifact is rows, not samples, so this is cheap.
    fn overlays(&self) -> Vec<OverlayView> {
        self.view
            .artifacts
            .iter()
            .enumerate()
            .filter_map(|(index, artifact)| {
                let ViewHint::Overlay { form } = artifact.data.schema().view else {
                    return None;
                };
                let magnitude = artifact.data.magnitude_field();
                let items = artifact
                    .data
                    .spans()
                    .into_iter()
                    .map(|(row, span)| OverlayItem {
                        span,
                        value: magnitude.and_then(|column| column.number_at(row)),
                        label: artifact.data.label_at(row),
                    })
                    .collect();
                Some(OverlayView {
                    form,
                    colour: PALETTE[index % PALETTE.len()],
                    items,
                    current: artifact.data.row_at(self.transport.playhead_s()),
                })
            })
            .collect()
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let body = column![
            self.rail(),
            ui::rule(),
            self.canvas(),
            self.transport_bar(),
            self.status_line(),
        ]
        .spacing(6)
        .padding(8)
        .width(Length::Fill);

        row![self.sidebar(), body, self.pane_column()]
            .height(Length::Fill)
            .into()
    }

    /// The run picker above the group list (§10.3).
    fn sidebar(&self) -> Element<'_, Message> {
        let mut runs = column![].spacing(2);
        for choice in &self.runs {
            let selected = self.run == Some(choice.id);
            // The pipeline is what the run is; the status and the clock time
            // are what it did. Two registers, not one interpunct-joined line.
            let label = column![
                row![
                    text(&choice.pipeline)
                        .size(typography::BODY_SIZE)
                        .font(if selected {
                            typography::BODY_STRONG
                        } else {
                            typography::BODY
                        }),
                    Space::with_width(Length::Fill),
                    text(choice.status.label())
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL)
                        .style(ui::dim),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                text(&choice.when)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
            ]
            .spacing(1);
            runs = runs.push(
                button(label)
                    .width(Length::Fill)
                    .padding([4.0, 8.0])
                    .style(ui::selectable(selected))
                    .on_press(Message::SelectRun(choice.id)),
            );
        }

        let mut groups = column![].spacing(2);
        if let Some(detail) = &self.detail {
            for entry in &detail.groups {
                let selected = self.group == Some(entry.id);
                let row = detail.stage_row(entry.id, self.stage);
                let metrics = row.map_or_else(String::new, |row| describe_metrics(&row.metrics));
                let status = row.map_or_else(
                    || entry.outcome.status.label().to_owned(),
                    |row| row.status.label().to_owned(),
                );
                let wall = entry
                    .outcome
                    .wall_ms
                    .map_or_else(String::new, |ms| format!(" · {ms} ms"));
                let failed_assertions = detail.failed_assertions(entry.id);
                let label = column![
                    text(&entry.name)
                        .size(typography::BODY_SIZE)
                        .font(if selected {
                            typography::BODY_STRONG
                        } else {
                            typography::BODY
                        }),
                    text(format!("{status}{wall}"))
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT)
                        .style(
                            if row.is_some_and(|row| row.status == StageStatus::Failed) {
                                text::danger
                            } else {
                                ui::dim
                            }
                        ),
                ]
                .spacing(1)
                // A group whose stages all ran but whose assertions failed is
                // still a failing case, and the list has to say so (§9.7).
                .push_maybe((failed_assertions > 0).then(|| {
                    text(format!(
                        "{failed_assertions} assertion{} failed",
                        if failed_assertions == 1 { "" } else { "s" }
                    ))
                    .size(typography::LABEL_SIZE)
                    .style(text::danger)
                }))
                .push_maybe((!metrics.is_empty()).then(|| {
                    text(metrics)
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT)
                        .style(ui::dim)
                }));
                groups = groups.push(
                    button(label)
                        .width(Length::Fill)
                        .padding([4.0, 8.0])
                        .style(ui::selectable(selected))
                        .on_press(Message::SelectGroup(entry.id)),
                );
            }
        }

        let body: Element<'_, Message> = if let Some(error) = &self.error {
            text(error)
                .size(typography::BODY_SIZE)
                .style(text::danger)
                .into()
        } else if self.runs.is_empty() {
            if self.loading {
                ui::empty(
                    "Reading the history…",
                    "Runs are read from the open library.",
                )
            } else {
                ui::empty(
                    "No runs yet.",
                    "Build a pipeline on the Pipeline screen and run it; its stages land here.",
                )
            }
        } else {
            scrollable(column![runs, ui::rule(), groups].spacing(8))
                .height(Length::Fill)
                .into()
        };

        container(
            column![
                row![
                    ui::caption("Runs & groups"),
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
                ui::rule(),
                body,
            ]
            .spacing(6)
            .push_maybe(self.run.map(|_| ui::rule()))
            .push_maybe(self.regression_controls()),
        )
        .padding([10, 10])
        .width(Length::Fixed(250.0))
        .height(Length::Fill)
        .style(ui::panel)
        .into()
    }

    /// Promoting the open run to a baseline, and diffing it against another
    /// run (§10.4). Both belong next to the run list because both are about
    /// the run as a whole rather than about one stage of it.
    fn regression_controls(&self) -> Option<Element<'_, Message>> {
        let run = self.run?;
        let named: Vec<&BaselineRow> = self
            .baselines
            .iter()
            .filter(|row| row.run_id == run)
            .collect();

        let mut panel = column![ui::caption("Regression")].spacing(6);
        if let Some(baseline) = named.first() {
            panel = panel.push(
                text(format!("This run is the baseline '{}'.", baseline.name))
                    .size(typography::LABEL_SIZE)
                    .style(text::success),
            );
        }

        panel = panel
            .push(
                row![
                    text_input("Baseline name", &self.promote_as)
                        .on_input(Message::PromoteAs)
                        .padding(4)
                        .size(typography::BODY_SIZE)
                        .width(Length::Fill),
                    text_input("0%", &self.promote_tolerance)
                        .on_input(Message::PromoteTolerance)
                        .padding(4)
                        .size(typography::BODY_SIZE)
                        .width(Length::Fixed(46.0)),
                ]
                .spacing(4),
            )
            .push(
                button(text("Promote to baseline").size(typography::BODY_SIZE))
                    .padding([3.0, 8.0])
                    .style(button::secondary)
                    .on_press_maybe(
                        (!self.promote_as.trim().is_empty()).then_some(Message::Promote),
                    ),
            );

        let others: Vec<RunChoice> = self
            .runs
            .iter()
            .filter(|choice| choice.id != run)
            .cloned()
            .collect();
        if !others.is_empty() {
            let selected = self
                .against
                .and_then(|id| others.iter().find(|choice| choice.id == id).cloned());
            panel = panel.push(
                pick_list(others, selected, Message::CompareWith)
                    .placeholder("Compare with run…")
                    .text_size(typography::BODY_SIZE)
                    .padding(4)
                    .width(Length::Fill),
            );
        }

        if let Some(status) = &self.status {
            panel = panel.push(text(status).size(typography::LABEL_SIZE).style(ui::dim));
        }
        Some(panel.into())
    }

    /// The stage rail: one chip per stage, in order (§10.2).
    fn rail(&self) -> Element<'_, Message> {
        let Some(detail) = &self.detail else {
            return container(ui::empty(
                "Pick a run on the left.",
                "The rail that appears here is the pipeline it ran, one chip per stage,                  in the order they were applied.",
            ))
            .padding(6)
            .into();
        };

        let mut chips = row![].spacing(6).align_y(Alignment::Center);
        for chip in &detail.stages {
            let selected = self.stage == chip.ordinal;
            // A stage the user did not name is shown by the registry's label
            // rather than by its kind string.
            let name = chip.label.clone().unwrap_or_else(|| {
                self.stages.descriptor(&chip.kind).map_or_else(
                    || chip.kind.clone(),
                    |descriptor| descriptor.label.to_owned(),
                )
            });
            let heading = match chip.ordinal {
                SOURCE_STAGE => "Source".to_owned(),
                ordinal => format!("{} {name}", ordinal + 1),
            };
            let pinned = self.pinned == Some(chip.ordinal);
            let row = self
                .group
                .and_then(|group| detail.stage_row(group, chip.ordinal));
            let produced = self.chip_summary(chip.ordinal, row);
            let label = column![
                text(heading).size(typography::BODY_SIZE).font(if selected {
                    typography::BODY_STRONG
                } else {
                    typography::BODY
                }),
                text(produced)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(
                        if row.is_some_and(|row| row.status == StageStatus::Failed) {
                            text::danger
                        } else {
                            ui::dim
                        },
                    ),
            ]
            .spacing(1);

            // Selected and pinned are two different states and were drawn as
            // two fills of the same shape, which made them look like degrees
            // of one thing. The chip carries the selection; the pin control
            // carries the pin, which is the control that sets it.
            chips = chips.push(
                button(label)
                    .padding([4.0, 8.0])
                    .style(ui::selectable(selected))
                    .on_press(Message::SelectStage(chip.ordinal)),
            );
            chips = chips.push(
                button(
                    text(if pinned { "unpin" } else { "pin" })
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL),
                )
                .padding([2.0, 5.0])
                .style(ui::selectable(pinned))
                .on_press(if pinned {
                    Message::Unpin
                } else {
                    Message::Pin(chip.ordinal)
                }),
            );
        }

        container(
            scrollable(chips).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(4).scroller_width(4),
            )),
        )
        .width(Length::Fill)
        .into()
    }

    /// What a chip says a stage produced (§10.2).
    fn chip_summary(&self, ordinal: i32, row: Option<&RunStageRow>) -> String {
        let counts = if ordinal == self.stage {
            format!(
                "{} sig · {} art",
                self.view.signals.len(),
                self.view.artifacts.len()
            )
        } else if self.pinned == Some(ordinal) {
            format!(
                "{} sig · {} art",
                self.pinned_view.signals.len(),
                self.pinned_view.artifacts.len()
            )
        } else {
            String::new()
        };
        match row {
            Some(row) => {
                let wall = row
                    .wall_ms
                    .map_or_else(String::new, |ms| format!("{ms} ms · "));
                let counts = if counts.is_empty() {
                    String::new()
                } else {
                    format!("{counts} · ")
                };
                format!("{counts}{wall}{}", row.status.label().to_lowercase())
            }
            None if ordinal == SOURCE_STAGE => {
                if counts.is_empty() {
                    "as imported".to_owned()
                } else {
                    counts
                }
            }
            None => "not recorded".to_owned(),
        }
    }

    fn canvas(&self) -> Element<'_, Message> {
        let views: Vec<TraceView<'_>> = self
            .traces
            .iter()
            .map(|trace| TraceView {
                name: &trace.display,
                colour: colour_for(trace),
                snapshot: trace.snapshot.as_ref(),
                logic_threshold: logic_threshold(&trace.signal),
            })
            .collect();

        let program = canvas_scope::Scope {
            traces: views,
            overlays: self.overlays(),
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
            // The plot is the subject of this screen, so it gets a hairline
            // and square corners: a rounded card would frame it as one widget
            // among several rather than as the instrument face.
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

    fn transport_bar(&self) -> Element<'_, Message> {
        let playing = self.is_playing();
        let has_content = !self.transport.range().is_empty();
        let scrub = slider(
            0.0..=1.0,
            self.transport.progress() as f32,
            Message::SeekFraction,
        )
        .step(0.0005_f32)
        .width(Length::Fill);

        container(
            row![
                button(ui::transport_label(playing))
                    .padding([4.0, 12.0])
                    .style(button::primary)
                    .on_press_maybe(has_content.then_some(if playing {
                        Message::Pause
                    } else {
                        Message::Play
                    })),
                button(ui::transport_stop())
                    .padding([4.0, 10.0])
                    .style(button::secondary)
                    .on_press_maybe(has_content.then_some(Message::Stop)),
                // The clock is the one number on this screen that changes
                // continuously, so it is set monospaced: the digits do not
                // shove each other about as it runs.
                text(canvas_scope::format_time(self.transport.playhead_s()))
                    .size(typography::HEADING_SIZE)
                    .font(typography::READOUT),
                scrub,
                button(text("Fit").size(typography::LABEL_SIZE))
                    .padding([3.0, 8.0])
                    .style(button::text)
                    .on_press_maybe(has_content.then_some(Message::FitAll)),
                pick_list(Layout::ALL, Some(self.layout), Message::LayoutChanged)
                    .text_size(typography::BODY_SIZE)
                    .padding(3),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([6, 6])
        .width(Length::Fill)
        .into()
    }

    fn status_line(&self) -> Element<'_, Message> {
        let state = match self.transport.state() {
            TransportState::Playing => format!("Playing ×{:.2}", self.transport.rate()),
            other => other.label().to_owned(),
        };
        let window = format!(
            "Window {} – {}",
            canvas_scope::format_time(self.viewport.time().start_s),
            canvas_scope::format_time(self.viewport.time().end_s),
        );
        let hover = self
            .hover_s
            .map(|t| format!("Cursor {}", canvas_scope::format_time(t)))
            .unwrap_or_default();
        let residual = self
            .residuals
            .first()
            .map(compare::Residual::describe)
            .unwrap_or_default();

        container(
            // Every field on this line is a reading, so every field is
            // monospaced: the line does not reflow as the transport runs.
            row![
                text(state)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT),
                Space::with_width(Length::Fixed(12.0)),
                text(window)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
                Space::with_width(Length::Fixed(12.0)),
                text(hover)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(ui::dim),
                Space::with_width(Length::Fill),
                text(residual)
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT),
                text(if self.reducing { " reducing…" } else { "" })
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
                text(self.status.clone().unwrap_or_default())
                    .size(typography::LABEL_SIZE)
                    .style(text::danger),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        )
        .padding([2, 6])
        .width(Length::Fill)
        .into()
    }

    /// A docked pane per non-overlay artifact, plus the diagnostics and the
    /// across-groups metric chart (§10.3, §10.5).
    fn pane_column(&self) -> Element<'_, Message> {
        let mut list = column![].spacing(8);
        let playhead = self.transport.playhead_s();

        for (index, artifact) in self.view.artifacts.iter().enumerate() {
            let pane = Pane {
                title: &artifact.port,
                summary: artifact.summary.as_deref(),
                data: &artifact.data,
                current_row: artifact.data.row_at(playhead),
                sort: self.sorts.get(&artifact.port),
                diff: self.diffs.get(&artifact.port).map(Vec::as_slice),
                colour: PALETTE[index % PALETTE.len()],
            };
            let port = artifact.port.clone();
            list = list.push(panes::view(&pane, move |field| {
                Message::SortBy(port.clone(), field)
            }));
        }

        if let Some(chart) = self.metric_pane() {
            list = list.push(chart);
        }
        if let Some(assertions) = self.assertions_pane() {
            list = list.push(assertions);
        }
        if let Some(diagnostics) = self.diagnostics_pane() {
            list = list.push(diagnostics);
        }
        if let Some(comparison) = self.comparison_pane() {
            list = list.push(comparison);
        }
        if self.view.artifacts.is_empty() {
            list = list.push(ui::empty(
                "This stage emitted no artifacts.",
                "It still ran — what it produced went downstream as a signal rather than                  as a table or a chart.",
            ));
        }

        container(scrollable(list).height(Length::Fill))
            .padding([10, 10])
            .width(Length::Fixed(330.0))
            .height(Length::Fill)
            .style(ui::panel)
            .into()
    }

    /// Diffs each artifact against the pinned stage's artifact on the same
    /// port and of the same kind (§10.4). A port only one side has is not a
    /// difference to report — the stages simply produce different things.
    fn rebuild_diffs(&mut self) {
        self.diffs.clear();
        for artifact in &self.view.artifacts {
            let Some(other) = self
                .pinned_view
                .artifacts
                .iter()
                .find(|other| other.port == artifact.port && other.kind == artifact.kind)
            else {
                continue;
            };
            self.diffs.insert(
                artifact.port.clone(),
                artifact::diff(&artifact.data, &other.data, 0.0),
            );
        }
    }

    /// The selected stage's metric across every group of the run, shaped as a
    /// series artifact — with a generated impairment ladder, this chart *is*
    /// the algorithm's performance curve (§10.5).
    ///
    /// Building it as an artifact rather than as a bespoke chart means the
    /// same viewer draws it, and the same code path is exercised.
    fn rebuild_metric(&mut self) {
        self.metric_data = None;
        let (Some(detail), Some(metric)) = (self.detail.as_ref(), self.metric.clone()) else {
            return;
        };

        let mut groups = Vec::new();
        let mut values = Vec::new();
        for (index, entry) in detail.groups.iter().enumerate() {
            let Some(row) = detail.stage_row(entry.id, self.stage) else {
                continue;
            };
            let Some(value) = row.metrics.get(&metric) else {
                continue;
            };
            groups.push(index as f64);
            values.push(*value);
        }
        if values.is_empty() {
            return;
        }

        let schema = ArtifactSchema::new(
            vec![
                FieldSpec::new("group", FieldKind::Float),
                FieldSpec::new(metric.clone(), FieldKind::Float),
            ],
            ViewHint::Series {
                x: "group".into(),
                y: vec![metric.as_str().into()],
                x_log: false,
                y_log: false,
            },
        );
        let payload = serde_json::json!({ "group": groups, metric.clone(): values });
        match ArtifactData::from_value(schema, &payload) {
            Ok(data) => self.metric_data = Some(data),
            Err(error) => tracing::warn!(%error, "the metric series would not decode"),
        }
    }

    /// The metric chart plus its picker, when the stage recorded any.
    fn metric_pane(&self) -> Option<Element<'_, Message>> {
        let detail = self.detail.as_ref()?;
        let data = self.metric_data.as_ref()?;
        let names = detail.metric_names(self.stage);
        let current = self.metric.clone()?;

        let mut picker = row![].spacing(4).align_y(Alignment::Center);
        for name in names {
            let selected = name == current;
            picker = picker.push(
                button(
                    text(name.clone())
                        .size(typography::LABEL_SIZE)
                        .font(typography::READOUT),
                )
                .padding([2.0, 6.0])
                .style(ui::selectable(selected))
                .on_press(Message::MetricSelected(name)),
            );
        }

        let pane = Pane {
            title: "Metric across groups",
            summary: None,
            data,
            current_row: None,
            sort: None,
            diff: None,
            colour: PALETTE[2],
        };
        let chart = panes::view(&pane, |_| Message::Refresh);
        Some(column![picker, chart].spacing(4).into())
    }

    /// How this group's assertions turned out (§9.7). Failures come first:
    /// they are what the screen was opened for.
    fn assertions_pane(&self) -> Option<Element<'_, Message>> {
        let (detail, group) = (self.detail.as_ref()?, self.group?);
        let rows = detail.group_assertions(group);
        if rows.is_empty() {
            return None;
        }
        let failed = detail.failed_assertions(group);

        let mut list = column![row![
            ui::caption("Assertions"),
            Space::with_width(Length::Fill),
            text(if failed == 0 {
                format!("{} passed", rows.len())
            } else {
                format!("{failed} of {} failed", rows.len())
            })
            .size(typography::LABEL_SIZE)
            .font(typography::READOUT)
            .style(if failed == 0 {
                text::success
            } else {
                text::danger
            }),
        ]
        .align_y(Alignment::Center)]
        .spacing(4);

        let mut ordered: Vec<&AssertionResultRow> = rows.iter().collect();
        ordered.sort_by_key(|row| (!row.status.is_failure(), row.ordinal));
        for row in ordered {
            let style = match row.status {
                AssertStatus::Pass => text::success,
                AssertStatus::NotApplicable => ui::dim,
                AssertStatus::Fail | AssertStatus::Error => text::danger,
            };
            // The expression is the thing that was asserted, in the form the
            // user wrote it, so it is set as written.
            list = list.push(
                text(row.expression.clone())
                    .size(typography::LABEL_SIZE)
                    .font(typography::READOUT)
                    .style(style),
            );
            if let Some(message) = &row.message {
                list = list.push(
                    text(message.clone())
                        .size(typography::LABEL_SIZE)
                        .style(ui::dim),
                );
            }
        }
        Some(list.into())
    }

    /// The diff against another run, when one has been chosen (§10.4). The
    /// selected group's differences are spelled out; the rest are counted, so
    /// a 500-group dataset does not fill the column.
    fn comparison_pane(&self) -> Option<Element<'_, Message>> {
        self.against?;
        let mut list = column![row![
            ui::caption("Compared with"),
            Space::with_width(Length::Fill),
            button(
                text("Clear")
                    .size(typography::LABEL_SIZE)
                    .font(typography::LABEL)
                    .style(ui::dim),
            )
            .padding([2.0, 6.0])
            .style(button::text)
            .on_press(Message::ClearComparison),
        ]
        .align_y(Alignment::Center)]
        .spacing(4);

        let Some(diff) = &self.diff else {
            return Some(
                list.push(
                    text(if self.diffing {
                        "Comparing…"
                    } else {
                        "No comparison."
                    })
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
                )
                .into(),
            );
        };

        list = list.push(text(diff.describe()).size(typography::LABEL_SIZE).style(
            if diff.is_clean() {
                text::success
            } else {
                text::danger
            },
        ));
        if !diff.same_pipeline {
            // Two runs of different pipelines can differ for a reason that
            // is not a regression, which the reader has to know before they
            // read the deviations below.
            list = list.push(
                text("The two runs ran different pipelines.")
                    .size(typography::LABEL_SIZE)
                    .style(ui::warned),
            );
        }

        let deviations = diff.deviations();
        let here = self.group;
        for (group, reasons) in &deviations {
            if Some(*group) != here {
                continue;
            }
            for reason in reasons {
                list = list.push(
                    text(reason.clone())
                        .size(typography::LABEL_SIZE)
                        .style(text::danger),
                );
            }
        }
        let elsewhere = deviations
            .iter()
            .filter(|(group, _)| Some(*group) != here)
            .count();
        if elsewhere > 0 {
            list = list.push(
                text(format!("{elsewhere} other group(s) deviate"))
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
            );
        } else if !deviations.is_empty() && here.is_some() {
            // Every deviation is in this group, which is worth saying plainly.
            list = list.push(
                text("no other group deviates")
                    .size(typography::LABEL_SIZE)
                    .style(ui::dim),
            );
        }
        Some(list.into())
    }

    /// What the selected stage said about this group (§9.4).
    fn diagnostics_pane(&self) -> Option<Element<'_, Message>> {
        let row = self
            .detail
            .as_ref()
            .zip(self.group)
            .and_then(|(detail, group)| detail.stage_row(group, self.stage))?;
        if row.diagnostics.is_empty() && row.message.is_none() {
            return None;
        }
        let mut list = column![ui::caption("Diagnostics")].spacing(4);
        if let Some(message) = &row.message {
            list = list.push(
                text(message.clone())
                    .size(typography::LABEL_SIZE)
                    .style(text::danger),
            );
        }
        for diagnostic in &row.diagnostics {
            let where_at = diagnostic
                .span
                .map(|span| {
                    format!(
                        " @ {} – {}",
                        canvas_scope::format_time(span.start_s),
                        canvas_scope::format_time(span.end_s)
                    )
                })
                .unwrap_or_default();
            list = list.push(
                text(format!(
                    "{}: {}{where_at}",
                    diagnostic.severity.as_str(),
                    diagnostic.message
                ))
                .size(typography::LABEL_SIZE)
                // A warning was taking the accent, which is the colour the
                // playhead and the selection are spent on; it now takes the
                // theme's warning, which is what it is.
                .style(match diagnostic.severity {
                    sp_core::Severity::Error => text::danger,
                    sp_core::Severity::Warn => ui::warned,
                    sp_core::Severity::Info => ui::dim,
                }),
            );
        }
        Some(container(list).padding(6).width(Length::Fill).into())
    }
}

/// The trace colour: pinned traces are the same hue faded, so a pair reads as
/// a pair, and the residual takes the danger colour of a difference.
fn colour_for(trace: &Trace) -> iced::Color {
    let base = PALETTE[trace.colour_index % PALETTE.len()];
    match trace.role {
        Role::Selected => base,
        Role::Pinned => iced::Color { a: 0.45, ..base },
        Role::Residual => crate::theme::RESIDUAL,
    }
}

fn descriptor_of(signal: &RunSignalRow) -> TraceDescriptor {
    TraceDescriptor {
        timebase: signal.timebase,
        count: signal.sample_count,
        domain: signal.domain,
    }
}

fn time_range_of(signal: &RunSignalRow) -> Option<TimeRange> {
    let rate = signal.timebase.sample_rate_hz?;
    if signal.sample_count == 0 || rate <= 0.0 {
        return None;
    }
    Some(TimeRange::new(
        signal.timebase.t0_s,
        signal.timebase.t0_s + signal.sample_count as f64 / rate,
    ))
}

/// Where a logic trace slices high from low: the middle of its own range.
fn logic_threshold(signal: &RunSignalRow) -> f64 {
    match signal.stats.min().zip(signal.stats.max()) {
        Some((min, max)) => (min + max) / 2.0,
        None => 0.5,
    }
}

fn describe_metrics(metrics: &BTreeMap<String, f64>) -> String {
    metrics
        .iter()
        .take(3)
        .map(|(name, value)| format!("{name} {value:.4}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Every run, newest first, with the pipeline it ran.
fn load_runs(conn: &sp_store::Connection) -> sp_store::Result<Vec<RunChoice>> {
    let mut names: BTreeMap<i64, String> = BTreeMap::new();
    let mut choices = Vec::new();
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
        choices.push(RunChoice {
            id: run.id,
            pipeline_id: run.pipeline_id,
            pipeline,
            status: run.status,
            when: format_timestamp(run.started_utc),
        });
    }
    Ok(choices)
}

/// The stages, groups and stage records of one run.
fn load_detail(conn: &sp_store::Connection, choice: &RunChoice) -> sp_store::Result<RunDetail> {
    let mut stages = vec![StageChip {
        ordinal: SOURCE_STAGE,
        label: Some("Source".to_owned()),
        kind: String::new(),
    }];
    for stage in runs::pipeline_stages(conn, choice.pipeline_id)? {
        stages.push(StageChip {
            ordinal: stage.ordinal as i32,
            label: stage.label.clone(),
            kind: stage.stage_kind,
        });
    }

    let mut groups = Vec::new();
    for outcome in runs::run_groups(conn, choice.id)? {
        let name = library::get_group(conn, outcome.group_id).map_or_else(
            |_| format!("Group {}", outcome.group_id.get()),
            |group| {
                group
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("Group {}", group.ordinal))
            },
        );
        groups.push(GroupEntry {
            id: outcome.group_id,
            name,
            outcome,
        });
    }

    let stage_rows = runs::run_stages(conn, choice.id)?
        .into_iter()
        .map(|row| ((row.group_id.get(), row.stage_ordinal), row))
        .collect();

    let mut assertions: BTreeMap<i64, Vec<AssertionResultRow>> = BTreeMap::new();
    for row in regress::run_assertions(conn, choice.id)? {
        assertions.entry(row.group_id.get()).or_default().push(row);
    }

    Ok(RunDetail {
        run: choice.id,
        stages,
        groups,
        stage_rows,
        assertions,
    })
}

/// One (group, stage) selection: its signals, and its artifacts decoded
/// against whatever schema the registry holds for their kinds.
fn load_stage(
    conn: &sp_store::Connection,
    registry: &ArtifactRegistry,
    run: RunId,
    group: GroupId,
    stage: i32,
) -> sp_store::Result<StageView> {
    let signals = runs::stage_signals(conn, run, group, stage)?;
    let mut artifacts = Vec::new();
    for row in runs::stage_artifacts(conn, run, Some(group), stage)? {
        let payload = runs::artifact_payload(conn, &row)?;
        match registry.decode(&row.kind, &payload) {
            Ok(data) => artifacts.push(LoadedArtifact {
                port: row.port.clone(),
                kind: row.kind.clone(),
                summary: row.summary.clone(),
                data,
            }),
            Err(error) => {
                // A payload that does not match its schema is a bug in the
                // stage that wrote it; the rest of the stage still displays.
                tracing::warn!(%error, kind = %row.kind, "an artifact would not decode");
            }
        }
    }
    Ok(StageView { signals, artifacts })
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
    use sp_core::run::{Diagnostic, Disposition};
    use sp_core::stats::summarise;
    use sp_core::{Artifact, Attributes, DType, Domain, SampleBuffer, SourceKind, Timebase};
    use sp_dsp::artifacts::Detections;
    use sp_proc::pipeline::{Pipeline, PipelineStage};
    use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions};
    use sp_store::library::{NewDataset, NewGroup, NewSignal};
    use sp_store::trains::NewTrain;
    use sp_store::{library, trains};

    use super::*;

    fn group(id: i64) -> GroupId {
        GroupId::new(id)
    }

    fn signal(name: &str, stage: i32, values: &[f64]) -> RunSignalRow {
        let buffer = SampleBuffer::from_f64(DType::F64, values);
        RunSignalRow {
            group_id: group(1),
            stage_ordinal: stage,
            signal_ordinal: 0,
            name: name.to_owned(),
            domain: Domain::Analog,
            disposition: Disposition::Replaced,
            timebase: Timebase::regular(1000.0, 0.0),
            sample_count: values.len() as u64,
            blob_id: None,
            stats: summarise(&buffer),
            attributes: Attributes::new(),
        }
    }

    fn detections(spans: &[(f64, f64)], scores: &[f64]) -> LoadedArtifact {
        let payload = serde_json::json!({
            "spans": spans,
            "peak": scores,
            "signal": vec!["rf"; spans.len()],
        })
        .to_string();
        let registry = sp_dsp::artifact_registry().unwrap();
        LoadedArtifact {
            port: "detections".to_owned(),
            kind: Detections::KIND.to_owned(),
            summary: Some(format!("{} detections", spans.len())),
            data: registry.decode(Detections::KIND, &payload).unwrap(),
        }
    }

    fn detail() -> RunDetail {
        let mut stage_rows = BTreeMap::new();
        for ordinal in 0..2i32 {
            for group_id in 1..=2i64 {
                let mut row = RunStageRow::new(group(group_id), ordinal, StageStatus::Ok);
                row.wall_ms = Some(7);
                row.metrics
                    .insert("snr_db".to_owned(), group_id as f64 * 3.0);
                stage_rows.insert((group_id, ordinal), row);
            }
        }
        RunDetail {
            run: RunId::new(1),
            stages: vec![
                StageChip {
                    ordinal: SOURCE_STAGE,
                    label: Some("Source".to_owned()),
                    kind: String::new(),
                },
                StageChip {
                    ordinal: 0,
                    label: None,
                    kind: "dsp.condition.gain".to_owned(),
                },
                StageChip {
                    ordinal: 1,
                    label: Some("Detect".to_owned()),
                    kind: "dsp.detect.threshold".to_owned(),
                },
            ],
            groups: (1..=2i64)
                .map(|id| GroupEntry {
                    id: group(id),
                    name: format!("dwell {id}"),
                    outcome: RunGroupRow {
                        group_id: group(id),
                        status: RunStatus::Ok,
                        wall_ms: Some(12),
                        message: None,
                    },
                })
                .collect(),
            stage_rows,
            assertions: (1..=2i64)
                .map(|id| {
                    let status = if id == 2 {
                        AssertStatus::Fail
                    } else {
                        AssertStatus::Pass
                    };
                    let mut row =
                        AssertionResultRow::new(group(id), 0, "metrics.snr_db > 4", status)
                            .with_values(Some(id as f64 * 3.0), Some(4.0));
                    if status.is_failure() {
                        row = row.with_message("6 is not > 4");
                    }
                    (id, vec![row])
                })
                .collect(),
        }
    }

    /// A screen with a run open, as it is after `Detail` lands.
    fn opened() -> State {
        let mut state = State {
            runs: vec![RunChoice {
                id: RunId::new(1),
                pipeline_id: PipelineId::new(1),
                pipeline: "conditioning".to_owned(),
                status: RunStatus::Ok,
                when: "2026-09-04 10:00:00Z".to_owned(),
            }],
            run: Some(RunId::new(1)),
            ..State::default()
        };
        let _ = state.update(None, Message::Detail(Ok(detail())));
        state
    }

    #[test]
    fn a_groups_assertion_outcomes_are_counted_and_shown() {
        let mut state = opened();
        let detail = state.detail.as_ref().unwrap();
        assert_eq!(detail.failed_assertions(group(1)), 0);
        assert_eq!(detail.failed_assertions(group(2)), 1);
        assert_eq!(detail.group_assertions(group(2))[0].actual, Some(6.0));

        // The pane exists for a group that has assertions, and not for one
        // whose run recorded none.
        assert!(state.assertions_pane().is_some());
        state.detail.as_mut().unwrap().assertions.clear();
        assert!(state.assertions_pane().is_none());
    }

    #[test]
    fn a_baseline_is_promoted_under_a_name_and_a_tolerance() {
        let mut state = opened();
        // Without a name there is nothing to promote to.
        let _ = state.update(None, Message::Promote);
        assert!(state.error.as_ref().unwrap().contains("name"));

        let _ = state.update(None, Message::PromoteAs("golden".into()));
        assert!(state.tolerances().unwrap().is_exact(), "empty means exact");

        let _ = state.update(None, Message::PromoteTolerance("2.5%".into()));
        let tolerances = state.tolerances().unwrap();
        assert_eq!(tolerances.sample_rel, 0.025);
        assert_eq!(tolerances.metric_rel, 0.025);

        let _ = state.update(None, Message::PromoteTolerance("loose".into()));
        assert!(state.tolerances().is_err());
    }

    #[test]
    fn a_run_cannot_be_compared_with_itself() {
        let mut state = opened();
        let _ = state.update(None, Message::CompareWith(state.runs[0].clone()));
        assert!(state.error.as_ref().unwrap().contains("does not differ"));
        assert_eq!(state.against, None);
    }

    #[test]
    fn opening_a_run_selects_its_last_stage_and_first_group() {
        // The output is what the user came to see; the rail walks back.
        let state = opened();
        assert_eq!(state.stage, 1);
        assert_eq!(state.group, Some(group(1)));
        assert_eq!(state.pinned, None);
    }

    #[test]
    fn the_arrow_keys_walk_the_rail_and_stop_at_its_ends() {
        let mut state = opened();
        let _ = state.update(None, Message::StepStage(-1));
        assert_eq!(state.stage, 0);
        let _ = state.update(None, Message::StepStage(-1));
        assert_eq!(state.stage, SOURCE_STAGE, "the source is the first chip");
        let _ = state.update(None, Message::StepStage(-1));
        assert_eq!(state.stage, SOURCE_STAGE);
        let _ = state.update(None, Message::StepStage(1));
        assert_eq!(state.stage, 0);
    }

    #[test]
    fn a_stage_cannot_be_pinned_against_itself() {
        let mut state = opened();
        let _ = state.update(None, Message::Pin(state.stage));
        assert_eq!(state.pinned, None);
        let _ = state.update(None, Message::Pin(0));
        assert_eq!(state.pinned, Some(0));
        // Pinning the same chip again releases it.
        let _ = state.update(None, Message::Pin(0));
        assert_eq!(state.pinned, None);
    }

    #[test]
    fn a_pinned_stage_gives_every_matched_signal_a_residual_trace() {
        let mut state = opened();
        let view = StageView {
            signals: vec![signal("rf", 1, &[1.0, 2.0]), signal("ref", 1, &[0.0, 0.0])],
            artifacts: Vec::new(),
        };
        let pinned = StageView {
            // Only `rf` exists on both sides, so only `rf` has a residual.
            signals: vec![signal("rf", 0, &[1.0, 1.5])],
            artifacts: Vec::new(),
        };
        let generation = state.generation;
        let _ = state.update(None, Message::Loaded(generation, Ok((view, pinned))));

        let roles: Vec<Role> = state.traces.iter().map(|trace| trace.role).collect();
        assert_eq!(
            roles,
            vec![Role::Selected, Role::Selected, Role::Pinned, Role::Residual]
        );
        let residual = state
            .traces
            .iter()
            .find(|trace| trace.role == Role::Residual)
            .unwrap();
        assert_eq!(residual.name, "rf");
        assert!(residual.display.ends_with("(A−B)"));
    }

    #[test]
    fn stepping_the_rail_keeps_the_window_while_changing_group_refits_it() {
        // Switching stages should transform the signal in place; switching to
        // a different group is a different signal and starts fitted (§10.3).
        let mut state = opened();
        let load = |state: &mut State, values: &[f64]| {
            let view = StageView {
                signals: vec![signal("rf", state.stage, values)],
                artifacts: Vec::new(),
            };
            let generation = state.generation;
            let _ = state.update(
                None,
                Message::Loaded(generation, Ok((view, StageView::default()))),
            );
        };

        load(&mut state, &[0.0; 1000]);
        let fitted = state.viewport.time();
        assert!(fitted.duration_s() > 0.0);
        let _ = state.update(None, Message::Canvas(Action::Seek(0.25)));

        let _ = state.update(None, Message::SelectStage(0));
        load(&mut state, &[0.0; 4000]);
        assert_eq!(state.viewport.time(), fitted, "the stage kept the window");
        assert_eq!(state.transport.playhead_s(), 0.25, "and the playhead");

        let _ = state.update(None, Message::SelectGroup(group(2)));
        load(&mut state, &[0.0; 4000]);
        assert_ne!(state.viewport.time(), fitted, "a new group refits");
    }

    #[test]
    fn a_stale_load_is_dropped_rather_than_drawn() {
        let mut state = opened();
        let stale = state.generation.wrapping_sub(1);
        let view = StageView {
            signals: vec![signal("rf", 1, &[1.0])],
            artifacts: Vec::new(),
        };
        let _ = state.update(
            None,
            Message::Loaded(stale, Ok((view, StageView::default()))),
        );
        assert!(state.traces.is_empty());
    }

    #[test]
    fn an_overlay_artifact_lands_on_the_scope_and_follows_the_playhead() {
        let mut state = opened();
        let view = StageView {
            signals: vec![signal("rf", 1, &[0.0; 4000])],
            artifacts: vec![detections(&[(0.5, 1.0), (2.0, 2.5)], &[0.4, 0.9])],
        };
        let generation = state.generation;
        let _ = state.update(
            None,
            Message::Loaded(generation, Ok((view, StageView::default()))),
        );

        let overlays = state.overlays();
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].items.len(), 2);
        assert_eq!(overlays[0].items[1].span, TimeRange::new(2.0, 2.5));
        assert_eq!(overlays[0].items[0].value, Some(0.4));
        // Nothing is current before the first detection.
        assert_eq!(overlays[0].current, None);

        let _ = state.update(None, Message::Canvas(Action::Seek(2.2)));
        assert_eq!(state.overlays()[0].current, Some(1));
    }

    #[test]
    fn a_pinned_artifact_is_diffed_field_by_field() {
        let mut state = opened();
        let view = StageView {
            signals: Vec::new(),
            artifacts: vec![detections(&[(0.0, 1.0), (2.0, 2.5)], &[0.4, 0.9])],
        };
        let pinned = StageView {
            signals: Vec::new(),
            artifacts: vec![detections(&[(0.0, 1.0), (2.0, 2.5)], &[0.4, 0.5])],
        };
        let generation = state.generation;
        let _ = state.update(None, Message::Loaded(generation, Ok((view, pinned))));

        let diff = state.diffs.get("detections").expect("the ports match");
        let scores = diff.iter().find(|field| field.field == "peak").unwrap();
        assert_eq!(scores.mismatches, 1);
        assert_eq!(scores.first_divergence, Some(1));
        assert!(diff
            .iter()
            .find(|field| field.field == "spans")
            .unwrap()
            .is_equal());
    }

    #[test]
    fn a_table_opens_sorted_the_way_its_schema_reads() {
        let mut state = opened();
        let view = StageView {
            signals: Vec::new(),
            artifacts: vec![detections(&[(0.0, 1.0)], &[0.4])],
        };
        let generation = state.generation;
        let _ = state.update(
            None,
            Message::Loaded(generation, Ok((view, StageView::default()))),
        );
        assert!(state.sorts.contains_key("detections"));

        let field = state.sorts["detections"].field.clone();
        let _ = state.update(None, Message::SortBy("detections".to_owned(), field));
        assert!(
            !state.sorts["detections"].ascending,
            "a second click reverses"
        );
    }

    #[test]
    fn a_stage_metric_becomes_a_series_across_the_groups() {
        // §10.5: with an impairment ladder for groups, this chart is the
        // algorithm's performance curve.
        let state = opened();
        assert_eq!(state.metric.as_deref(), Some("snr_db"));
        let data = state.metric_data.as_ref().expect("the stage recorded one");
        assert_eq!(data.rows(), 2);
        assert_eq!(data.column("snr_db").unwrap().number_at(1), Some(6.0));
        assert_eq!(data.column("group").unwrap().number_at(1), Some(1.0));
    }

    #[test]
    fn a_chip_says_what_its_stage_produced_and_what_it_cost() {
        let state = opened();
        let detail = state.detail.as_ref().unwrap();
        let row = detail.stage_row(group(1), 1);
        assert!(state.chip_summary(1, row).contains("7 ms"));
        assert!(state.chip_summary(1, row).contains("ran"));
        // A stage with no record says so rather than showing a blank chip.
        assert_eq!(state.chip_summary(0, None), "not recorded");
    }

    #[test]
    fn a_failing_group_keeps_its_diagnostics_where_they_can_be_read() {
        let mut state = opened();
        let mut row = RunStageRow::new(group(1), 1, StageStatus::Failed);
        row.diagnostics = vec![Diagnostic::warn("clipped").at(TimeRange::new(1.0, 2.0))];
        row.message = Some("the filter did not converge".to_owned());
        if let Some(detail) = state.detail.as_mut() {
            detail.stage_rows.insert((1, 1), row);
        }
        assert!(state.diagnostics_pane().is_some());
    }

    /// The M6 exit criterion, end to end: run a pipeline, then read any
    /// (group, stage) back the way the screen does (§16).
    #[test]
    fn every_group_and_stage_of_a_real_run_is_inspectable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let groups = store
            .write(|conn| {
                let dataset =
                    library::insert_dataset(conn, &NewDataset::new("runs", SourceKind::Generated))?;
                let train = trains::insert_train(conn, &NewTrain::new(dataset, 0).named("cap"))?;
                let mut groups = Vec::new();
                for ordinal in 0..2u32 {
                    let id = library::insert_group(conn, &NewGroup::new(train, ordinal, 0))?;
                    let values: Vec<f64> = (0..64)
                        .map(|i| f64::from(ordinal) + f64::from(i) * 0.1)
                        .collect();
                    library::insert_signal(
                        conn,
                        &NewSignal::new(
                            id,
                            0,
                            "rf",
                            Timebase::regular(1000.0, 0.0),
                            SampleBuffer::from_f64(DType::F64, &values),
                        ),
                    )?;
                    groups.push(id);
                }
                Ok(groups)
            })
            .unwrap();

        let pipeline = Pipeline::new("conditioning")
            .with_stage(PipelineStage::new("dsp.condition.detrend"))
            .with_stage(PipelineStage::new("dsp.measure.statistics"))
            .with_stage(PipelineStage::new("dsp.detect.threshold").with_param("level", 1.0));
        let rows = pipeline.to_rows();
        let pipeline_id = store
            .write(move |conn| {
                let id = runs::insert_pipeline(conn, &runs::NewPipeline::new("conditioning"))?;
                runs::set_pipeline_stages(conn, id, &rows)?;
                Ok(id)
            })
            .unwrap();
        let registry = sp_dsp::registry().unwrap();
        let summary = run_pipeline(
            &store,
            &registry,
            pipeline_id,
            &pipeline,
            &groups,
            &RunOptions::default(),
            &RunControl::new(),
        )
        .unwrap();
        assert_eq!(summary.status, RunStatus::Ok, "{}", summary.describe());

        let choices = store.read(load_runs).unwrap();
        assert_eq!(choices.len(), 1);
        let choice = choices[0].clone();
        assert_eq!(choice.pipeline, "conditioning");

        let detail = store.read(move |conn| load_detail(conn, &choice)).unwrap();
        assert_eq!(detail.stages.len(), 4, "the source plus three stages");
        assert_eq!(detail.groups.len(), 2);

        let artifacts = sp_dsp::artifact_registry().unwrap();
        for entry in &detail.groups {
            for chip in &detail.stages {
                let (run, group, stage) = (summary.run, entry.id, chip.ordinal);
                let registry = artifacts.clone();
                let view = store
                    .read(move |conn| load_stage(conn, &registry, run, group, stage))
                    .unwrap();
                assert!(
                    !view.signals.is_empty(),
                    "stage {stage} of group {group} recorded its signals"
                );
                assert!(view.signals.iter().all(|signal| signal.blob_id.is_some()));
            }
        }

        // The measuring and detecting stages are the ones with artifacts, and
        // both decode against a registered schema rather than as a tree.
        let registry = artifacts.clone();
        let (run, group) = (summary.run, detail.groups[0].id);
        let measured = store
            .read(move |conn| load_stage(conn, &registry, run, group, 1))
            .unwrap();
        assert_eq!(measured.artifacts.len(), 1);
        assert_eq!(measured.artifacts[0].kind, "statistics.v1");
        assert!(!matches!(
            measured.artifacts[0].data.schema().view,
            ViewHint::Tree
        ));
        assert!(measured.artifacts[0].data.rows() > 0);

        let registry = artifacts.clone();
        let detected = store
            .read(move |conn| load_stage(conn, &registry, run, group, 2))
            .unwrap();
        assert_eq!(detected.artifacts[0].kind, "detections.v1");
        assert!(detected.artifacts[0].data.schema().view.is_overlay());
    }
}
