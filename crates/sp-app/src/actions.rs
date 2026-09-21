//! The action catalogue: one list of everything the application can be *told
//! to do* (`docs/DESIGN.md` §12.5).
//!
//! Before M13 there was no such list. A command existed as a button in one
//! screen's `view` and, if it had a key, as an arm of a `match` in the root
//! subscription — so "every action" was not a thing the code could be asked
//! about, and neither a palette nor a configurable keyboard map could be
//! built without inventing the list twice.
//!
//! [`Action`] is that list. Each entry carries a stable `id` (what
//! `settings.json` binds a key to), a label (what the palette shows), the
//! screen it acts on, and the message it sends. The command palette
//! ([`crate::palette`]) and the keyboard map ([`crate::keymap`]) are both
//! readers of it, which is what makes *every action is reachable from the
//! palette* a test rather than a claim.
//!
//! **What is an action and what is not.** An action is a command that takes
//! no argument from the screen it is on: `Run the pipeline`, `Cancel the
//! import`, `Reclaim unused blobs`. Setting a control's *value* — the text in
//! a search box, which stage is selected, a parameter field, a picked
//! delimiter — is not an action and is not here; it has no name a user would
//! search for and no meaning without the thing it is setting. The line is
//! drawn there deliberately: a catalogue that included every
//! `ParamText(key, text)` would be a catalogue of nothing.

use crate::keymap::{Chord, Key};
use crate::screens::{
    generate, import, inspector, library, pipeline, properties, results, runs, scope, settings,
    Screen,
};
use crate::state::Message;

/// One command, named once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // ------------------------------------------------------------- global
    OpenPalette,
    ToggleTheme,
    Navigate(Screen),

    // ------------------------------------------------------------ library
    LibraryRefresh,
    LibrarySearch,
    LibraryClearSearch,

    // ------------------------------------------------------------- import
    ImportBrowse,
    ImportStart,
    ImportCancel,
    ImportClearQueue,
    ImportSweepWatched,

    // ----------------------------------------------------------- generate
    GenerateStart,
    GenerateCancel,
    GenerateRandomiseSeed,
    GenerateLoadPreset,
    GenerateSavePreset,

    // ----------------------------------------------------------- pipeline
    PipelineRun,
    PipelineCancel,
    PipelineSave,
    PipelineAddAssertion,
    PipelineAllGroups,
    PipelineNoGroups,

    // ------------------------------------------------------------ results
    ResultsRefresh,
    ResultsToggle,
    ResultsStop,
    ResultsStepBack,
    ResultsStepForward,
    ResultsToStart,
    ResultsToEnd,
    ResultsFitAll,
    ResultsUnpin,
    ResultsPromote,
    ResultsClearComparison,

    // --------------------------------------------------------------- runs
    RunsRefresh,
    RunsFailuresOnly,
    RunsEveryRun,
    RunsClearAgainst,

    // -------------------------------------------------------------- scope
    ScopeRefresh,
    ScopeToggle,
    ScopeStop,
    ScopeToStart,
    ScopeToEnd,
    ScopeLoopStart,
    ScopeLoopEnd,
    ScopeClearLoop,
    ScopeFitAll,
    ScopeFitAmplitude,

    // ---------------------------------------------------------- inspector
    InspectorRefresh,
    InspectorSaveProperties,

    // --------------------------------------------------------- properties
    PropertiesDeclare,

    // ----------------------------------------------------------- settings
    SettingsRefresh,
    SettingsReclaimBlobs,
    SettingsClearCache,
    SettingsChooseLibrary,
    SettingsUseDefaultLibrary,
    SettingsAddExternalLibrary,
    SettingsChooseWatchFolder,
    SettingsStopWatching,
    SettingsResetKeymap,
}

