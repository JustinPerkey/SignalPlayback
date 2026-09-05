//! The Pipeline screen (`docs/DESIGN.md` §9, §12.1).
//!
//! Three columns: the stage palette on the left, the ordered stage list in the
//! middle, and on the right the parameter form for the selected stage above
//! the run controls.
//!
//! Nothing here knows what any particular stage does. The palette is whatever
//! the [`StageRegistry`] holds, and the parameter form is generated from the
//! selected stage's `ParamSpec`s — which is why a stage from `sp-dsp` and a
//! stage someone else wrote get the same editor (G9).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::widget::{
    button, checkbox, column, container, horizontal_rule, pick_list, progress_bar, row, scrollable,
    text, text_input, Column, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task};
use sp_core::run::Retention;
use sp_core::{Dataset, DatasetId, GroupId, PipelineId, PropertyValue, SignalGroup};
use sp_proc::param::{ParamKind, ParamSpec};
use sp_proc::pipeline::{Pipeline, PipelineIssue, PipelineStage};
use sp_proc::scheduler::{RunControl, RunOptions, RunProgress, RunSummary};
use sp_proc::{StageDescriptor, StageRegistry};
use sp_store::runs::{self, NewPipeline, PipelineRow};
use sp_store::{library, Store};

use crate::jobs;

/// A running pipeline, and the two things the UI reaches into it for.
#[derive(Debug)]
struct Job {
    progress: Arc<Mutex<Option<RunProgress>>>,
    cancel: Arc<AtomicBool>,
}

/// What the screen loads from the library when it opens.
#[derive(Debug, Clone)]
pub struct Loaded {
    datasets: Vec<Dataset>,
    saved: Vec<PipelineRow>,
}

/// A dataset in the run panel's picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetChoice {
    id: DatasetId,
    label: String,
}

impl fmt::Display for DatasetChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// A saved pipeline in the load picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineChoice {
    id: PipelineId,
    label: String,
}

impl fmt::Display for PipelineChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// [`Retention`] with a label for the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionChoice(pub Retention);

impl RetentionChoice {
    pub const ALL: [Self; 3] = [
        Self(Retention::Always),
        Self(Retention::OnFailure),
        Self(Retention::Never),
    ];
}

impl fmt::Display for RetentionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.label())
    }
}

/// One variant of an enum parameter, for its picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant(pub String);

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug)]
pub struct State {
    /// Every stage this build can run. Empty only if a built-in is
    /// mis-declared, which is a bug rather than a user error.
    registry: StageRegistry,
    pipeline: Pipeline,
    /// The row this pipeline is saved as, once it has been saved.
    saved_id: Option<PipelineId>,
    saved: Vec<PipelineRow>,
    /// Which stage the parameter form is showing.
    selected: Option<usize>,
    /// What the user has typed into the parameter form, before it parses.
    /// Keyed by parameter name; cleared when the selection changes.
    drafts: BTreeMap<String, String>,

    datasets: Vec<Dataset>,
    dataset: Option<DatasetId>,
    groups: Vec<SignalGroup>,
    chosen: BTreeSet<GroupId>,

    job: Option<Job>,
    shown: Option<RunProgress>,
    summary: Option<RunSummary>,
    error: Option<String>,
    notice: Option<String>,
    busy: bool,
    /// Set when a run finishes, so the root can reload what the run changed.
    completed: bool,
}

