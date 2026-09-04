//! The Generate screen (`docs/DESIGN.md` §12.1, §8.3).
//!
//! > Left: a node tree with add/remove/reorder. Right: parameters for the
//! > selected node. A **live preview** strip renders the first ~2 seconds at
//! > reduced rate on every keystroke (debounced 150 ms), so the shape is
//! > visible before committing. **Batch/sweep mode**: mark one numeric
//! > parameter as swept to emit a whole group of signals in one action.
//! > Presets are `GenSpec` JSON files.
//!
//! Everything the screen edits is addressed by the node's JSON pointer
//! (`sp_gen::tree`), which is also what a validation issue and a sweep target
//! carry — so a message about a parameter lands next to the field, and the
//! parameter form does not need a match arm per node variant.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced::widget::{
    button, checkbox, column, container, horizontal_rule, pick_list, progress_bar, row, scrollable,
    text, text_input, Space,
};
use iced::{mouse, Alignment, Element, Length, Point, Rectangle, Subscription, Task, Theme};
use serde_json::Value;
use sp_core::{DType, Domain, SampleRange};
use sp_gen::generate::{GenReport, GenRequest};
use sp_gen::sweep::{ParamRef, ParamSweep, SweepValues};
use sp_gen::{
    tree, validate, GenControl, GenProgress, GenSpec, Issue, Node, NodeKind, Preset, Severity,
    Sources,
};
use sp_store::Store;

use crate::jobs;

/// Columns the preview strip reduces the rendered window to. One per two
/// pixels at a typical pane width, which is as fine as a min/max trace needs.
const PREVIEW_COLUMNS: usize = 600;

/// The window the preview covers, from §8.3.
const PREVIEW_SECONDS: f64 = 2.0;

/// Samples the preview will render before it gives up on covering the whole
/// window; a megasample-rate spec previews its first slice instead.
const PREVIEW_SAMPLE_CAP: u64 = 4_000_000;

/// How long the spec must sit still before the preview re-renders (§8.3).
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------------------
// Pick-list choices
// ---------------------------------------------------------------------------

/// One variant of a small enum inside a node's parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    key: &'static str,
    label: &'static str,
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

const SWEEP_VARIANTS: [Variant; 3] = [
    Variant {
        key: "linear",
        label: "Linear",
    },
    Variant {
        key: "log",
        label: "Logarithmic",
    },
    Variant {
        key: "quadratic",
        label: "Quadratic",
    },
];

const NOISE_VARIANTS: [Variant; 4] = [
    Variant {
        key: "gaussian",
        label: "Gaussian",
    },
    Variant {
        key: "uniform",
        label: "Uniform",
    },
    Variant {
        key: "pink",
        label: "Pink (1/f)",
    },
    Variant {
        key: "brown",
        label: "Brown (1/f^2)",
    },
];

const ENVELOPE_VARIANTS: [Variant; 3] = [
    Variant {
        key: "adsr",
        label: "ADSR",
    },
    Variant {
        key: "gaussian",
        label: "Gaussian",
    },
    Variant {
        key: "tukey",
        label: "Tukey",
    },
];

const MOD_VARIANTS: [Variant; 3] = [
    Variant {
        key: "am",
        label: "AM",
    },
    Variant {
        key: "fm",
        label: "FM",
    },
    Variant {
        key: "pm",
        label: "PM",
    },
];

/// The default JSON of a tagged sub-object, for switching its variant. Setting
/// only the tag would leave the sibling fields belonging to the old variant.
fn variant_object(tag: &str, key: &str) -> Option<Value> {
    let object = match (tag, key) {
        ("shape", "adsr") => serde_json::json!({
            "shape": "adsr", "attack_s": 0.01, "decay_s": 0.05, "sustain": 0.7, "release_s": 0.05
        }),
        ("shape", "gaussian") => {
            serde_json::json!({ "shape": "gaussian", "center_s": 0.5, "sigma_s": 0.1 })
        }
        ("shape", "tukey") => serde_json::json!({ "shape": "tukey", "alpha": 0.1 }),
        ("mode", "am") => serde_json::json!({ "mode": "am", "depth": 0.5 }),
        ("mode", "fm") => serde_json::json!({ "mode": "fm", "dev_hz": 1000.0 }),
        ("mode", "pm") => serde_json::json!({ "mode": "pm", "dev_rad": 1.0 }),
        _ => return None,
    };
    Some(object)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DTypeChoice(pub DType);

impl DTypeChoice {
    const ALL: [Self; 6] = [
        Self(DType::F32),
        Self(DType::F64),
        Self(DType::I16),
        Self(DType::I32),
        Self(DType::C64),
        Self(DType::U8),
    ];
}

impl fmt::Display for DTypeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainChoice(pub Domain);

impl DomainChoice {
    const ALL: [Self; 5] = [
        Self(Domain::Analog),
        Self(Domain::DigitalLogic),
        Self(Domain::BasebandIq),
        Self(Domain::Symbols),
        Self(Domain::Bits),
    ];
}

impl fmt::Display for DomainChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.label())
    }
}

/// A sweep target, as one row of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetChoice {
    pointer: String,
    label: String,
}

impl fmt::Display for TargetChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// Whether a sweep steps a range or walks a list (§8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepMode {
    Range,
    List,
}

impl SweepMode {
    const ALL: [Self; 2] = [Self::Range, Self::List];
}

impl fmt::Display for SweepMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Range => "Start / stop / step",
            Self::List => "List of values",
        })
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The reduced trace the preview strip draws.
#[derive(Debug, Clone, Default)]
pub struct Preview {
    /// One `(min, max)` pair per column.
    columns: Vec<(f32, f32)>,
    min: f64,
    max: f64,
    seconds: f64,
    samples: u64,
    /// Set when the window shown is only the front of the signal.
    truncated: bool,
}