impl Action {
    /// Every action, in the order the palette lists them and the order a
    /// keyboard conflict resolves in: the global ones first, then screen by
    /// screen in rail order.
    pub const ALL: &'static [Self] = &[
        Self::OpenPalette,
        Self::ToggleTheme,
        Self::Navigate(Screen::Library),
        Self::Navigate(Screen::Inspector),
        Self::Navigate(Screen::Properties),
        Self::Navigate(Screen::Import),
        Self::Navigate(Screen::Generate),
        Self::Navigate(Screen::Pipeline),
        Self::Navigate(Screen::Results),
        Self::Navigate(Screen::Runs),
        Self::Navigate(Screen::Scope),
        Self::Navigate(Screen::Settings),
        Self::LibraryRefresh,
        Self::LibrarySearch,
        Self::LibraryClearSearch,
        Self::InspectorRefresh,
        Self::InspectorSaveProperties,
        Self::PropertiesDeclare,
        Self::ImportBrowse,
        Self::ImportStart,
        Self::ImportCancel,
        Self::ImportClearQueue,
        Self::ImportSweepWatched,
        Self::GenerateStart,
        Self::GenerateCancel,
        Self::GenerateRandomiseSeed,
        Self::GenerateLoadPreset,
        Self::GenerateSavePreset,
        Self::PipelineRun,
        Self::PipelineCancel,
        Self::PipelineSave,
        Self::PipelineAddAssertion,
        Self::PipelineAllGroups,
        Self::PipelineNoGroups,
        Self::ResultsRefresh,
        Self::ResultsToggle,
        Self::ResultsStop,
        Self::ResultsStepBack,
        Self::ResultsStepForward,
        Self::ResultsToStart,
        Self::ResultsToEnd,
        Self::ResultsFitAll,
        Self::ResultsUnpin,
        Self::ResultsPromote,
        Self::ResultsClearComparison,
        Self::RunsRefresh,
        Self::RunsFailuresOnly,
        Self::RunsEveryRun,
        Self::RunsClearAgainst,
        Self::ScopeRefresh,
        Self::ScopeToggle,
        Self::ScopeStop,
        Self::ScopeToStart,
        Self::ScopeToEnd,
        Self::ScopeLoopStart,
        Self::ScopeLoopEnd,
        Self::ScopeClearLoop,
        Self::ScopeFitAll,
        Self::ScopeFitAmplitude,
        Self::SettingsRefresh,
        Self::SettingsReclaimBlobs,
        Self::SettingsClearCache,
        Self::SettingsChooseLibrary,
        Self::SettingsUseDefaultLibrary,
        Self::SettingsAddExternalLibrary,
        Self::SettingsChooseWatchFolder,
        Self::SettingsStopWatching,
        Self::SettingsResetKeymap,
    ];

    /// Every action, by value.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied()
    }

    /// The name `settings.json` binds a key to.
    ///
    /// It is written down rather than derived from the variant, because a
    /// variant may be renamed and a user's binding may not.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenPalette => "palette.open",
            Self::ToggleTheme => "theme.toggle",
            Self::Navigate(screen) => match screen {
                Screen::Library => "nav.library",
                Screen::Inspector => "nav.inspector",
                Screen::Properties => "nav.properties",
                Screen::Import => "nav.import",
                Screen::Generate => "nav.generate",
                Screen::Pipeline => "nav.pipeline",
                Screen::Results => "nav.results",
                Screen::Runs => "nav.runs",
                Screen::Scope => "nav.scope",
                Screen::Settings => "nav.settings",
            },
            Self::LibraryRefresh => "library.refresh",
            Self::LibrarySearch => "library.search",
            Self::LibraryClearSearch => "library.clear_search",
            Self::ImportBrowse => "import.browse",
            Self::ImportStart => "import.start",
            Self::ImportCancel => "import.cancel",
            Self::ImportClearQueue => "import.clear_queue",
            Self::ImportSweepWatched => "import.sweep_watched",
            Self::GenerateStart => "generate.start",
            Self::GenerateCancel => "generate.cancel",
            Self::GenerateRandomiseSeed => "generate.randomise_seed",
            Self::GenerateLoadPreset => "generate.load_preset",
            Self::GenerateSavePreset => "generate.save_preset",
            Self::PipelineRun => "pipeline.run",
            Self::PipelineCancel => "pipeline.cancel",
            Self::PipelineSave => "pipeline.save",
            Self::PipelineAddAssertion => "pipeline.add_assertion",
            Self::PipelineAllGroups => "pipeline.all_groups",
            Self::PipelineNoGroups => "pipeline.no_groups",
            Self::ResultsRefresh => "results.refresh",
            Self::ResultsToggle => "results.play_pause",
            Self::ResultsStop => "results.stop",
            Self::ResultsStepBack => "results.step_back",
            Self::ResultsStepForward => "results.step_forward",
            Self::ResultsToStart => "results.to_start",
            Self::ResultsToEnd => "results.to_end",
            Self::ResultsFitAll => "results.fit_all",
            Self::ResultsUnpin => "results.unpin",
            Self::ResultsPromote => "results.promote",
            Self::ResultsClearComparison => "results.clear_comparison",
            Self::RunsRefresh => "runs.refresh",
            Self::RunsFailuresOnly => "runs.failures_only",
            Self::RunsEveryRun => "runs.every_run",
            Self::RunsClearAgainst => "runs.clear_against",
            Self::ScopeRefresh => "scope.refresh",
            Self::ScopeToggle => "scope.play_pause",
            Self::ScopeStop => "scope.stop",
            Self::ScopeToStart => "scope.to_start",
            Self::ScopeToEnd => "scope.to_end",
            Self::ScopeLoopStart => "scope.loop_start",
            Self::ScopeLoopEnd => "scope.loop_end",
            Self::ScopeClearLoop => "scope.clear_loop",
            Self::ScopeFitAll => "scope.fit_all",
            Self::ScopeFitAmplitude => "scope.fit_amplitude",
            Self::InspectorRefresh => "inspector.refresh",
            Self::InspectorSaveProperties => "inspector.save_properties",
            Self::PropertiesDeclare => "properties.declare",
            Self::SettingsRefresh => "settings.refresh",
            Self::SettingsReclaimBlobs => "settings.reclaim_blobs",
            Self::SettingsClearCache => "settings.clear_cache",
            Self::SettingsChooseLibrary => "settings.choose_library",
            Self::SettingsUseDefaultLibrary => "settings.use_default_library",
            Self::SettingsAddExternalLibrary => "settings.add_external_library",
            Self::SettingsChooseWatchFolder => "settings.choose_watch_folder",
            Self::SettingsStopWatching => "settings.stop_watching",
            Self::SettingsResetKeymap => "settings.reset_keymap",
        }
    }

    /// What the palette shows, and what it is searched by.
    ///
    /// A verb first, because the palette is a list of things to do: `Run the
    /// pipeline`, not `Pipeline: run`. Navigation is the exception — `Library`
    /// reads better than `Go to the Library` in a list of ten of them, and
    /// `go` still finds it through the id.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::OpenPalette => "Open the command palette".to_owned(),
            Self::ToggleTheme => "Toggle the light and dark theme".to_owned(),
            Self::Navigate(screen) => format!("Go to {}", screen.label()),
            Self::LibraryRefresh => "Reload the library tree".to_owned(),
            Self::LibrarySearch => "Run the library search".to_owned(),
            Self::LibraryClearSearch => "Clear the search and every filter".to_owned(),
            Self::ImportBrowse => "Choose files to import…".to_owned(),
            Self::ImportStart => "Start the import".to_owned(),
            Self::ImportCancel => "Cancel the import".to_owned(),
            Self::ImportClearQueue => "Clear the import queue".to_owned(),
            Self::ImportSweepWatched => "Import every file in the watched folder".to_owned(),
            Self::GenerateStart => "Generate into the library".to_owned(),
            Self::GenerateCancel => "Cancel the generation".to_owned(),
            Self::GenerateRandomiseSeed => "Randomise the generator seed".to_owned(),
            Self::GenerateLoadPreset => "Load a generator spec from a file…".to_owned(),
            Self::GenerateSavePreset => "Save the generator spec to a file…".to_owned(),
            Self::PipelineRun => "Run the pipeline".to_owned(),
            Self::PipelineCancel => "Cancel the run".to_owned(),
            Self::PipelineSave => "Save the pipeline".to_owned(),
            Self::PipelineAddAssertion => "Add an assertion".to_owned(),
            Self::PipelineAllGroups => "Run over every group".to_owned(),
            Self::PipelineNoGroups => "Run over no groups".to_owned(),
            Self::ResultsRefresh => "Reload the run list".to_owned(),
            Self::ResultsToggle => "Play or pause the results playhead".to_owned(),
            Self::ResultsStop => "Stop the results playhead".to_owned(),
            Self::ResultsStepBack => "Step back one stage".to_owned(),
            Self::ResultsStepForward => "Step forward one stage".to_owned(),
            Self::ResultsToStart => "Jump the results playhead to the start".to_owned(),
            Self::ResultsToEnd => "Jump the results playhead to the end".to_owned(),
            Self::ResultsFitAll => "Fit every result trace".to_owned(),
            Self::ResultsUnpin => "Unpin the compared stage".to_owned(),
            Self::ResultsPromote => "Promote this run to a baseline".to_owned(),
            Self::ResultsClearComparison => "Clear the run comparison".to_owned(),
            Self::RunsRefresh => "Reload the run history".to_owned(),
            Self::RunsFailuresOnly => "Show only failing runs".to_owned(),
            Self::RunsEveryRun => "Show every run".to_owned(),
            Self::RunsClearAgainst => "Clear the run marked for diffing".to_owned(),
            Self::ScopeRefresh => "Reload the signal list".to_owned(),
            Self::ScopeToggle => "Play or pause".to_owned(),
            Self::ScopeStop => "Stop playback".to_owned(),
            Self::ScopeToStart => "Jump the playhead to the start".to_owned(),
            Self::ScopeToEnd => "Jump the playhead to the end".to_owned(),
            Self::ScopeLoopStart => "Set the loop start to the playhead".to_owned(),
            Self::ScopeLoopEnd => "Set the loop end to the playhead".to_owned(),
            Self::ScopeClearLoop => "Clear the loop region".to_owned(),
            Self::ScopeFitAll => "Fit every trace".to_owned(),
            Self::ScopeFitAmplitude => "Fit the amplitude".to_owned(),
            Self::InspectorRefresh => "Reload what the Inspector is showing".to_owned(),
            Self::InspectorSaveProperties => "Save the edited properties".to_owned(),
            Self::PropertiesDeclare => "Declare the property definition".to_owned(),
            Self::SettingsRefresh => "Re-read what the library is made of".to_owned(),
            Self::SettingsReclaimBlobs => "Reclaim unused blobs".to_owned(),
            Self::SettingsClearCache => "Clear the stage cache".to_owned(),
            Self::SettingsChooseLibrary => "Open another library…".to_owned(),
            Self::SettingsUseDefaultLibrary => "Open the default library".to_owned(),
            Self::SettingsAddExternalLibrary => "Allow an external stage library…".to_owned(),
            Self::SettingsChooseWatchFolder => "Watch a folder for new files…".to_owned(),
            Self::SettingsStopWatching => "Stop watching the folder".to_owned(),
            Self::SettingsResetKeymap => "Reset every keyboard shortcut".to_owned(),
        }
    }

    /// The screen this action acts on, or `None` when it acts on the whole
    /// application.
    ///
    /// It is two things at once: the scope a keyboard binding fires in, and
    /// where the palette takes the user before running the action. Both follow
    /// from the same fact — `Space` means *play* because the Scope screen is
    /// showing, and running `Play or pause` from the Library screen can only
    /// sensibly mean *go and play*.
    #[must_use]
    pub const fn screen(self) -> Option<Screen> {
        match self {
            Self::OpenPalette | Self::ToggleTheme | Self::Navigate(_) => None,
            Self::LibraryRefresh | Self::LibrarySearch | Self::LibraryClearSearch => {
                Some(Screen::Library)
            }
            Self::ImportBrowse
            | Self::ImportStart
            | Self::ImportCancel
            | Self::ImportClearQueue
            | Self::ImportSweepWatched => Some(Screen::Import),
            Self::GenerateStart
            | Self::GenerateCancel
            | Self::GenerateRandomiseSeed
            | Self::GenerateLoadPreset
            | Self::GenerateSavePreset => Some(Screen::Generate),
            Self::PipelineRun
            | Self::PipelineCancel
            | Self::PipelineSave
            | Self::PipelineAddAssertion
            | Self::PipelineAllGroups
            | Self::PipelineNoGroups => Some(Screen::Pipeline),
            Self::ResultsRefresh
            | Self::ResultsToggle
            | Self::ResultsStop
            | Self::ResultsStepBack
            | Self::ResultsStepForward
            | Self::ResultsToStart
            | Self::ResultsToEnd
            | Self::ResultsFitAll
            | Self::ResultsUnpin
            | Self::ResultsPromote
            | Self::ResultsClearComparison => Some(Screen::Results),
            Self::RunsRefresh
            | Self::RunsFailuresOnly
            | Self::RunsEveryRun
            | Self::RunsClearAgainst => Some(Screen::Runs),
            Self::ScopeRefresh
            | Self::ScopeToggle
            | Self::ScopeStop
            | Self::ScopeToStart
            | Self::ScopeToEnd
            | Self::ScopeLoopStart
            | Self::ScopeLoopEnd
            | Self::ScopeClearLoop
            | Self::ScopeFitAll
            | Self::ScopeFitAmplitude => Some(Screen::Scope),
            Self::InspectorRefresh | Self::InspectorSaveProperties => Some(Screen::Inspector),
            Self::PropertiesDeclare => Some(Screen::Properties),
            Self::SettingsRefresh
            | Self::SettingsReclaimBlobs
            | Self::SettingsClearCache
            | Self::SettingsChooseLibrary
            | Self::SettingsUseDefaultLibrary
            | Self::SettingsAddExternalLibrary
            | Self::SettingsChooseWatchFolder
            | Self::SettingsStopWatching
            | Self::SettingsResetKeymap => Some(Screen::Settings),
        }
    }

    /// Where this action sits in the Settings screen's shortcut list and in
    /// the palette's right-hand column: the screen's name, or `Global`.
    #[must_use]
    pub const fn group(self) -> &'static str {
        match self.screen() {
            None => "Global",
            Some(screen) => screen.label(),
        }
    }

    /// The chord bound to this action out of the box.
    ///
    /// Everything that had a key in v1 keeps exactly the key it had — the
    /// `Ctrl`+digit rail, `Ctrl`+`T`, the transport keys and the stage rail's
    /// arrows — plus `Ctrl`+`K` for the palette, which is the one new default.
    /// Every other action is unbound and reached from the palette: a shipped
    /// map that claims fifty chords is a map the user has to fight.
    #[must_use]
    pub fn default_chord(self) -> Option<Chord> {
        Some(match self {
            Self::OpenPalette => Chord::ctrl(Key::Char('k')),
            Self::ToggleTheme => Chord::ctrl(Key::Char('t')),
            Self::Navigate(screen) => Chord::ctrl(Key::Char(screen.shortcut()?)),
            Self::ScopeToggle | Self::ResultsToggle => Chord::plain(Key::Space),
            Self::ScopeToStart | Self::ResultsToStart => Chord::plain(Key::Home),
            Self::ScopeToEnd | Self::ResultsToEnd => Chord::plain(Key::End),
            Self::ScopeLoopStart => Chord::plain(Key::Char('[')),
            Self::ScopeLoopEnd => Chord::plain(Key::Char(']')),
            Self::ResultsStepBack => Chord::plain(Key::Left),
            Self::ResultsStepForward => Chord::plain(Key::Right),
            _ => return None,
        })
    }

    /// The message this action sends.
    ///
    /// The root runs it through its own `update`, after navigating to the
    /// action's screen, so an action is dispatched by exactly the same path
    /// whether it came from a button, a key or the palette.
    #[must_use]
    pub fn message(self) -> Message {
        match self {
            Self::OpenPalette => Message::OpenPalette,
            Self::ToggleTheme => Message::ToggleTheme,
            Self::Navigate(screen) => Message::Nav(screen),

            Self::LibraryRefresh => Message::Library(library::Message::Refresh),
            Self::LibrarySearch => Message::Library(library::Message::Search),
            Self::LibraryClearSearch => Message::Library(library::Message::ClearSearch),

            Self::ImportBrowse => Message::Import(import::Message::Browse),
            Self::ImportStart => Message::Import(import::Message::Start),
            Self::ImportCancel => Message::Import(import::Message::Cancel),
            Self::ImportClearQueue => Message::Import(import::Message::ClearQueue),
            Self::ImportSweepWatched => Message::SweepWatchedFolder,

            Self::GenerateStart => Message::Generate(generate::Message::Start),
            Self::GenerateCancel => Message::Generate(generate::Message::Cancel),
            Self::GenerateRandomiseSeed => Message::Generate(generate::Message::RandomiseSeed),
            Self::GenerateLoadPreset => Message::Generate(generate::Message::BrowsePreset),
            Self::GenerateSavePreset => Message::Generate(generate::Message::SavePreset),

            Self::PipelineRun => Message::Pipeline(pipeline::Message::Run),
            Self::PipelineCancel => Message::Pipeline(pipeline::Message::Cancel),
            Self::PipelineSave => Message::Pipeline(pipeline::Message::Save),
            Self::PipelineAddAssertion => Message::Pipeline(pipeline::Message::AddAssertion),
            Self::PipelineAllGroups => Message::Pipeline(pipeline::Message::AllGroups(true)),
            Self::PipelineNoGroups => Message::Pipeline(pipeline::Message::AllGroups(false)),

            Self::ResultsRefresh => Message::Results(results::Message::Refresh),
            Self::ResultsToggle => Message::Results(results::Message::Toggle),
            Self::ResultsStop => Message::Results(results::Message::Stop),
            Self::ResultsStepBack => Message::Results(results::Message::StepStage(-1)),
            Self::ResultsStepForward => Message::Results(results::Message::StepStage(1)),
            Self::ResultsToStart => Message::Results(results::Message::SeekFraction(0.0)),
            Self::ResultsToEnd => Message::Results(results::Message::SeekFraction(1.0)),
            Self::ResultsFitAll => Message::Results(results::Message::FitAll),
            Self::ResultsUnpin => Message::Results(results::Message::Unpin),
            Self::ResultsPromote => Message::Results(results::Message::Promote),
            Self::ResultsClearComparison => Message::Results(results::Message::ClearComparison),

            Self::RunsRefresh => Message::Runs(runs::Message::Refresh),
            Self::RunsFailuresOnly => Message::Runs(runs::Message::ToggleFailuresOnly(true)),
            Self::RunsEveryRun => Message::Runs(runs::Message::ToggleFailuresOnly(false)),
            Self::RunsClearAgainst => Message::Runs(runs::Message::ClearAgainst),

            Self::ScopeRefresh => Message::Scope(scope::Message::Refresh),
            Self::ScopeToggle => Message::Scope(scope::Message::Toggle),
            Self::ScopeStop => Message::Scope(scope::Message::Stop),
            Self::ScopeToStart => Message::Scope(scope::Message::SeekFraction(0.0)),
            Self::ScopeToEnd => Message::Scope(scope::Message::SeekFraction(1.0)),
            Self::ScopeLoopStart => Message::Scope(scope::Message::SetLoopStart),
            Self::ScopeLoopEnd => Message::Scope(scope::Message::SetLoopEnd),
            Self::ScopeClearLoop => Message::Scope(scope::Message::ClearLoop),
            Self::ScopeFitAll => Message::Scope(scope::Message::FitAll),
            Self::ScopeFitAmplitude => Message::Scope(scope::Message::FitAmplitude),

            Self::InspectorRefresh => Message::Inspector(inspector::Message::Refresh),
            Self::InspectorSaveProperties => Message::Inspector(inspector::Message::SaveProperties),

            Self::PropertiesDeclare => Message::Properties(properties::Message::Submit),

            Self::SettingsRefresh => Message::Settings(settings::Message::Refresh),
            Self::SettingsReclaimBlobs => Message::Settings(settings::Message::Sweep),
            Self::SettingsClearCache => Message::Settings(settings::Message::ClearCache),
            Self::SettingsChooseLibrary => Message::Settings(settings::Message::ChooseLibrary),
            Self::SettingsUseDefaultLibrary => {
                Message::Settings(settings::Message::UseDefaultLibrary)
            }
            Self::SettingsAddExternalLibrary => {
                Message::Settings(settings::Message::AddExternalLibrary)
            }
            Self::SettingsChooseWatchFolder => {
                Message::Settings(settings::Message::ChooseWatchFolder)
            }
            Self::SettingsStopWatching => Message::Settings(settings::Message::StopWatching),
            Self::SettingsResetKeymap => Message::Settings(settings::Message::ResetKeymap),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_action_has_an_id_and_a_label_of_its_own() {
        let mut ids = BTreeSet::new();
        let mut labels = BTreeSet::new();
        for action in Action::all() {
            assert!(
                ids.insert(action.id()),
                "{} is used by two actions",
                action.id()
            );
            assert!(
                labels.insert(action.label()),
                "'{}' labels two actions",
                action.label()
            );
            assert!(
                action.id().contains('.'),
                "{} should be scope-qualified",
                action.id()
            );
            assert!(!action.label().is_empty());
        }
        assert_eq!(ids.len(), Action::ALL.len());
    }

    /// The catalogue is what the palette lists and what the shortcut editor
    /// edits, so an action that is in neither list because it was left out of
    /// `ALL` is the one bug this file can have.
    #[test]
    fn every_screen_has_at_least_one_action_and_the_globals_have_no_screen() {
        for screen in Screen::ALL {
            assert!(
                Action::all().any(|action| action.screen() == Some(screen)),
                "{screen:?} has no action"
            );
        }
        let globals: Vec<Action> = Action::all().filter(|a| a.screen().is_none()).collect();
        assert_eq!(
            globals.len(),
            12,
            "the palette, the theme and the ten screens"
        );
    }

    #[test]
    fn the_default_keys_are_the_ones_v1_had() {
        // Every screen still has its Ctrl+digit …
        for screen in Screen::ALL {
            let chord = Action::Navigate(screen).default_chord().unwrap();
            assert!(chord.ctrl);
            assert_eq!(chord.key, Key::Char(screen.shortcut().unwrap()));
        }
        // … and no chord is claimed twice inside one scope.
        let mut seen: BTreeSet<(Option<Screen>, Chord)> = BTreeSet::new();
        for action in Action::all() {
            if let Some(chord) = action.default_chord() {
                assert!(
                    seen.insert((action.screen(), chord)),
                    "{} claims {} twice in its scope",
                    action.id(),
                    chord.label()
                );
            }
        }
    }

    /// The transport keys were a `match` in the root subscription and are now
    /// bindings, which is the §11.4 note about `[` and `]` finally answered.
    #[test]
    fn the_transport_keys_are_bindings_now() {
        assert_eq!(
            Action::ScopeLoopStart.default_chord(),
            Some(Chord::plain(Key::Char('[')))
        );
        assert_eq!(
            Action::ScopeLoopEnd.default_chord(),
            Some(Chord::plain(Key::Char(']')))
        );
        assert_eq!(
            Action::ScopeToggle.default_chord(),
            Some(Chord::plain(Key::Space))
        );
        assert_eq!(
            Action::ResultsStepForward.default_chord(),
            Some(Chord::plain(Key::Right))
        );
    }

    #[test]
    fn an_action_is_grouped_by_the_screen_it_acts_on() {
        assert_eq!(Action::OpenPalette.group(), "Global");
        assert_eq!(Action::PipelineRun.group(), "Pipeline");
        assert_eq!(Action::Navigate(Screen::Scope).group(), "Global");
    }
}