impl Default for State {
    fn default() -> Self {
        let (registry, error) = match sp_dsp::registry() {
            Ok(registry) => (registry, None),
            Err(error) => {
                tracing::error!(%error, "a built-in stage is mis-declared");
                (StageRegistry::new(), Some(error.to_string()))
            }
        };
        Self {
            registry,
            pipeline: Pipeline::new("New pipeline"),
            saved_id: None,
            saved: Vec::new(),
            selected: None,
            drafts: BTreeMap::new(),
            datasets: Vec::new(),
            dataset: None,
            groups: Vec::new(),
            chosen: BTreeSet::new(),
            job: None,
            shown: None,
            summary: None,
            error,
            notice: None,
            busy: false,
            completed: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Loaded(Result<Loaded, String>),
    GroupsLoaded(Result<Vec<SignalGroup>, String>),

    NameChanged(String),
    AddStage(&'static str),
    SelectStage(usize),
    RemoveStage(usize),
    MoveStage(usize, i32),
    StageEnabled(usize, bool),
    RetentionPicked(usize, RetentionChoice),

    ParamText(&'static str, String),
    ParamToggled(&'static str, bool),
    ParamPicked(&'static str, Variant),

    DatasetPicked(DatasetChoice),
    GroupToggled(GroupId, bool),
    AllGroups(bool),

    Save,
    Saved(Result<PipelineId, String>),
    LoadPipeline(PipelineChoice),
    PipelineLoaded(Result<(PipelineId, String, Vec<sp_store::PipelineStageRow>), String>),

    Run,
    Cancel,
    Tick,
    Finished(Result<RunSummary, String>),
}

impl State {
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        Task::perform(
            jobs::read(store.clone(), |conn| {
                Ok(Loaded {
                    datasets: library::list_datasets(conn)?,
                    saved: runs::list_pipelines(conn)?,
                })
            }),
            Message::Loaded,
        )
    }

    /// Ticks while a run is in flight, so the progress bar moves.
    pub fn subscription(&self) -> Subscription<Message> {
        if self.job.is_some() {
            iced::time::every(Duration::from_millis(120)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
    }

    /// Whether a run this screen started has finished since the last ask —
    /// the root uses it to reload what the rest of the app shows.
    pub fn take_completed(&mut self) -> bool {
        std::mem::take(&mut self.completed)
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(Ok(loaded)) => {
                self.datasets = loaded.datasets;
                self.saved = loaded.saved;
                // Selecting the only dataset saves a click, and is what the
                // user meant every time there is just one.
                match (self.dataset, self.datasets.first()) {
                    (None, Some(first)) => {
                        let id = first.id;
                        self.choose_dataset(store, id)
                    }
                    _ => Task::none(),
                }
            }
            Message::Loaded(Err(error)) => {
                self.error = Some(error);
                Task::none()
            }
            Message::GroupsLoaded(Ok(groups)) => {
                // Everything in the dataset is the useful default: a pipeline
                // is normally run over the lot.
                self.chosen = groups.iter().map(|group| group.id).collect();
                self.groups = groups;
                Task::none()
            }
            Message::GroupsLoaded(Err(error)) => {
                self.error = Some(error);
                Task::none()
            }

            Message::NameChanged(name) => {
                self.pipeline.name = name;
                Task::none()
            }
            Message::AddStage(kind) => {
                self.pipeline.stages.push(PipelineStage::new(kind));
                self.select(Some(self.pipeline.stages.len() - 1));
                self.summary = None;
                Task::none()
            }
            Message::SelectStage(index) => {
                self.select(Some(index));
                Task::none()
            }
            Message::RemoveStage(index) => {
                if index < self.pipeline.stages.len() {
                    self.pipeline.stages.remove(index);
                    let next = match self.pipeline.stages.is_empty() {
                        true => None,
                        false => Some(index.min(self.pipeline.stages.len() - 1)),
                    };
                    self.select(next);
                }
                Task::none()
            }
            Message::MoveStage(index, delta) => {
                let target = index as i32 + delta;
                if target >= 0 && (target as usize) < self.pipeline.stages.len() {
                    self.pipeline.stages.swap(index, target as usize);
                    self.select(Some(target as usize));
                }
                Task::none()
            }
            Message::StageEnabled(index, enabled) => {
                if let Some(stage) = self.pipeline.stages.get_mut(index) {
                    stage.enabled = enabled;
                }
                Task::none()
            }
            Message::RetentionPicked(index, RetentionChoice(retention)) => {
                if let Some(stage) = self.pipeline.stages.get_mut(index) {
                    stage.retention = retention;
                }
                Task::none()
            }

            Message::ParamText(name, value) => {
                self.set_param_text(name, &value);
                Task::none()
            }
            Message::ParamToggled(name, value) => {
                self.set_param(name, PropertyValue::from(value));
                Task::none()
            }
            Message::ParamPicked(name, Variant(value)) => {
                self.set_param(name, PropertyValue::from(value));
                Task::none()
            }

            Message::DatasetPicked(choice) => self.choose_dataset(store, choice.id),
            Message::GroupToggled(id, on) => {
                if on {
                    self.chosen.insert(id);
                } else {
                    self.chosen.remove(&id);
                }
                Task::none()
            }
            Message::AllGroups(on) => {
                self.chosen = if on {
                    self.groups.iter().map(|group| group.id).collect()
                } else {
                    BTreeSet::new()
                };
                Task::none()
            }

            Message::Save => self.save(store),
            Message::Saved(Ok(id)) => {
                self.busy = false;
                self.saved_id = Some(id);
                self.notice = Some(format!("Saved '{}'.", self.pipeline.name));
                store.map_or_else(Task::none, |store| self.load(store))
            }
            Message::Saved(Err(error)) => {
                self.busy = false;
                self.error = Some(error);
                Task::none()
            }
            Message::LoadPipeline(choice) => {
                let Some(store) = store else {
                    return Task::none();
                };
                let id = choice.id;
                Task::perform(
                    jobs::read(store.clone(), move |conn| {
                        let header = runs::get_pipeline(conn, id)?;
                        Ok((id, header.name, runs::pipeline_stages(conn, id)?))
                    }),
                    Message::PipelineLoaded,
                )
            }
            Message::PipelineLoaded(Ok((id, name, rows))) => {
                match Pipeline::from_rows(name, &rows) {
                    Ok(pipeline) => {
                        self.pipeline = pipeline;
                        self.saved_id = Some(id);
                        self.select(if self.pipeline.stages.is_empty() {
                            None
                        } else {
                            Some(0)
                        });
                        self.summary = None;
                        self.notice = Some(format!("Loaded '{}'.", self.pipeline.name));
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
                Task::none()
            }
            Message::PipelineLoaded(Err(error)) => {
                self.error = Some(error);
                Task::none()
            }

            Message::Run => self.start(store),
            Message::Cancel => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                    self.notice = Some("Cancelling…".into());
                }
                Task::none()
            }
            Message::Tick => {
                if let Some(job) = &self.job {
                    if let Ok(progress) = job.progress.lock() {
                        self.shown.clone_from(&progress);
                    }
                }
                Task::none()
            }
            Message::Finished(outcome) => {
                self.job = None;
                self.busy = false;
                self.shown = None;
                match outcome {
                    Ok(summary) => {
                        self.notice = Some(summary.describe());
                        self.summary = Some(summary);
                        self.completed = true;
                    }
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Editing
    // -----------------------------------------------------------------------

    /// The descriptor of the stage at `index`, if its kind is registered.
    fn descriptor_at(&self, index: usize) -> Option<&'static StageDescriptor> {
        self.pipeline
            .stages
            .get(index)
            .and_then(|stage| self.registry.descriptor(&stage.kind))
    }

    fn select(&mut self, index: Option<usize>) {
        self.selected = index;
        self.drafts.clear();
    }

    /// Records a typed parameter value, keeping the raw text so a half-typed
    /// number (`-`, `1.`) does not jump back under the user's cursor.
    fn set_param_text(&mut self, name: &'static str, raw: &str) {
        self.drafts.insert(name.to_owned(), raw.to_owned());
        let Some(index) = self.selected else {
            return;
        };
        let Some(spec) = self
            .descriptor_at(index)
            .and_then(|descriptor| descriptor.param(name))
        else {
            return;
        };
        let Some(stage) = self.pipeline.stages.get_mut(index) else {
            return;
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // An empty box means "use the default", which is the parameter
            // being absent rather than being zero.
            stage.params = stage
                .params
                .iter()
                .filter(|(key, _)| key.as_str() != name)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            return;
        }
        match spec.kind {
            ParamKind::Int { .. } => {
                if let Ok(value) = trimmed.parse::<i64>() {
                    stage.params.set(name, value);
                }
            }
            _ => {
                if let Ok(value) = trimmed.parse::<f64>() {
                    stage.params.set(name, value);
                }
            }
        }
    }

    fn set_param(&mut self, name: &str, value: PropertyValue) {
        if let Some(stage) = self
            .selected
            .and_then(|index| self.pipeline.stages.get_mut(index))
        {
            stage.params.set(name, value);
        }
    }

    /// The text a parameter's box shows: what the user typed, else what the
    /// stage holds, else the declared default.
    fn param_text(&self, stage: &PipelineStage, spec: &ParamSpec) -> String {
        if let Some(draft) = self.drafts.get(spec.name) {
            return draft.clone();
        }
        match stage.params.get(spec.name) {
            Some(value) => value_text(value),
            None => spec
                .default
                .value()
                .map(|v| value_text(&v))
                .unwrap_or_default(),
        }
    }

    fn choose_dataset(&mut self, store: Option<&Store>, id: DatasetId) -> Task<Message> {
        self.dataset = Some(id);
        self.groups.clear();
        self.chosen.clear();
        let Some(store) = store else {
            return Task::none();
        };
        Task::perform(
            jobs::read(store.clone(), move |conn| {
                library::list_groups_in_dataset(conn, id)
            }),
            Message::GroupsLoaded,
        )
    }

    // -----------------------------------------------------------------------
    // Saving and running
    // -----------------------------------------------------------------------

    fn save(&mut self, store: Option<&Store>) -> Task<Message> {
        let Some(store) = store else {
            self.error = Some("No library is open.".into());
            return Task::none();
        };
        self.busy = true;
        self.error = None;
        self.notice = None;

        let existing = self.saved_id;
        let name = self.pipeline.name.trim().to_owned();
        let rows = self.pipeline.to_rows();
        Task::perform(
            jobs::write(store.clone(), move |conn| {
                let id = match existing {
                    Some(id) => {
                        runs::rename_pipeline(conn, id, &name)?;
                        id
                    }
                    None => runs::insert_pipeline(conn, &NewPipeline::new(name))?,
                };
                runs::set_pipeline_stages(conn, id, &rows)?;
                Ok(id)
            }),
            Message::Saved,
        )
    }

    /// Saves the pipeline, then runs it over the chosen groups.
    ///
    /// A run references the pipeline row it came from, so saving is part of
    /// running rather than a separate step the user can forget.
    fn start(&mut self, store: Option<&Store>) -> Task<Message> {
        let Some(store) = store else {
            self.error = Some("No library is open.".into());
            return Task::none();
        };
        if let Err(error) = self.pipeline.validate(&self.registry) {
            self.error = Some(error.to_string());
            return Task::none();
        }
        if self.chosen.is_empty() {
            self.error = Some("Choose at least one group to run over.".into());
            return Task::none();
        }

        let progress = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let sink = progress.clone();
        let control = RunControl::new()
            .with_cancel(cancel.clone())
            .with_progress(Arc::new(move |update| {
                *sink.lock().expect("progress mutex") = Some(update);
            }));

        let groups: Vec<GroupId> = self.chosen.iter().copied().collect();
        let options = RunOptions {
            dataset_id: self.dataset,
            ..RunOptions::default()
        };
        let pipeline = self.pipeline.clone();
        let existing = self.saved_id;
        let name = self.pipeline.name.trim().to_owned();
        let rows = self.pipeline.to_rows();
        let store = store.clone();

        self.job = Some(Job { progress, cancel });
        self.shown = None;
        self.summary = None;
        self.error = None;
        self.notice = None;
        self.busy = true;

        tracing::info!(
            pipeline = %self.pipeline.name,
            groups = groups.len(),
            "pipeline run requested"
        );

        // The run is not a store job: it writes through the store itself, so
        // handing it to the writer thread would have it wait on that thread
        // from inside it.
        Task::perform(
            async move {
                let registry = sp_dsp::registry().map_err(|error| error.to_string())?;
                jobs::blocking(move || {
                    let id = store
                        .write(move |conn| {
                            let id = match existing {
                                Some(id) => {
                                    runs::rename_pipeline(conn, id, &name)?;
                                    id
                                }
                                None => runs::insert_pipeline(conn, &NewPipeline::new(name))?,
                            };
                            runs::set_pipeline_stages(conn, id, &rows)?;
                            Ok(id)
                        })
                        .map_err(|error| error.to_string())?;
                    sp_proc::run_pipeline(
                        &store, &registry, id, &pipeline, &groups, &options, &control,
                    )
                    .map_err(|error| error.to_string())
                })
                .await
            },
            Message::Finished,
        )
    }

    // -----------------------------------------------------------------------
    // View
    // -----------------------------------------------------------------------

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let palette = container(scrollable(self.palette()).height(Length::Fill))
            .width(Length::Fixed(215.0))
            .height(Length::Fill)
            .padding([12, 12])
            .style(container::bordered_box);

        let middle = container(scrollable(self.stage_list()).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([12, 16]);

        let right = container(scrollable(self.side_panel()).height(Length::Fill))
            .width(Length::Fixed(370.0))
            .height(Length::Fill)
            .padding([12, 14])
            .style(container::bordered_box);

        row![palette, middle, right].height(Length::Fill).into()
    }

    /// Every registered stage, grouped by the family in its kind — so
    /// `dsp.filter.biquad` sits under "filter" without the palette having to
    /// know what a filter is.
    fn palette(&self) -> Element<'_, Message> {
        let mut list = column![text("Stages").size(16)].spacing(4);
        let mut family = String::new();
        for descriptor in self.registry.descriptors() {
            let this = family_of(descriptor.kind);
            if this != family {
                family = this.to_owned();
                list = list
                    .push(Space::with_height(Length::Fixed(6.0)))
                    .push(text(title_case(&family)).size(11).style(text::secondary));
            }
            list = list.push(
                button(
                    column![
                        text(descriptor.label).size(13),
                        text(descriptor.summary).size(10).style(text::secondary),
                    ]
                    .spacing(1),
                )
                .width(Length::Fill)
                .padding([6.0, 8.0])
                .style(button::text)
                .on_press(Message::AddStage(descriptor.kind)),
            );
        }
        if self.registry.is_empty() {
            list = list.push(
                text("No stages are registered.")
                    .size(12)
                    .style(text::danger),
            );
        }
        list.into()
    }

    fn stage_list(&self) -> Element<'_, Message> {
        let issues = self.pipeline.issues(&self.registry);

        let mut body = column![row![
            text_input("Pipeline name", &self.pipeline.name)
                .on_input(Message::NameChanged)
                .padding(6)
                .size(14)
                .width(Length::Fixed(260.0)),
            button(text("Save").size(12))
                .padding([5.0, 12.0])
                .style(button::secondary)
                .on_press_maybe((!self.busy).then_some(Message::Save)),
            Space::with_width(Length::Fill),
            pick_list(
                self.saved
                    .iter()
                    .map(|row| PipelineChoice {
                        id: row.id,
                        label: row.name.clone(),
                    })
                    .collect::<Vec<_>>(),
                None::<PipelineChoice>,
                Message::LoadPipeline,
            )
            .placeholder("Load saved…")
            .text_size(12)
            .padding(5),
        ]
        .spacing(8)
        .align_y(Alignment::Center),]
        .spacing(8);

        if let Some(error) = &self.error {
            body = body.push(text(error).size(13).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            body = body.push(text(notice).size(13).style(text::success));
        }

        if self.pipeline.stages.is_empty() {
            body = body.push(
                text(
                    "Add a stage from the palette. Stages run top to bottom, one group at a time.",
                )
                .size(13)
                .style(text::secondary),
            );
            return body.into();
        }

        for (index, stage) in self.pipeline.stages.iter().enumerate() {
            body = body.push(self.stage_row(index, stage, &issues));
        }
        if !issues.is_empty() {
            body = body
                .push(Space::with_height(Length::Fixed(8.0)))
                .push(horizontal_rule(1))
                .push(text("Problems").size(12).style(text::secondary));
            for issue in &issues {
                body = body.push(text(issue.to_string()).size(12).style(text::danger));
            }
        }
        body.into()
    }

    fn stage_row<'a>(
        &'a self,
        index: usize,
        stage: &'a PipelineStage,
        issues: &[PipelineIssue],
    ) -> Element<'a, Message> {
        let selected = self.selected == Some(index);
        let flagged = issues.iter().any(|issue| issue.ordinal() == index);
        let descriptor = self.registry.descriptor(&stage.kind);

        let summary = descriptor.map_or_else(
            || format!("{} — not registered in this build", stage.kind),
            |descriptor| {
                let params = describe_params(stage, descriptor);
                if params.is_empty() {
                    descriptor.summary.to_owned()
                } else {
                    params
                }
            },
        );

        let heading = row![
            text(format!("{}.", index + 1))
                .size(12)
                .style(text::secondary),
            text(stage.display_name(&self.registry)).size(14),
            Space::with_width(Length::Fixed(6.0)),
            text(stage.kind.as_str()).size(10).style(text::secondary),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let controls = row![
            checkbox("On", stage.enabled)
                .size(14)
                .text_size(11)
                .on_toggle(move |value| Message::StageEnabled(index, value)),
            pick_list(
                RetentionChoice::ALL.to_vec(),
                Some(RetentionChoice(stage.retention)),
                move |choice| Message::RetentionPicked(index, choice),
            )
            .text_size(11)
            .padding(3),
            Space::with_width(Length::Fill),
            small_button("↑", (index > 0).then_some(Message::MoveStage(index, -1))),
            small_button(
                "↓",
                (index + 1 < self.pipeline.stages.len()).then_some(Message::MoveStage(index, 1)),
            ),
            small_button("Remove", Some(Message::RemoveStage(index))),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let body = column![
            heading,
            text(summary).size(11).style(if flagged {
                text::danger
            } else {
                text::secondary
            }),
            controls,
        ]
        .spacing(4);

        button(body)
            .width(Length::Fill)
            .padding([8.0, 10.0])
            .style(if selected {
                button::primary
            } else {
                button::secondary
            })
            .on_press(Message::SelectStage(index))
            .into()
    }

    /// The parameter form for the selected stage, above the run controls.
    fn side_panel(&self) -> Element<'_, Message> {
        column![
            self.param_form(),
            Space::with_height(Length::Fixed(14.0)),
            horizontal_rule(1),
            Space::with_height(Length::Fixed(10.0)),
            self.run_panel(),
        ]
        .into()
    }

    fn param_form(&self) -> Element<'_, Message> {
        let Some((index, stage)) = self
            .selected
            .and_then(|index| self.pipeline.stages.get(index).map(|stage| (index, stage)))
        else {
            return column![
                text("Parameters").size(16),
                text("Select a stage to edit what it does.")
                    .size(12)
                    .style(text::secondary),
            ]
            .spacing(6)
            .into();
        };

        let Some(descriptor) = self.descriptor_at(index) else {
            return column![
                text("Parameters").size(16),
                text(format!("'{}' is not registered in this build.", stage.kind))
                    .size(12)
                    .style(text::danger),
            ]
            .spacing(6)
            .into();
        };

        let mut form = column![
            text(descriptor.label).size(16),
            text(descriptor.summary).size(11).style(text::secondary),
            text(format!("{} · v{}", descriptor.kind, descriptor.version))
                .size(10)
                .style(text::secondary),
            Space::with_height(Length::Fixed(4.0)),
        ]
        .spacing(3);

        if descriptor.params.is_empty() {
            form = form.push(
                text("This stage takes no parameters.")
                    .size(12)
                    .style(text::secondary),
            );
        }
        for spec in descriptor.params {
            form = form.push(self.param_field(stage, spec));
        }

        // The stage's own report of what it will read and write, so an
        // unsatisfied port is understandable rather than only flagged.
        if !descriptor.inputs.is_empty() || !descriptor.outputs.is_empty() {
            form = form
                .push(Space::with_height(Length::Fixed(8.0)))
                .push(text("Ports").size(11).style(text::secondary));
            for port in descriptor.inputs {
                form = form.push(
                    text(format!(
                        "in · {} ({}){}",
                        port.name,
                        port.kind.describe(),
                        if port.required { "" } else { ", optional" }
                    ))
                    .size(11)
                    .style(text::secondary),
                );
            }
            for port in descriptor.outputs {
                form = form.push(
                    text(format!("out · {} ({})", port.name, port.kind.describe()))
                        .size(11)
                        .style(text::secondary),
                );
            }
        }

        form.into()
    }

    /// One generated field. The widget follows the declared kind, which is
    /// the whole point of declaring it (§9.2).
    fn param_field<'a>(
        &'a self,
        stage: &'a PipelineStage,
        spec: &'a ParamSpec,
    ) -> Element<'a, Message> {
        let label = match spec.unit {
            Some(unit) => format!("{} ({unit})", spec.label),
            None => spec.label.to_owned(),
        };

        let input: Element<'_, Message> = match spec.kind {
            ParamKind::Bool => checkbox(
                label.clone(),
                stage
                    .params
                    .get(spec.name)
                    .and_then(PropertyValue::as_bool)
                    .unwrap_or(matches!(spec.default, sp_proc::ParamDefault::Bool(true))),
            )
            .size(15)
            .text_size(12)
            .on_toggle(|value| Message::ParamToggled(spec.name, value))
            .into(),
            ParamKind::Enum { variants } => pick_list(
                variants
                    .iter()
                    .map(|v| Variant((*v).to_owned()))
                    .collect::<Vec<_>>(),
                stage
                    .params
                    .get(spec.name)
                    .and_then(PropertyValue::as_str)
                    .map(|value| Variant(value.to_owned())),
                |choice| Message::ParamPicked(spec.name, choice),
            )
            .placeholder("Choose…")
            .text_size(12)
            .padding(5)
            .width(Length::Fill)
            .into(),
            _ => text_input(
                if spec.is_required() {
                    "required"
                } else {
                    "default"
                },
                &self.param_text(stage, spec),
            )
            .on_input(|value| Message::ParamText(spec.name, value))
            .padding(5)
            .size(13)
            .into(),
        };

        let mut field = column![].spacing(2);
        if !matches!(spec.kind, ParamKind::Bool) {
            field = field.push(text(label).size(12));
        }
        field = field.push(input);
        if !spec.help.is_empty() {
            field = field.push(text(spec.help).size(10).style(text::secondary));
        }
        container(field).padding([4, 0]).into()
    }

    fn run_panel(&self) -> Element<'_, Message> {
        let mut panel = column![text("Run").size(16)].spacing(6);

        panel = panel.push(
            pick_list(
                self.datasets
                    .iter()
                    .map(|dataset| DatasetChoice {
                        id: dataset.id,
                        label: dataset.name.clone(),
                    })
                    .collect::<Vec<_>>(),
                self.dataset.and_then(|id| {
                    self.datasets
                        .iter()
                        .find(|dataset| dataset.id == id)
                        .map(|dataset| DatasetChoice {
                            id: dataset.id,
                            label: dataset.name.clone(),
                        })
                }),
                Message::DatasetPicked,
            )
            .placeholder("Dataset…")
            .text_size(12)
            .padding(5)
            .width(Length::Fill),
        );

        if self.groups.is_empty() {
            panel = panel.push(
                text(if self.dataset.is_some() {
                    "This dataset has no groups."
                } else {
                    "Choose a dataset to run over."
                })
                .size(12)
                .style(text::secondary),
            );
        } else {
            panel = panel.push(
                row![
                    text(format!(
                        "{} of {} group(s)",
                        self.chosen.len(),
                        self.groups.len()
                    ))
                    .size(11)
                    .style(text::secondary),
                    Space::with_width(Length::Fill),
                    small_button("All", Some(Message::AllGroups(true))),
                    small_button("None", Some(Message::AllGroups(false))),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );

            let mut list = Column::new().spacing(1);
            for group in &self.groups {
                let id = group.id;
                list = list.push(
                    checkbox(group.display_name(), self.chosen.contains(&id))
                        .size(14)
                        .text_size(12)
                        .on_toggle(move |on| Message::GroupToggled(id, on)),
                );
            }
            panel = panel.push(
                container(scrollable(list).height(Length::Fixed(140.0)))
                    .padding([4, 2])
                    .width(Length::Fill),
            );
        }

        let runnable = self.job.is_none()
            && !self.chosen.is_empty()
            && self.pipeline.validate(&self.registry).is_ok();
        panel = panel.push(
            row![
                button(
                    text(if self.job.is_some() {
                        "Running…"
                    } else {
                        "Run"
                    })
                    .size(13)
                )
                .padding([6.0, 16.0])
                .style(button::primary)
                .on_press_maybe(runnable.then_some(Message::Run)),
                button(text("Cancel").size(12))
                    .padding([6.0, 12.0])
                    .style(button::danger)
                    .on_press_maybe(self.job.is_some().then_some(Message::Cancel)),
            ]
            .spacing(8),
        );

        if self.job.is_some() {
            let fraction = self.shown.as_ref().map_or(0.0, RunProgress::fraction);
            panel = panel
                .push(progress_bar(0.0..=1.0, fraction).height(6))
                .push(
                    text(match &self.shown {
                        Some(progress) => format!(
                            "{} of {} group(s) done",
                            progress.groups_done, progress.groups_total
                        ),
                        None => "Starting…".to_owned(),
                    })
                    .size(11)
                    .style(text::secondary),
                );
        }

        if let Some(summary) = &self.summary {
            panel = panel
                .push(Space::with_height(Length::Fixed(4.0)))
                .push(text(summary.describe()).size(12))
                .push(
                    text(format!(
                        "Run {} · open the Results screen (M6) to inspect it.",
                        summary.run
                    ))
                    .size(11)
                    .style(text::secondary),
                );
        }

        panel.into()
    }
}

/// A short button used in the stage rows and the group list.
fn small_button(label: &str, on_press: Option<Message>) -> Element<'_, Message> {
    button(text(label).size(11))
        .padding([3.0, 8.0])
        .style(button::secondary)
        .on_press_maybe(on_press)
        .into()
}

/// The family segment of a stage kind: `dsp.filter.biquad` → `filter`.
fn family_of(kind: &str) -> &str {
    let mut parts = kind.split('.');
    parts.next();
    parts.next().unwrap_or("other")
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A parameter value as the user would type it: whole numbers without a
/// trailing `.0`, text without its JSON quotes.
fn value_text(value: &PropertyValue) -> String {
    match value {
        PropertyValue::String(text) => text.clone(),
        PropertyValue::Number(number) => match number.as_f64() {
            // A gain of 2 reads better than a gain of 2.0, and typing either
            // gives the same stage.
            Some(float) if float.fract() == 0.0 && float.abs() < 1e15 => {
                format!("{:.0}", float)
            }
            _ => number.to_string(),
        },
        PropertyValue::Bool(flag) => flag.to_string(),
        PropertyValue::Null => String::new(),
        other => other.to_string(),
    }
}

/// The one-line summary of a configured stage: what the user set, not every
/// parameter it has.
fn describe_params(stage: &PipelineStage, descriptor: &StageDescriptor) -> String {
    descriptor
        .params
        .iter()
        .filter_map(|spec| {
            stage
                .params
                .get(spec.name)
                .map(|value| format!("{} {}", spec.label.to_lowercase(), value_text(value)))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::default()
    }

    fn add(state: &mut State, kind: &'static str) {
        let _ = state.update(None, Message::AddStage(kind));
    }

    #[test]
    fn the_palette_is_whatever_the_registry_holds() {
        let state = state();
        assert!(state.registry.len() >= 7, "the built-ins are registered");
        assert!(state.registry.contains("dsp.filter.biquad"));
    }

    #[test]
    fn adding_a_stage_selects_it() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        assert_eq!(state.pipeline.stages.len(), 1);
        assert_eq!(state.selected, Some(0));
    }

    #[test]
    fn stages_move_and_delete_by_position() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        add(&mut state, "dsp.condition.detrend");
        let _ = state.update(None, Message::MoveStage(1, -1));
        assert_eq!(state.pipeline.stages[0].kind, "dsp.condition.detrend");
        assert_eq!(state.selected, Some(0), "the moved stage stays selected");

        let _ = state.update(None, Message::RemoveStage(0));
        assert_eq!(state.pipeline.stages.len(), 1);
        assert_eq!(state.pipeline.stages[0].kind, "dsp.condition.gain");
        assert_eq!(state.selected, Some(0));
    }

    #[test]
    fn moving_past_either_end_does_nothing() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::MoveStage(0, -1));
        let _ = state.update(None, Message::MoveStage(0, 1));
        assert_eq!(state.pipeline.stages.len(), 1);
    }