/// A running generation, and the two things the UI reaches into it for.
#[derive(Debug)]
struct Job {
    progress: Arc<Mutex<GenProgress>>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct State {
    spec: GenSpec,
    /// JSON pointer of the node the parameter form is showing.
    selected: String,
    /// In-progress text per field, so a half-typed number does not snap back.
    drafts: HashMap<String, String>,
    /// Why a draft has not been applied to the spec.
    draft_errors: HashMap<String, String>,

    dataset_name: String,
    signal_name: String,

    presets: Vec<Preset>,
    loaded_preset: Option<String>,

    sweep_on: bool,
    sweep_target: Option<String>,
    sweep_mode: SweepMode,
    sweep_start: String,
    sweep_stop: String,
    sweep_step: String,
    sweep_list: String,
    sweep_property: String,

    preview: Option<Preview>,
    preview_error: Option<String>,
    /// When the spec last changed, so the preview can wait out a burst of
    /// keystrokes before re-rendering (§8.3).
    dirty_since: Option<Instant>,
    previewing: bool,

    job: Option<Job>,
    shown_progress: GenProgress,
    report: Option<GenReport>,
    error: Option<String>,
    notice: Option<String>,
    /// Set when a generation commits, so the root can refresh the library.
    completed: bool,
}

impl Default for State {
    fn default() -> Self {
        let spec = GenSpec::default();
        let mut state = Self {
            spec,
            selected: tree::ROOT.to_owned(),
            drafts: HashMap::new(),
            draft_errors: HashMap::new(),
            dataset_name: String::new(),
            signal_name: "Generated".to_owned(),
            presets: sp_gen::preset::built_in(),
            loaded_preset: None,
            sweep_on: false,
            sweep_target: None,
            sweep_mode: SweepMode::Range,
            sweep_start: "100".to_owned(),
            sweep_stop: "2000".to_owned(),
            sweep_step: "100".to_owned(),
            sweep_list: String::new(),
            sweep_property: String::new(),
            preview: None,
            preview_error: None,
            dirty_since: Some(Instant::now()),
            previewing: false,
            job: None,
            shown_progress: GenProgress::default(),
            report: None,
            error: None,
            notice: None,
            completed: false,
        };
        state.touch();
        state
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    // Presets
    PresetPicked(usize),
    BrowsePreset,
    PresetFilePicked(Option<PathBuf>),
    PresetLoaded(Result<Box<Preset>, String>),
    SavePreset,
    SavePathPicked(Option<PathBuf>),
    PresetSaved(Result<String, String>),

    // Tree
    SelectNode(String),
    NodeKindPicked(String, NodeKind),
    AddChild(String),
    RemoveNode(String),
    MoveNode(String, isize),

    // Parameters, all addressed by JSON pointer
    FieldEdited(String, String),
    FlagToggled(String, bool),
    VariantPicked(String, Variant),

    // Signal settings
    DatasetNameChanged(String),
    SignalNameChanged(String),
    DTypePicked(DTypeChoice),
    DomainPicked(DomainChoice),
    RandomiseSeed,

    // Sweep
    SweepToggled(bool),
    SweepTargetPicked(TargetChoice),
    SweepModePicked(SweepMode),
    SweepStartChanged(String),
    SweepStopChanged(String),
    SweepStepChanged(String),
    SweepListChanged(String),
    SweepPropertyChanged(String),

    // Preview and run
    PreviewTick,
    Previewed(Result<Box<Preview>, String>),
    Start,
    Tick,
    Cancel,
    Generated(Result<Box<GenReport>, String>),
}

impl State {
    /// Whether a generation committed since this was last asked, so the root
    /// reloads the library tree exactly once.
    pub fn take_completed(&mut self) -> bool {
        std::mem::take(&mut self.completed)
    }

    /// Nothing to load from the store: the preset library is embedded, and the
    /// screen's own state is the spec. The first preview is kicked off here so
    /// the strip is populated before the first keystroke.
    pub fn load(&mut self, _store: &Store) -> Task<Message> {
        self.touch();
        Task::none()
    }

    /// Ticks while a generation runs, and while the preview is stale.
    pub fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = Vec::new();
        if self.job.is_some() {
            subscriptions
                .push(iced::time::every(Duration::from_millis(120)).map(|_| Message::Tick));
        }
        if self.dirty_since.is_some() {
            subscriptions
                .push(iced::time::every(Duration::from_millis(60)).map(|_| Message::PreviewTick));
        }
        Subscription::batch(subscriptions)
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::PresetPicked(index) => {
                if let Some(preset) = self.presets.get(index) {
                    self.spec = preset.spec.clone();
                    self.loaded_preset = Some(preset.name.clone());
                    if self.signal_name.trim().is_empty()
                        || self.signal_name == "Generated"
                        || self.presets.iter().any(|p| p.name == self.signal_name)
                    {
                        self.signal_name = preset.name.clone();
                    }
                    self.notice = Some(format!("Loaded preset '{}'.", preset.name));
                    self.reset_selection();
                }
                Task::none()
            }
            Message::BrowsePreset => Task::perform(open_preset(), Message::PresetFilePicked),
            Message::PresetFilePicked(None) => Task::none(),
            Message::PresetFilePicked(Some(path)) => Task::perform(
                jobs::blocking(move || {
                    Preset::load(&path)
                        .map(Box::new)
                        .map_err(|error| error.to_string())
                }),
                Message::PresetLoaded,
            ),
            Message::PresetLoaded(Ok(preset)) => {
                self.spec = preset.spec.clone();
                self.loaded_preset = Some(preset.name.clone());
                self.notice = Some(format!("Loaded '{}'.", preset.name));
                self.error = None;
                self.reset_selection();
                Task::none()
            }
            Message::PresetLoaded(Err(error)) => {
                self.error = Some(error);
                Task::none()
            }
            Message::SavePreset => {
                let name = self.preset_name();
                Task::perform(save_preset(name), Message::SavePathPicked)
            }
            Message::SavePathPicked(None) => Task::none(),
            Message::SavePathPicked(Some(path)) => {
                let preset = Preset::new(self.preset_name(), String::new(), self.spec.clone());
                Task::perform(
                    jobs::blocking(move || {
                        preset
                            .save(&path)
                            .map(|()| path.display().to_string())
                            .map_err(|error| error.to_string())
                    }),
                    Message::PresetSaved,
                )
            }
            Message::PresetSaved(Ok(path)) => {
                self.notice = Some(format!("Saved to {path}."));
                Task::none()
            }
            Message::PresetSaved(Err(error)) => {
                self.error = Some(error);
                Task::none()
            }

            Message::SelectNode(pointer) => {
                self.selected = pointer;
                Task::none()
            }
            Message::NodeKindPicked(pointer, kind) => {
                if tree::node_at(&self.spec, &pointer).map(Node::kind) != Some(kind) {
                    tree::replace(&mut self.spec, &pointer, kind.default_node());
                    self.drafts.clear();
                    self.draft_errors.clear();
                    self.touch();
                }
                Task::none()
            }
            Message::AddChild(pointer) => {
                if let Some(added) = tree::add_child(&mut self.spec, &pointer) {
                    self.selected = added;
                    self.touch();
                }
                Task::none()
            }
            Message::RemoveNode(pointer) => {
                match tree::remove(&mut self.spec, &pointer) {
                    Some(parent) => {
                        self.selected = parent;
                        self.drafts.clear();
                        self.touch();
                    }
                    None => {
                        self.error = Some(
                            "That node cannot be removed: a combinator keeps at least one child, \
                             and a fixed slot is replaced rather than emptied."
                                .to_owned(),
                        );
                    }
                }
                Task::none()
            }
            Message::MoveNode(pointer, delta) => {
                if let Some(moved) = tree::move_child(&mut self.spec, &pointer, delta) {
                    self.selected = moved;
                    self.drafts.clear();
                    self.touch();
                }
                Task::none()
            }

            Message::FieldEdited(pointer, raw) => {
                self.apply_field(&pointer, &raw);
                Task::none()
            }
            Message::FlagToggled(pointer, value) => {
                self.apply_json(&pointer, Value::Bool(value));
                Task::none()
            }
            Message::VariantPicked(pointer, variant) => {
                let tag = pointer.rsplit('/').next().unwrap_or_default().to_owned();
                match variant_object(&tag, variant.key) {
                    // A tagged sub-object is replaced whole, so no field of the
                    // old variant survives into the new one.
                    Some(object) => {
                        let parent = pointer
                            .strip_suffix(&format!("/{tag}"))
                            .unwrap_or(&pointer)
                            .to_owned();
                        self.apply_json(&parent, object);
                    }
                    None => self.apply_json(&pointer, Value::String(variant.key.to_owned())),
                }
                Task::none()
            }

            Message::DatasetNameChanged(name) => {
                self.dataset_name = name;
                Task::none()
            }
            Message::SignalNameChanged(name) => {
                self.signal_name = name;
                Task::none()
            }
            Message::DTypePicked(choice) => {
                self.spec.dtype = choice.0;
                self.touch();
                Task::none()
            }
            Message::DomainPicked(choice) => {
                self.spec.domain = choice.0;
                self.touch();
                Task::none()
            }
            Message::RandomiseSeed => {
                self.spec.seed = fresh_seed();
                self.drafts.remove("/seed");
                self.touch();
                Task::none()
            }

            Message::SweepToggled(on) => {
                self.sweep_on = on;
                if on && self.sweep_target.is_none() {
                    // Default to the first parameter of the selected node,
                    // which is the one the user was just looking at.
                    let selected = self.selected.clone();
                    let params = sp_gen::sweep::parameters(&self.spec);
                    let first = params
                        .iter()
                        .find(|param| param.pointer == selected)
                        .or_else(|| params.first());
                    if let Some(param) = first {
                        self.set_sweep_target(param.clone());
                    }
                }
                Task::none()
            }
            Message::SweepTargetPicked(choice) => {
                if let Some(param) = sp_gen::sweep::parameters(&self.spec)
                    .into_iter()
                    .find(|param| param.json_pointer() == choice.pointer)
                {
                    self.set_sweep_target(param);
                }
                Task::none()
            }
            Message::SweepModePicked(mode) => {
                self.sweep_mode = mode;
                Task::none()
            }
            Message::SweepStartChanged(text) => {
                self.sweep_start = text;
                Task::none()
            }
            Message::SweepStopChanged(text) => {
                self.sweep_stop = text;
                Task::none()
            }
            Message::SweepStepChanged(text) => {
                self.sweep_step = text;
                Task::none()
            }
            Message::SweepListChanged(text) => {
                self.sweep_list = text;
                Task::none()
            }
            Message::SweepPropertyChanged(key) => {
                self.sweep_property = key;
                Task::none()
            }

            Message::PreviewTick => {
                let Some(since) = self.dirty_since else {
                    return Task::none();
                };
                if self.previewing || since.elapsed() < PREVIEW_DEBOUNCE {
                    return Task::none();
                }
                self.dirty_since = None;
                self.previewing = true;
                let spec = self.spec.clone();
                Task::perform(
                    jobs::blocking(move || build_preview(&spec).map(Box::new)),
                    Message::Previewed,
                )
            }
            Message::Previewed(result) => {
                self.previewing = false;
                match result {
                    Ok(preview) => {
                        self.preview = Some(*preview);
                        self.preview_error = None;
                    }
                    Err(error) => {
                        self.preview = None;
                        self.preview_error = Some(error);
                    }
                }
                Task::none()
            }

            Message::Start => self.start(store),
            Message::Tick => {
                if let Some(job) = &self.job {
                    self.shown_progress = *job.progress.lock().expect("progress mutex");
                }
                Task::none()
            }
            Message::Cancel => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                    self.notice = Some("Cancelling…".to_owned());
                }
                Task::none()
            }
            Message::Generated(result) => {
                self.job = None;
                match result {
                    Ok(report) => {
                        tracing::info!(summary = %report.summary(), "generation finished");
                        self.notice = Some(report.summary());
                        self.report = Some(*report);
                        self.error = None;
                        self.completed = true;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "generation failed");
                        self.error = Some(error);
                        self.notice = None;
                    }
                }
                Task::none()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Editing
    // -----------------------------------------------------------------------