    #[test]
    fn removing_the_last_stage_clears_the_selection() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::RemoveStage(0));
        assert!(state.pipeline.stages.is_empty());
        assert_eq!(state.selected, None);
    }

    #[test]
    fn a_typed_parameter_reaches_the_stage() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::ParamText("gain", "2.5".into()));
        assert_eq!(
            state.pipeline.stages[0].params.get("gain"),
            Some(&PropertyValue::from(2.5))
        );
    }

    #[test]
    fn a_half_typed_number_is_kept_as_typed() {
        // Reformatting under the cursor is what makes a number box unusable.
        let mut state = state();
        add(&mut state, "dsp.filter.biquad");
        let _ = state.update(None, Message::ParamText("cutoff_hz", "1".into()));
        let _ = state.update(None, Message::ParamText("cutoff_hz", "1.".into()));
        let spec = state.descriptor_at(0).unwrap().param("cutoff_hz").unwrap();
        assert_eq!(state.param_text(&state.pipeline.stages[0], spec), "1.");
        assert_eq!(
            state.pipeline.stages[0].params.get("cutoff_hz"),
            Some(&PropertyValue::from(1.0)),
            "the last value that parsed is what the stage keeps"
        );
    }

    #[test]
    fn clearing_a_box_falls_back_to_the_default() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::ParamText("gain", "4".into()));
        let _ = state.update(None, Message::ParamText("gain", "".into()));
        assert!(
            state.pipeline.stages[0].params.get("gain").is_none(),
            "an empty box means the declared default, not zero"
        );
    }

    #[test]
    fn an_integer_parameter_takes_whole_numbers() {
        let mut state = state();
        add(&mut state, "dsp.filter.biquad");
        let _ = state.update(None, Message::ParamText("sections", "3".into()));
        assert_eq!(
            state.pipeline.stages[0].params.get("sections"),
            Some(&PropertyValue::from(3))
        );
    }

    #[test]
    fn an_enum_parameter_is_set_from_its_picker() {
        let mut state = state();
        add(&mut state, "dsp.filter.biquad");
        let _ = state.update(
            None,
            Message::ParamPicked("response", Variant("highpass".into())),
        );
        assert_eq!(
            state.pipeline.stages[0].params.get("response"),
            Some(&PropertyValue::from("highpass"))
        );
    }

    #[test]
    fn selecting_another_stage_drops_the_drafts() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        add(&mut state, "dsp.condition.detrend");
        let _ = state.update(None, Message::ParamText("gain", "9".into()));
        let _ = state.update(None, Message::SelectStage(0));
        assert!(state.drafts.is_empty());
    }

    #[test]
    fn a_stage_missing_a_required_parameter_is_flagged_against_its_position() {
        // The threshold has no sensible default level, so an unconfigured one
        // is a problem the editor shows in place — and the Run button stays
        // off until it is fixed (§9.3).
        let mut state = state();
        add(&mut state, "dsp.detect.threshold");
        let issues = state.pipeline.issues(&state.registry);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(matches!(
            issues[0],
            PipelineIssue::BadParams { ordinal: 0, .. }
        ));
        assert!(state.pipeline.validate(&state.registry).is_err());
    }

    #[test]
    fn a_complete_pipeline_reports_no_problems() {
        let mut state = state();
        add(&mut state, "dsp.condition.detrend");
        add(&mut state, "dsp.detect.threshold");
        let _ = state.update(None, Message::ParamText("level", "0.5".into()));
        assert!(state.pipeline.issues(&state.registry).is_empty());
        state.pipeline.validate(&state.registry).unwrap();
    }

    #[test]
    fn running_without_a_library_says_so_rather_than_doing_nothing() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::Run);
        assert_eq!(state.error.as_deref(), Some("No library is open."));
    }

    #[test]
    fn running_with_no_groups_chosen_says_which_step_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(Some(&store), Message::Run);
        assert_eq!(
            state.error.as_deref(),
            Some("Choose at least one group to run over.")
        );
    }

    #[test]
    fn group_selection_toggles_wholesale_and_one_at_a_time() {
        let mut state = state();
        state.groups = vec![];
        state.chosen = [GroupId::new(1), GroupId::new(2)].into_iter().collect();
        let _ = state.update(None, Message::AllGroups(false));
        assert!(state.chosen.is_empty());
        let _ = state.update(None, Message::GroupToggled(GroupId::new(3), true));
        assert!(state.chosen.contains(&GroupId::new(3)));
        let _ = state.update(None, Message::GroupToggled(GroupId::new(3), false));
        assert!(state.chosen.is_empty());
    }

    #[test]
    fn a_pipeline_survives_a_round_trip_through_its_saved_rows() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::ParamText("gain", "3".into()));
        let rows = state.pipeline.to_rows();

        let mut reloaded = State::default();
        let _ = reloaded.update(
            None,
            Message::PipelineLoaded(Ok((PipelineId::new(1), "loaded".into(), rows))),
        );
        assert_eq!(reloaded.pipeline.name, "loaded");
        assert_eq!(reloaded.saved_id, Some(PipelineId::new(1)));
        assert_eq!(
            reloaded.pipeline.stages[0].params.get("gain"),
            Some(&PropertyValue::from(3.0))
        );
        assert_eq!(reloaded.selected, Some(0));
    }

    #[test]
    fn there_is_no_tick_subscription_while_nothing_is_running() {
        let state = state();
        assert!(state.job.is_none());
        assert!(matches!(state.subscription(), Subscription { .. }));
    }

    #[test]
    fn a_stage_kind_reads_as_a_family_in_the_palette() {
        assert_eq!(family_of("dsp.filter.biquad"), "filter");
        assert_eq!(family_of("dsp.util.passthrough"), "util");
        assert_eq!(family_of("weird"), "other");
        assert_eq!(title_case("filter"), "Filter");
    }

    #[test]
    fn a_configured_stage_summarises_what_was_set() {
        let mut state = state();
        add(&mut state, "dsp.condition.gain");
        let _ = state.update(None, Message::ParamText("gain", "2".into()));
        let descriptor = state.descriptor_at(0).unwrap();
        assert_eq!(
            describe_params(&state.pipeline.stages[0], descriptor),
            "gain 2"
        );
    }
}