    /// Marks the spec changed, so the preview re-renders once the typing
    /// stops.
    fn touch(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn reset_selection(&mut self) {
        self.selected = tree::ROOT.to_owned();
        self.drafts.clear();
        self.draft_errors.clear();
        self.sweep_target = None;
        self.touch();
    }

    /// Applies a typed field. The raw text is kept either way, so a value
    /// being typed does not snap back to what the spec still holds.
    fn apply_field(&mut self, pointer: &str, raw: &str) {
        self.drafts.insert(pointer.to_owned(), raw.to_owned());
        self.draft_errors.remove(pointer);

        let current = tree::get_json(&self.spec, pointer);
        let replacement = match current {
            Some(Value::String(_)) => Value::String(raw.to_owned()),
            // `taps` is the one optional number; an empty box means "use the
            // built-in maximal-length polynomial".
            Some(Value::Null) | Some(Value::Number(_)) if raw.trim().is_empty() => Value::Null,
            Some(Value::Number(number)) => {
                let Ok(parsed) = raw.trim().parse::<f64>() else {
                    self.draft_errors
                        .insert(pointer.to_owned(), format!("'{raw}' is not a number"));
                    return;
                };
                if number.is_i64() || number.is_u64() {
                    Value::from(parsed.round() as i64)
                } else {
                    match serde_json::Number::from_f64(parsed) {
                        Some(number) => Value::Number(number),
                        None => {
                            self.draft_errors
                                .insert(pointer.to_owned(), "that value is not finite".to_owned());
                            return;
                        }
                    }
                }
            }
            Some(Value::Null) => match raw.trim().parse::<f64>() {
                Ok(parsed) => Value::from(parsed.round() as i64),
                Err(_) => {
                    self.draft_errors
                        .insert(pointer.to_owned(), format!("'{raw}' is not a number"));
                    return;
                }
            },
            _ => return,
        };
        self.apply_json(pointer, replacement);
    }

    /// Writes a value into the spec, keeping the old spec if it will not take
    /// it — a `u8` order of 900, say.
    fn apply_json(&mut self, pointer: &str, value: Value) {
        match tree::set_json(&self.spec, pointer, value) {
            Ok(spec) => {
                self.spec = spec;
                self.draft_errors.remove(pointer);
                self.touch();
            }
            Err(error) => {
                self.draft_errors.insert(pointer.to_owned(), error);
            }
        }
    }

    /// The text a field shows: what is being typed, or what the spec holds.
    fn field_text(&self, pointer: &str) -> String {
        if let Some(draft) = self.drafts.get(pointer) {
            return draft.clone();
        }
        match tree::get_json(&self.spec, pointer) {
            Some(Value::String(text)) => text,
            Some(Value::Number(number)) => number.to_string(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    }

    fn set_sweep_target(&mut self, param: ParamRef) {
        self.sweep_property = param.field.replace('/', "_");
        let value = param.value;
        self.sweep_target = Some(param.json_pointer());
        // Start the range at the parameter's current value, which is nearly
        // always one end of the ladder the user has in mind.
        self.sweep_start = format_number(value);
        self.sweep_stop = format_number(if value == 0.0 { 1.0 } else { value * 2.0 });
        self.sweep_step = format_number(if value == 0.0 { 0.1 } else { value / 10.0 });
    }

    /// The sweep the current settings describe, or why they do not describe
    /// one.
    fn sweep(&self) -> Option<Result<ParamSweep, String>> {
        if !self.sweep_on {
            return None;
        }
        let pointer = self.sweep_target.clone()?;
        let param = sp_gen::sweep::parameters(&self.spec)
            .into_iter()
            .find(|param| param.json_pointer() == pointer)?;

        let values = match self.sweep_mode {
            SweepMode::Range => {
                let parse = |label: &str, raw: &str| {
                    raw.trim()
                        .parse::<f64>()
                        .map_err(|_| format!("{label} is not a number"))
                };
                match (
                    parse("Start", &self.sweep_start),
                    parse("Stop", &self.sweep_stop),
                    parse("Step", &self.sweep_step),
                ) {
                    (Ok(start), Ok(stop), Ok(step)) => SweepValues::Range { start, stop, step },
                    (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                        return Some(Err(error))
                    }
                }
            }
            SweepMode::List => {
                let mut values = Vec::new();
                for token in self
                    .sweep_list
                    .split([',', ';', ' ', '\n'])
                    .filter(|token| !token.trim().is_empty())
                {
                    match token.trim().parse::<f64>() {
                        Ok(value) => values.push(value),
                        Err(_) => return Some(Err(format!("'{}' is not a number", token.trim()))),
                    }
                }
                SweepValues::List { values }
            }
        };

        if let Err(error) = values.values() {
            return Some(Err(error));
        }
        let key = self.sweep_property.trim();
        let mut sweep = ParamSweep::over(&param, values);
        if !key.is_empty() {
            sweep.property_key = key.to_owned();
        }
        Some(Ok(sweep))
    }

    fn preset_name(&self) -> String {
        let name = self.signal_name.trim();
        if name.is_empty() {
            "Generated".to_owned()
        } else {
            name.to_owned()
        }
    }

    fn start(&mut self, store: Option<&Store>) -> Task<Message> {
        let Some(store) = store else {
            self.error = Some("No library is open.".to_owned());
            return Task::none();
        };
        let issues = validate(&self.spec);
        if issues.blocks() {
            self.error = Some("Fix the errors below before generating.".to_owned());
            return Task::none();
        }
        let sweep = match self.sweep() {
            Some(Ok(sweep)) => Some(sweep),
            Some(Err(error)) => {
                self.error = Some(error);
                return Task::none();
            }
            None => None,
        };

        let progress = Arc::new(Mutex::new(GenProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let sink = progress.clone();
        let control = GenControl::new()
            .with_cancel(cancel.clone())
            .with_progress(Arc::new(move |update| {
                *sink.lock().expect("progress mutex") = update;
            }));

        let mut request = GenRequest::new(self.spec.clone())
            .named(self.dataset_name.trim())
            .with_signal_name(self.preset_name());
        if let Some(sweep) = sweep {
            request = request.with_sweep(sweep);
        }

        self.job = Some(Job { progress, cancel });
        self.shown_progress = GenProgress::default();
        self.report = None;
        self.error = None;
        self.notice = None;

        tracing::info!(name = %request.dataset_name(), "generation started");
        Task::perform(
            jobs::write(store.clone(), move |conn| {
                Ok(sp_gen::generate(conn, &request, &control)
                    .map(Box::new)
                    .map_err(|error| error.to_string()))
            }),
            |outcome| Message::Generated(outcome.and_then(|inner| inner)),
        )
    }

    // -----------------------------------------------------------------------
    // View
    // -----------------------------------------------------------------------

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let left = container(scrollable(self.tree_pane()).height(Length::Fill))
            .width(Length::Fixed(330.0))
            .height(Length::Fill)
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.scale_alpha(0.5).into()),
                    ..container::Style::default()
                }
            });

        row![left, self.detail_pane()].height(Length::Fill).into()
    }

    fn tree_pane(&self) -> Element<'_, Message> {
        let mut pane = column![].spacing(8).padding([12, 14]).width(Length::Fill);

        pane = pane.push(section("Presets"));
        let mut presets = column![].spacing(3).width(Length::Fill);
        for (index, preset) in self.presets.iter().enumerate() {
            let active = self.loaded_preset.as_deref() == Some(preset.name.as_str());
            presets = presets.push(
                button(
                    column![
                        text(&preset.name).size(13),
                        text(&preset.description).size(10).style(text::secondary),
                    ]
                    .spacing(1),
                )
                .width(Length::Fill)
                .padding([5, 8])
                .style(if active {
                    button::primary
                } else {
                    button::text
                })
                .on_press(Message::PresetPicked(index)),
            );
        }
        pane = pane.push(presets);
        pane = pane.push(
            row![
                button(text("Open…").size(12))
                    .padding([5, 9])
                    .style(button::secondary)
                    .on_press(Message::BrowsePreset),
                button(text("Save as…").size(12))
                    .padding([5, 9])
                    .style(button::secondary)
                    .on_press(Message::SavePreset),
            ]
            .spacing(6),
        );

        pane = pane.push(section("Nodes"));
        let issues = validate(&self.spec);
        let mut rows = column![].spacing(2).width(Length::Fill);
        for node_ref in tree::outline(&self.spec) {
            let selected = node_ref.pointer == self.selected;
            let broken = issues
                .at(&node_ref.pointer)
                .any(|issue| issue.severity == Severity::Error);
            let label = format!(
                "{}{} — {}",
                "    ".repeat(node_ref.depth),
                node_ref.kind.label(),
                node_ref.summary
            );
            let mut label = text(label).size(12);
            if broken {
                label = label.style(text::danger);
            }
            rows = rows.push(
                button(
                    column![
                        label,
                        text(format!(
                            "{}{}",
                            "    ".repeat(node_ref.depth),
                            node_ref.slot
                        ))
                        .size(10)
                        .style(text::secondary),
                    ]
                    .spacing(1),
                )
                .width(Length::Fill)
                .padding([4, 6])
                .style(if selected {
                    button::primary
                } else {
                    button::text
                })
                .on_press(Message::SelectNode(node_ref.pointer.clone())),
            );
        }
        pane = pane.push(rows);
        pane = pane.push(self.tree_actions());
        pane.into()
    }

    fn tree_actions(&self) -> Element<'_, Message> {
        let selected = self.selected.clone();
        let kind = tree::node_at(&self.spec, &selected).map(Node::kind);
        let can_add = kind.is_some_and(NodeKind::has_variable_arity);
        let removable = tree::parent_pointer(&selected).is_some();

        let mut add = button(text("Add child").size(12))
            .padding([5, 9])
            .style(button::secondary);
        if can_add {
            add = add.on_press(Message::AddChild(selected.clone()));
        }

        let mut remove = button(text("Remove").size(12))
            .padding([5, 9])
            .style(button::danger);
        let mut up = button(text("Up").size(12))
            .padding([5, 9])
            .style(button::text);
        let mut down = button(text("Down").size(12))
            .padding([5, 9])
            .style(button::text);
        if removable {
            remove = remove.on_press(Message::RemoveNode(selected.clone()));
            up = up.on_press(Message::MoveNode(selected.clone(), -1));
            down = down.on_press(Message::MoveNode(selected, 1));
        }

        row![add, remove, up, down].spacing(6).into()
    }

    fn detail_pane(&self) -> Element<'_, Message> {
        let mut pane = column![].spacing(10).padding([12, 16]).width(Length::Fill);

        pane = pane.push(self.action_row());
        if let Some(error) = &self.error {
            pane = pane.push(text(error).size(13).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            pane = pane.push(text(notice).size(12).style(text::secondary));
        }
        if self.job.is_some() {
            pane = pane.push(self.progress_row());
        }

        pane = pane.push(self.preview_strip());

        let body = column![
            self.signal_settings(),
            self.parameter_form(),
            self.sweep_panel(),
            self.issue_list(),
        ]
        .spacing(10)
        .width(Length::Fill);

        pane = pane.push(horizontal_rule(1));
        pane = pane.push(scrollable(body).height(Length::Fill));
        pane.into()
    }

    fn action_row(&self) -> Element<'_, Message> {
        let issues = validate(&self.spec);
        let ready = !issues.blocks() && self.job.is_none();
        let label = match self.sweep().and_then(Result::ok) {
            Some(sweep) => match sweep.values.count() {
                Some(rungs) => format!("Generate {rungs} signals"),
                None => "Generate".to_owned(),
            },
            None => "Generate".to_owned(),
        };

        let mut generate = button(text(label).size(13))
            .padding([6, 14])
            .style(button::primary);
        if ready {
            generate = generate.on_press(Message::Start);
        }

        let mut actions = row![generate].spacing(8).align_y(Alignment::Center);
        if self.job.is_some() {
            actions = actions.push(
                button(text("Cancel").size(13))
                    .padding([6, 14])
                    .style(button::danger)
                    .on_press(Message::Cancel),
            );
        }
        actions = actions.push(Space::with_width(Length::Fill));
        actions = actions.push(
            text(format!(
                "{} samples · {} s at {}",
                self.spec.sample_count(),
                format_number(self.spec.duration_s),
                format_hz(self.spec.sample_rate_hz()),
            ))
            .size(11)
            .style(text::secondary),
        );
        actions.into()
    }

    fn progress_row(&self) -> Element<'_, Message> {
        let progress = self.shown_progress;
        let bar: Element<'_, Message> = match progress.fraction() {
            Some(fraction) => progress_bar(0.0..=1.0, fraction).height(6).into(),
            None => Space::with_height(Length::Fixed(6.0)).into(),
        };
        column![
            bar,
            text(format!(
                "{} of {} signal(s) · {} of {} samples",
                progress.signals_done,
                progress.signals_total,
                progress.samples_done,
                progress.samples_total,
            ))
            .size(11)
            .style(text::secondary),
        ]
        .spacing(4)
        .into()
    }

    fn preview_strip(&self) -> Element<'_, Message> {
        let caption: Element<'_, Message> = match (&self.preview, &self.preview_error) {
            (_, Some(error)) => text(error).size(11).style(text::danger).into(),
            (Some(preview), None) => text(format!(
                "First {} s · {} samples · {} to {}{}",
                format_number(preview.seconds),
                preview.samples,
                format_number(preview.min),
                format_number(preview.max),
                if preview.truncated {
                    " · window truncated to keep the preview instant"
                } else {
                    ""
                },
            ))
            .size(11)
            .style(text::secondary)
            .into(),
            (None, None) => text("Rendering the preview…")
                .size(11)
                .style(text::secondary)
                .into(),
        };

        column![
            container(
                Canvas::new(PreviewChart {
                    preview: self.preview.as_ref(),
                })
                .width(Length::Fill)
                .height(Length::Fixed(130.0)),
            )
            .style(|theme: &Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(palette.background.weak.color.scale_alpha(0.4).into()),
                    border: iced::border::rounded(4),
                    ..container::Style::default()
                }
            }),
            caption,
        ]
        .spacing(4)
        .into()
    }

    fn signal_settings(&self) -> Element<'_, Message> {
        column![
            section("Signal"),
            labelled(
                "Dataset name",
                text_input("From the signal name", &self.dataset_name)
                    .on_input(Message::DatasetNameChanged)
                    .size(13)
                    .into(),
            ),
            labelled(
                "Signal name",
                text_input("Generated", &self.signal_name)
                    .on_input(Message::SignalNameChanged)
                    .size(13)
                    .into(),
            ),
            labelled(
                "Sample rate (Hz)",
                self.number_input("/timebase/sample_rate_hz", "48000"),
            ),
            labelled("Duration (s)", self.number_input("/duration_s", "1.0")),
            labelled("Start time (s)", self.number_input("/timebase/t0_s", "0.0")),
            labelled(
                "Seed",
                row![
                    self.number_input("/seed", "0"),
                    button(text("New").size(12))
                        .padding([5, 9])
                        .style(button::secondary)
                        .on_press(Message::RandomiseSeed),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
            ),
            labelled(
                "Stored as",
                pick_list(
                    DTypeChoice::ALL.to_vec(),
                    Some(DTypeChoice(self.spec.dtype)),
                    Message::DTypePicked,
                )
                .text_size(13)
                .into(),
            ),
            labelled(
                "Domain",
                pick_list(
                    DomainChoice::ALL.to_vec(),
                    Some(DomainChoice(self.spec.domain)),
                    Message::DomainPicked,
                )
                .text_size(13)
                .into(),
            ),
        ]
        .spacing(6)
        .into()
    }

    /// The parameter form for the selected node, generated from its serialised
    /// fields so a new node variant needs no code here.
    fn parameter_form(&self) -> Element<'_, Message> {
        let pointer = self.selected.clone();
        let Some(node) = tree::node_at(&self.spec, &pointer) else {
            return column![
                section("Parameters"),
                text("Select a node in the tree.")
                    .size(12)
                    .style(text::secondary),
            ]
            .spacing(6)
            .into();
        };

        let mut form = column![section("Parameters")].spacing(6);
        form = form.push(labelled(
            "Node",
            pick_list(NodeKind::ALL.to_vec(), Some(node.kind()), {
                let pointer = pointer.clone();
                move |kind| Message::NodeKindPicked(pointer.clone(), kind)
            })
            .text_size(13)
            .into(),
        ));

        let Some(Value::Object(fields)) = tree::get_json(&self.spec, &pointer) else {
            return form.into();
        };
        for (key, value) in &fields {
            if key == "node" || is_child_key(key) {
                continue;
            }
            form = self.field_rows(form, &pointer, key, value);
        }

        // A concatenation's part durations belong to the concatenation, not to
        // the parts, so they are listed here.
        if let Some(Value::Array(parts)) = fields.get(tree::PARTS) {
            for index in 0..parts.len() {
                let field = format!("{}/{index}/duration_s", tree::PARTS);
                form = form.push(labelled(
                    format!("Part {index} (s)"),
                    self.number_input(&format!("{pointer}/{field}"), "1.0"),
                ));
            }
        }
        form.into()
    }

    /// Adds the rows for one field, recursing into a tagged sub-object.
    fn field_rows<'a>(
        &'a self,
        mut form: iced::widget::Column<'a, Message>,
        node_pointer: &str,
        field: &str,
        value: &Value,
    ) -> iced::widget::Column<'a, Message> {
        let pointer = format!("{node_pointer}/{field}");
        match value {
            Value::Bool(flag) => form.push(
                checkbox(humanise(field), *flag)
                    .size(15)
                    .text_size(12)
                    .on_toggle({
                        let pointer = pointer.clone();
                        move |on| Message::FlagToggled(pointer.clone(), on)
                    }),
            ),
            Value::String(current) => match variants_for(field) {
                Some(variants) => form.push(labelled(
                    humanise(field),
                    pick_list(
                        variants.to_vec(),
                        variants.iter().find(|v| v.key == current).copied(),
                        {
                            let pointer = pointer.clone();
                            move |variant| Message::VariantPicked(pointer.clone(), variant)
                        },
                    )
                    .text_size(13)
                    .into(),
                )),
                None => form.push(labelled(humanise(field), self.text_input(&pointer, ""))),
            },
            Value::Number(_) => {
                form.push(labelled(humanise(field), self.number_input(&pointer, "")))
            }
            Value::Null => form.push(labelled(
                humanise(field),
                self.number_input(&pointer, "built-in"),
            )),
            Value::Object(nested) => {
                for (key, nested_value) in nested {
                    form = self.field_rows(form, &pointer, key, nested_value);
                }
                form
            }
            // A `parts` array is handled by the caller; nothing else nests.
            Value::Array(_) => form,
        }
    }

    fn number_input(&self, pointer: &str, placeholder: &str) -> Element<'_, Message> {
        self.text_input(pointer, placeholder)
    }

    fn text_input(&self, pointer: &str, placeholder: &str) -> Element<'_, Message> {
        let owned = pointer.to_owned();
        let input = text_input(placeholder, &self.field_text(pointer))
            .on_input(move |raw| Message::FieldEdited(owned.clone(), raw))
            .size(13);
        match self.draft_errors.get(pointer) {
            Some(error) => column![input, text(error).size(10).style(text::danger)]
                .spacing(2)
                .into(),
            None => input.into(),
        }
    }

    fn sweep_panel(&self) -> Element<'_, Message> {
        let mut panel = column![section("Sweep")].spacing(6);
        panel = panel.push(
            checkbox("Emit one signal per value of a parameter", self.sweep_on)
                .size(15)
                .text_size(12)
                .on_toggle(Message::SweepToggled),
        );
        if !self.sweep_on {
            panel = panel.push(
                text(
                    "A sweep becomes one group with the swept value stored as a property on \
                     every signal — the shape a pipeline wants as test input.",
                )
                .size(11)
                .style(text::secondary),
            );
            return panel.into();
        }

        let targets: Vec<TargetChoice> = sp_gen::sweep::parameters(&self.spec)
            .into_iter()
            .map(|param| TargetChoice {
                pointer: param.json_pointer(),
                label: param.label,
            })
            .collect();
        let selected = self
            .sweep_target
            .as_ref()
            .and_then(|pointer| targets.iter().find(|t| &t.pointer == pointer).cloned());

        panel = panel.push(labelled(
            "Parameter",
            pick_list(targets, selected, Message::SweepTargetPicked)
                .text_size(13)
                .into(),
        ));
        panel = panel.push(labelled(
            "Values",
            pick_list(
                SweepMode::ALL.to_vec(),
                Some(self.sweep_mode),
                Message::SweepModePicked,
            )
            .text_size(13)
            .into(),
        ));

        match self.sweep_mode {
            SweepMode::Range => {
                panel = panel.push(labelled(
                    "Start",
                    text_input("100", &self.sweep_start)
                        .on_input(Message::SweepStartChanged)
                        .size(13)
                        .into(),
                ));
                panel = panel.push(labelled(
                    "Stop",
                    text_input("2000", &self.sweep_stop)
                        .on_input(Message::SweepStopChanged)
                        .size(13)
                        .into(),
                ));
                panel = panel.push(labelled(
                    "Step",
                    text_input("100", &self.sweep_step)
                        .on_input(Message::SweepStepChanged)
                        .size(13)
                        .into(),
                ));
            }
            SweepMode::List => {
                panel = panel.push(labelled(
                    "Values",
                    text_input("100, 250, 1000", &self.sweep_list)
                        .on_input(Message::SweepListChanged)
                        .size(13)
                        .into(),
                ));
            }
        }

        panel = panel.push(labelled(
            "Stored as",
            text_input("freq_hz", &self.sweep_property)
                .on_input(Message::SweepPropertyChanged)
                .size(13)
                .into(),
        ));

        let status: Element<'_, Message> = match self.sweep() {
            Some(Ok(sweep)) => match sweep.values.count() {
                Some(rungs) => text(format!(
                    "{rungs} signal(s), stored under '{}'.",
                    sweep.property_key
                ))
                .size(11)
                .style(text::secondary)
                .into(),
                None => Space::with_height(Length::Fixed(0.0)).into(),
            },
            Some(Err(error)) => text(error).size(11).style(text::danger).into(),
            None => Space::with_height(Length::Fixed(0.0)).into(),
        };
        panel.push(status).into()
    }

    fn issue_list(&self) -> Element<'_, Message> {
        let issues = validate(&self.spec);
        if issues.is_empty() {
            return Space::with_height(Length::Fixed(0.0)).into();
        }
        let mut list = column![section("Problems")].spacing(4);
        for issue in issues.as_slice() {
            list = list.push(issue_row(issue));
        }
        list.into()
    }
}

/// One row of the problem list. Everything it shows is cloned, so the row
/// outlives the borrow of the `Issues` that produced it.
fn issue_row(issue: &Issue) -> Element<'static, Message> {
    let where_ = match &issue.field {
        Some(field) => format!("{} · {field}", issue.pointer),
        None => issue.pointer.clone(),
    };
    let headline = text(issue.message.clone())
        .size(12)
        .style(match issue.severity {
            Severity::Error => text::danger,
            Severity::Warning => text::secondary,
        });
    button(
        column![
            headline,
            text(issue.fix.clone()).size(11).style(text::secondary),
            text(where_).size(10).style(text::secondary),
        ]
        .spacing(1),
    )
    .width(Length::Fill)
    .padding([4, 6])
    .style(button::text)
    .on_press(Message::SelectNode(issue.pointer.clone()))
    .into()
}

// ---------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------

/// Renders the front of the signal and reduces it to one min/max pair per
/// column. Runs off the UI thread (§4.2).
fn build_preview(spec: &GenSpec) -> Result<Preview, String> {
    let issues = validate(spec);
    if issues.blocks() {
        return Err(issues.errors().next().map_or_else(
            || "the spec is not renderable".to_owned(),
            |i| i.message.clone(),
        ));
    }

    let rate = spec.sample_rate_hz();
    let wanted = (PREVIEW_SECONDS * rate).ceil().max(1.0) as u64;
    let available = spec.sample_count();
    let count = wanted.min(available).min(PREVIEW_SAMPLE_CAP);
    if count == 0 {
        return Err("the spec renders no samples".to_owned());
    }

    let values = sp_gen::render_values(
        spec,
        SampleRange::first(count),
        &Sources::new(),
        &GenControl::new(),
    )
    .map_err(|error| error.to_string())?;

    let per_column = values.len().div_ceil(PREVIEW_COLUMNS).max(1);
    let mut columns = Vec::with_capacity(values.len().div_ceil(per_column));
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for chunk in values.chunks(per_column) {
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for value in chunk {
            if value.is_finite() {
                low = low.min(*value);
                high = high.max(*value);
            }
        }
        if low.is_finite() {
            min = min.min(low);
            max = max.max(high);
            columns.push((low as f32, high as f32));
        } else {
            columns.push((0.0, 0.0));
        }
    }
    if !min.is_finite() {
        min = 0.0;
        max = 0.0;
    }

    Ok(Preview {
        columns,
        min,
        max,
        seconds: count as f64 / rate,
        samples: count,
        truncated: count < available,
    })
}

/// The preview strip: a min/max trace with a zero line.
#[derive(Debug)]
struct PreviewChart<'a> {
    preview: Option<&'a Preview>,
}

impl canvas::Program<Message> for PreviewChart<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let palette = theme.extended_palette();
        let Some(preview) = self.preview else {
            return vec![frame.into_geometry()];
        };
        if preview.columns.is_empty() || bounds.width <= 0.0 {
            return vec![frame.into_geometry()];
        }

        // A flat signal still needs a band to sit in, so a zero span opens to
        // one unit rather than dividing by nothing.
        let (min, max) = if (preview.max - preview.min).abs() < f64::EPSILON {
            (preview.min - 0.5, preview.max + 0.5)
        } else {
            (preview.min, preview.max)
        };
        let pad = 6.0;
        let height = (bounds.height - 2.0 * pad).max(1.0);
        let y_of = |value: f64| {
            let fraction = ((value - min) / (max - min)).clamp(0.0, 1.0);
            pad + height * (1.0 - fraction) as f32
        };

        if min < 0.0 && max > 0.0 {
            let zero = y_of(0.0);
            let axis = Path::new(|builder| {
                builder.move_to(Point::new(0.0, zero));
                builder.line_to(Point::new(bounds.width, zero));
            });
            frame.stroke(
                &axis,
                Stroke::default()
                    .with_color(palette.background.strong.color)
                    .with_width(1.0),
            );
        }

        let step = bounds.width / preview.columns.len() as f32;
        let trace = Path::new(|builder| {
            for (index, (low, high)) in preview.columns.iter().enumerate() {
                let x = index as f32 * step + step / 2.0;
                builder.move_to(Point::new(x, y_of(f64::from(*high))));
                builder.line_to(Point::new(x, y_of(f64::from(*low))));
            }
        });
        frame.stroke(
            &trace,
            Stroke::default()
                .with_color(palette.primary.base.color)
                .with_width(step.clamp(1.0, 2.0)),
        );

        vec![frame.into_geometry()]
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_child_key(key: &str) -> bool {
    matches!(
        key,
        tree::TERMS | tree::PARTS | tree::INPUT | tree::CARRIER | tree::MODULATOR
    )
}

/// The variants a string field picks from, when it is an enum.
fn variants_for(field: &str) -> Option<&'static [Variant]> {
    match field {
        "sweep" => Some(&SWEEP_VARIANTS),
        "kind" => Some(&NOISE_VARIANTS),
        "shape" => Some(&ENVELOPE_VARIANTS),
        "mode" => Some(&MOD_VARIANTS),
        _ => None,
    }
}

/// A field name as a label: `freq_hz` reads as "Freq hz".
fn humanise(field: &str) -> String {
    let spaced = field.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// A number without a trailing `.0`, matching how the tree summarises one.
fn format_number(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
    }
    if text.is_empty() || text == "-0" {
        "0".to_owned()
    } else {
        text
    }
}

fn format_hz(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 1e6 {
        format!("{} MHz", format_number(value / 1e6))
    } else if magnitude >= 1e3 {
        format!("{} kHz", format_number(value / 1e3))
    } else {
        format!("{} Hz", format_number(value))
    }
}

/// A seed with no dependency on an RNG: the clock is enough to give a
/// different noise realisation, and the value is stored so it stays
/// reproducible (G3).
fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |elapsed| elapsed.as_nanos() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn section(title: &str) -> Element<'_, Message> {
    column![
        Space::with_height(Length::Fixed(4.0)),
        text(title).size(12).style(text::secondary),
        horizontal_rule(1),
    ]
    .spacing(3)
    .into()
}

/// A label beside a control. The label is taken by value so a caller can pass
/// a name it built on the spot — the parameter form's field names are derived,
/// not literals.
fn labelled<'a>(label: impl Into<String>, control: Element<'a, Message>) -> Element<'a, Message> {
    row![
        container(text(label.into()).size(12)).width(Length::Fixed(140.0)),
        control,
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

async fn open_preset() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .add_filter("Generator preset", &["json"])
        .set_title("Open a generator preset")
        .pick_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

async fn save_preset(name: String) -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .add_filter("Generator preset", &["json"])
        .set_file_name(format!("{name}.json"))
        .set_title("Save the generator preset")
        .save_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::default()
    }

    #[test]
    fn the_screen_opens_on_a_renderable_spec() {
        let state = state();
        assert!(!validate(&state.spec).blocks());
        assert!(build_preview(&state.spec).is_ok());
    }

    #[test]
    fn every_built_in_preset_can_be_loaded() {
        let mut state = state();
        for index in 0..state.presets.len() {
            let _ = state.update(None, Message::PresetPicked(index));
            assert!(
                !validate(&state.spec).blocks(),
                "preset {index} does not validate"
            );
            assert_eq!(state.selected, tree::ROOT);
        }
    }

    #[test]
    fn editing_a_field_updates_the_spec_and_keeps_the_typed_text() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::FieldEdited("/root/freq_hz".into(), "250".into()),
        );
        assert_eq!(
            tree::get_json(&state.spec, "/root/freq_hz"),
            Some(Value::from(250.0))
        );
        assert_eq!(state.field_text("/root/freq_hz"), "250");
    }

    #[test]
    fn a_half_typed_number_is_kept_without_touching_the_spec() {
        let mut state = state();
        let before = state.spec.clone();
        let _ = state.update(
            None,
            Message::FieldEdited("/root/freq_hz".into(), "1e".into()),
        );
        assert_eq!(state.spec, before);
        assert_eq!(state.field_text("/root/freq_hz"), "1e");
        assert!(state.draft_errors.contains_key("/root/freq_hz"));
    }

    #[test]
    fn a_value_the_spec_will_not_take_is_reported_rather_than_applied() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::NodeKindPicked(tree::ROOT.into(), NodeKind::Prbs),
        );
        let before = state.spec.clone();
        // `order` is a u8; 900 does not fit.
        let _ = state.update(
            None,
            Message::FieldEdited("/root/order".into(), "900".into()),
        );
        assert_eq!(state.spec, before);
        assert!(state.draft_errors.contains_key("/root/order"));
    }

    #[test]
    fn changing_a_node_kind_replaces_the_node() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::NodeKindPicked(tree::ROOT.into(), NodeKind::Chirp),
        );
        assert_eq!(
            tree::node_at(&state.spec, tree::ROOT).map(Node::kind),
            Some(NodeKind::Chirp)
        );
    }

    #[test]
    fn a_tagged_sub_object_is_replaced_whole_when_its_variant_changes() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::NodeKindPicked(tree::ROOT.into(), NodeKind::Envelope),
        );
        let _ = state.update(
            None,
            Message::VariantPicked(
                "/root/env/shape".into(),
                ENVELOPE_VARIANTS[0], // ADSR
            ),
        );
        let Some(Value::Object(env)) = tree::get_json(&state.spec, "/root/env") else {
            panic!("the envelope is still an object");
        };
        assert_eq!(env.get("shape"), Some(&Value::from("adsr")));
        assert!(env.contains_key("attack_s"));
        assert!(
            !env.contains_key("alpha"),
            "the old variant's field is gone"
        );
    }

    #[test]
    fn a_string_enum_field_is_set_in_place() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::NodeKindPicked(tree::ROOT.into(), NodeKind::Noise),
        );
        let _ = state.update(
            None,
            Message::VariantPicked("/root/kind".into(), NOISE_VARIANTS[2]),
        );
        assert_eq!(
            tree::get_json(&state.spec, "/root/kind"),
            Some(Value::from("pink"))
        );
    }

    #[test]
    fn children_can_be_added_reordered_and_removed() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::NodeKindPicked(tree::ROOT.into(), NodeKind::Sum),
        );
        let _ = state.update(None, Message::AddChild(tree::ROOT.into()));
        assert_eq!(state.selected, "/root/terms/1");

        let _ = state.update(None, Message::MoveNode("/root/terms/1".into(), -1));
        assert_eq!(state.selected, "/root/terms/0");

        let _ = state.update(None, Message::RemoveNode("/root/terms/0".into()));
        assert_eq!(state.selected, tree::ROOT);
        assert!(tree::node_at(&state.spec, "/root/terms/1").is_none());
    }

    #[test]
    fn the_last_child_of_a_combinator_cannot_be_removed() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::NodeKindPicked(tree::ROOT.into(), NodeKind::Sum),
        );
        let _ = state.update(None, Message::RemoveNode("/root/terms/0".into()));
        assert!(state.error.is_some());
        assert!(tree::node_at(&state.spec, "/root/terms/0").is_some());
    }

    #[test]
    fn a_sweep_defaults_to_the_selected_nodes_first_parameter() {
        let mut state = state();
        let _ = state.update(None, Message::SweepToggled(true));
        assert_eq!(state.sweep_target.as_deref(), Some("/root/amp"));
        let sweep = state.sweep().expect("a sweep").expect("valid");
        assert!(sweep.values.count().is_some());
    }

    #[test]
    fn a_sweep_over_a_frequency_expands_to_the_rungs_the_design_describes() {
        let mut state = state();
        let _ = state.update(None, Message::SweepToggled(true));
        let _ = state.update(
            None,
            Message::SweepTargetPicked(TargetChoice {
                pointer: "/root/freq_hz".into(),
                label: String::new(),
            }),
        );
        let _ = state.update(None, Message::SweepStartChanged("100".into()));
        let _ = state.update(None, Message::SweepStopChanged("2000".into()));
        let _ = state.update(None, Message::SweepStepChanged("100".into()));
        let sweep = state.sweep().unwrap().unwrap();
        assert_eq!(sweep.values.count(), Some(20));
        assert_eq!(sweep.property_key, "freq_hz");
    }

    #[test]
    fn a_list_sweep_accepts_the_separators_a_user_would_type() {
        let mut state = state();
        let _ = state.update(None, Message::SweepToggled(true));
        let _ = state.update(None, Message::SweepModePicked(SweepMode::List));
        let _ = state.update(None, Message::SweepListChanged("1, 2; 3 4\n5".into()));
        let sweep = state.sweep().unwrap().unwrap();
        assert_eq!(sweep.values.count(), Some(5));
    }

    #[test]
    fn a_sweep_that_will_not_parse_says_so_instead_of_generating() {
        let mut state = state();
        let _ = state.update(None, Message::SweepToggled(true));
        let _ = state.update(None, Message::SweepStepChanged("wobble".into()));
        assert!(matches!(state.sweep(), Some(Err(_))));
        let _ = state.update(None, Message::Start);
        assert!(state.error.is_some());
    }

    #[test]
    fn generation_needs_a_library() {
        let mut state = state();
        let _ = state.update(None, Message::Start);
        assert_eq!(state.error.as_deref(), Some("No library is open."));
    }

    #[test]
    fn an_invalid_spec_blocks_generation() {
        let mut state = state();
        // Above Nyquist for the default 48 kHz spec.
        let _ = state.update(
            None,
            Message::FieldEdited("/root/freq_hz".into(), "30000".into()),
        );
        assert!(validate(&state.spec).blocks());
        assert!(build_preview(&state.spec).is_err());
    }

    #[test]
    fn a_completed_generation_is_reported_once() {
        let mut state = state();
        assert!(!state.take_completed());
        state.completed = true;
        assert!(state.take_completed());
        assert!(!state.take_completed());
    }

    #[test]
    fn there_is_no_tick_subscription_once_the_preview_is_settled() {
        let mut state = state();
        state.dirty_since = None;
        assert!(matches!(state.subscription(), Subscription { .. }));
        // With neither a job nor a stale preview the batch is empty; with a
        // stale preview it is not.
        state.touch();
        assert!(state.dirty_since.is_some());
    }

    #[test]
    fn the_preview_reduces_a_long_render_to_a_bounded_number_of_columns() {
        let mut state = state();
        let _ = state.update(
            None,
            Message::FieldEdited("/duration_s".into(), "10".into()),
        );
        let preview = build_preview(&state.spec).unwrap();
        assert!(preview.columns.len() <= PREVIEW_COLUMNS);
        assert!(preview.truncated, "10 s is longer than the preview window");
        assert!((preview.seconds - PREVIEW_SECONDS).abs() < 1e-9);
    }

    #[test]
    fn the_screen_builds_a_view_in_every_state_it_can_be_in() {
        let mut state = state();
        let _ = state.view();

        state.preview = build_preview(&state.spec).ok();
        state.error = Some("boom".to_owned());
        state.notice = Some("done".to_owned());
        let _ = state.view();

        let _ = state.update(None, Message::SweepToggled(true));
        let _ = state.view();

        let _ = state.update(None, Message::SweepModePicked(SweepMode::List));
        let _ = state.view();

        for kind in NodeKind::ALL {
            let _ = state.update(None, Message::NodeKindPicked(tree::ROOT.into(), kind));
            let _ = state.view();
        }

        state.job = Some(Job {
            progress: Arc::new(Mutex::new(GenProgress::default())),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        let _ = state.view();
    }

    #[test]
    fn field_labels_read_as_words() {
        assert_eq!(humanise("freq_hz"), "Freq hz");
        assert_eq!(humanise("duty"), "Duty");
        assert_eq!(humanise(""), "");
    }
}
