//! The headless command line (`docs/DESIGN.md` §10.4, M7).
//!
//! The same binary is a window and a test runner. With no arguments it opens
//! the GUI; with a subcommand it runs to completion on the terminal and exits
//! with a status a CI job can read:
//!
//! ```text
//! signalplayback run --library lib.db --pipeline "detector" \
//!                    --dataset "impairment ladder" --assert-baseline golden
//! ```
//!
//! | Exit | Meaning |
//! |------|---------|
//! | 0 | the run finished and everything it was asked to check passed |
//! | 1 | the request itself was wrong — bad arguments, no such pipeline, an unreadable library |
//! | 2 | the run finished and *failed*: a stage error, a failed assertion, or a deviation from the baseline |
//!
//! Separating 1 from 2 is what lets a pipeline distinguish "the harness is
//! broken" from "the algorithm regressed", which is the whole point of running
//! it in CI (G8).
//!
//! Everything the CLI does is a call into the same crates the GUI uses — no
//! headless-only paths, so a green CI run and a green window mean the same
//! thing.

use std::fmt::Write as _;
use std::path::PathBuf;

use sp_core::run::RunStatus;
use sp_core::{DatasetId, GroupId, RunId, Tolerances};
use sp_proc::compare::{self, BaselineReport};
use sp_proc::pipeline::Pipeline;
use sp_proc::scheduler::{run_pipeline, RunControl, RunOptions, RunSummary};
use sp_store::regress::{self, BaselineRow};
use sp_store::runs::{self, PipelineRow};
use sp_store::{library, Store};

/// What the process should do with the arguments it was given.
#[derive(Debug, PartialEq)]
pub enum Invocation {
    /// No subcommand: open the window.
    Gui,
    /// A subcommand ran; exit with this code.
    Exited(i32),
}

/// Exit codes, named where they are decided.
const OK: i32 = 0;
const USAGE: i32 = 1;
const FAILED: i32 = 2;

const HELP: &str = "\
SignalPlayback — a workbench for time-domain signals.

Usage:
  signalplayback                       open the application window
  signalplayback run [options]         run a pipeline headlessly
  signalplayback baselines [options]   list the library's baselines
  signalplayback promote [options]     promote a run to a named baseline
  signalplayback help                  show this text

Options for `run`:
  --library <path>        library file (default: the application's own)
  --pipeline <name|#id>   the saved pipeline to run (required)
  --dataset <name|#id>    run over every group of this dataset
  --groups <id,id,...>    run over these groups instead
  --baseline <name>       resolve `baseline` in assertions against this baseline
  --assert-baseline <n>   compare the finished run against baseline <n> and
                          fail if it deviates (implies --baseline)
  --promote <name>        on success, promote the run to this baseline
  --tolerance-sample-abs <x>, --tolerance-sample-rel <x>,
  --tolerance-metric-abs <x>, --tolerance-metric-rel <x>,
  --tolerance-artifact-abs <x>   tolerances stored with --promote
  --no-cache              recompute every stage rather than reusing output
  --notes <text>          recorded on the run
  --quiet                 print only the final verdict

Options for `baselines` and `promote`:
  --library <path>        library file
  --run <id>              the run to promote (promote only; required)
  --promote <name>        the baseline name to promote it to (promote only)

Exit codes: 0 passed, 1 the request was wrong, 2 the run failed.
";

/// Reads the process arguments and, when they name a subcommand, carries it
/// out. `Invocation::Gui` means there was no subcommand and the window should
/// open as usual.
///
/// Output goes to stdout and stderr rather than the log: a CI job reads the
/// terminal. On Windows a release build is linked as a GUI application and so
/// has no console of its own, but a redirected or piped stdout — which is what
/// a CI job gives it — is inherited and works.
pub fn dispatch(args: &[String]) -> Invocation {
    let Some(command) = args.first() else {
        return Invocation::Gui;
    };
    match command.as_str() {
        "run" => Invocation::Exited(run_command(&args[1..])),
        "baselines" => Invocation::Exited(baselines_command(&args[1..])),
        "promote" => Invocation::Exited(promote_command(&args[1..])),
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Invocation::Exited(OK)
        }
        "--version" | "-V" => {
            println!("signalplayback {}", env!("CARGO_PKG_VERSION"));
            Invocation::Exited(OK)
        }
        other => {
            eprintln!("signalplayback: '{other}' is not a command\n");
            print!("{HELP}");
            Invocation::Exited(USAGE)
        }
    }
}

/// `signalplayback run`.
fn run_command(args: &[String]) -> i32 {
    let options = match Args::parse(args) {
        Ok(options) => options,
        Err(error) => return usage(&error),
    };
    match run_headless(&options) {
        Ok(code) => code,
        Err(error) => usage(&error),
    }
}

/// `signalplayback baselines`.
fn baselines_command(args: &[String]) -> i32 {
    let options = match Args::parse(args) {
        Ok(options) => options,
        Err(error) => return usage(&error),
    };
    let store = match open(&options.library) {
        Ok(store) => store,
        Err(error) => return usage(&error),
    };
    match store.read(regress::list_baselines) {
        Ok(baselines) if baselines.is_empty() => {
            println!("no baselines");
            OK
        }
        Ok(baselines) => {
            for baseline in baselines {
                println!("{}", describe_baseline(&baseline));
            }
            OK
        }
        Err(error) => usage(&error.to_string()),
    }
}

/// `signalplayback promote`.
fn promote_command(args: &[String]) -> i32 {
    let options = match Args::parse(args) {
        Ok(options) => options,
        Err(error) => return usage(&error),
    };
    let (Some(run), Some(name)) = (options.run, options.promote.clone()) else {
        return usage("promote needs --run <id> and --promote <name>");
    };
    let store = match open(&options.library) {
        Ok(store) => store,
        Err(error) => return usage(&error),
    };
    let tolerances = options.tolerances;
    match store.write(move |conn| regress::promote(conn, &name, RunId::new(run), &tolerances)) {
        Ok(_) => {
            println!("promoted run {run} to '{}'", options.promote.unwrap());
            OK
        }
        Err(error) => usage(&error.to_string()),
    }
}

fn usage(message: &str) -> i32 {
    eprintln!("signalplayback: {message}");
    USAGE
}

/// Runs the pipeline and reports. The two failure kinds stay apart: anything
/// wrong with the *request* comes back as `Err` (exit 1), while a run that
/// happened and failed returns [`FAILED`].
fn run_headless(options: &Args) -> Result<i32, String> {
    let store = open(&options.library)?;
    let registry = sp_dsp::registry().map_err(|error| error.to_string())?;

    let name = options
        .pipeline
        .clone()
        .ok_or("run needs --pipeline <name|#id>")?;
    let (id, pipeline) = load_pipeline(&store, &name)?;
    pipeline
        .validate(&registry)
        .map_err(|error| error.to_string())?;

    let groups = resolve_groups(&store, options)?;
    if groups.is_empty() {
        return Err("the chosen dataset has no groups".to_owned());
    }

    // `--assert-baseline` also gives the assertions something to resolve
    // `baseline` against: checking a run against a baseline and testing
    // individual values against it are the same intent.
    let baseline_name = options
        .baseline
        .as_ref()
        .or(options.assert_baseline.as_ref());
    let baseline = match baseline_name {
        Some(name) => {
            let name = name.clone();
            Some(
                store
                    .read(move |conn| regress::get_baseline_by_name(conn, &name))
                    .map_err(|error| error.to_string())?,
            )
        }
        None => None,
    };

    let mut run_options = RunOptions {
        dataset_id: options.dataset_id,
        use_cache: !options.no_cache,
        notes: options.notes.clone(),
        ..RunOptions::default()
    };
    run_options.baseline_run = baseline.as_ref().map(|row| row.run_id);

    if !options.quiet {
        println!(
            "running '{}' over {} group(s)…",
            pipeline.name,
            groups.len()
        );
    }
    let summary = run_pipeline(
        &store,
        &registry,
        id,
        &pipeline,
        &groups,
        &run_options,
        &RunControl::new(),
    )
    .map_err(|error| error.to_string())?;

    let mut failed = summary.status != RunStatus::Ok;
    println!("{}", report(&store, &summary));

    if let Some(name) = &options.assert_baseline {
        let report = compare::check_baseline(&store, summary.run, name)
            .map_err(|error| error.to_string())?;
        for line in report.lines() {
            println!("{line}");
        }
        failed |= !report.passed();
        log_report(&report);
    }

    if failed {
        return Ok(FAILED);
    }

    if let Some(name) = &options.promote {
        let owned = name.clone();
        let run = summary.run;
        let tolerances = options.tolerances;
        store
            .write(move |conn| regress::promote(conn, &owned, run, &tolerances).map(|_| ()))
            .map_err(|error| error.to_string())?;
        println!("promoted run {} to '{}'", summary.run.get(), name);
    }
    Ok(OK)
}

fn log_report(report: &BaselineReport) {
    if report.passed() {
        tracing::info!(baseline = report.baseline, "the run matches its baseline");
    } else {
        tracing::warn!(
            baseline = report.baseline,
            deviations = report.diff.deviations().len(),
            "the run deviates from its baseline"
        );
    }
}

/// The run's outcome, plus the assertion failures that explain it.
fn report(store: &Store, summary: &RunSummary) -> String {
    let mut text = summary.describe();
    let run = summary.run;
    let Ok(assertions) = store.read(move |conn| regress::run_assertions(conn, run)) else {
        return text;
    };
    for row in assertions.iter().filter(|row| row.status.is_failure()) {
        let _ = write!(text, "\n  group {}: {}", row.group_id.get(), row.describe());
    }
    text
}

fn describe_baseline(baseline: &BaselineRow) -> String {
    let tolerances = if baseline.tolerances.is_exact() {
        "exact".to_owned()
    } else {
        format!("{:?}", baseline.tolerances)
    };
    format!(
        "{}  run {}  {}",
        baseline.name,
        baseline.run_id.get(),
        tolerances
    )
}

fn open(path: &Option<PathBuf>) -> Result<Store, String> {
    let path = path
        .clone()
        .or_else(crate::paths::default_library_file)
        .ok_or("no library was given and there is no default location")?;
    if !path.exists() {
        return Err(format!("no library at {}", path.display()));
    }
    Store::open(&path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Finds a pipeline by name or by `#id`, and rebuilds it with its assertions.
fn load_pipeline(store: &Store, wanted: &str) -> Result<(sp_core::PipelineId, Pipeline), String> {
    let wanted = wanted.trim().to_owned();
    let rows: Vec<PipelineRow> = store
        .read(runs::list_pipelines)
        .map_err(|error| error.to_string())?;

    let chosen = match wanted
        .strip_prefix('#')
        .and_then(|id| id.parse::<i64>().ok())
    {
        Some(id) => rows.iter().find(|row| row.id.get() == id),
        None => rows.iter().find(|row| row.name == wanted),
    }
    .ok_or_else(|| {
        let known = rows
            .iter()
            .map(|row| format!("'{}' (#{})", row.name, row.id.get()))
            .collect::<Vec<_>>()
            .join(", ");
        if known.is_empty() {
            format!("no pipeline called '{wanted}'; the library has none")
        } else {
            format!("no pipeline called '{wanted}'; the library has {known}")
        }
    })?;

    let id = chosen.id;
    let name = chosen.name.clone();
    let (stages, assertions) = store
        .read(move |conn| {
            Ok((
                runs::pipeline_stages(conn, id)?,
                regress::pipeline_assertions(conn, id)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let pipeline = Pipeline::from_rows(name, &stages)
        .map_err(|error| format!("the saved pipeline would not load: {error}"))?
        .with_assertion_rows(&assertions);
    Ok((id, pipeline))
}

/// The groups to run over: those named, else every group of the chosen
/// dataset, else — when the library holds exactly one dataset — that one.
fn resolve_groups(store: &Store, options: &Args) -> Result<Vec<GroupId>, String> {
    if !options.groups.is_empty() {
        return Ok(options.groups.iter().map(|id| GroupId::new(*id)).collect());
    }

    let datasets = store
        .read(library::list_datasets)
        .map_err(|error| error.to_string())?;
    let chosen = match &options.dataset {
        Some(wanted) => {
            let wanted = wanted.trim();
            match wanted
                .strip_prefix('#')
                .and_then(|id| id.parse::<i64>().ok())
            {
                Some(id) => datasets.iter().find(|row| row.id.get() == id),
                None => datasets.iter().find(|row| row.name == wanted),
            }
            .ok_or_else(|| format!("no dataset called '{wanted}'"))?
        }
        // One dataset is unambiguous; more than one has to be chosen, because
        // running the wrong corpus would look like a pass.
        None => match datasets.as_slice() {
            [only] => only,
            [] => return Err("the library has no datasets".to_owned()),
            many => {
                let names = many
                    .iter()
                    .map(|row| format!("'{}'", row.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "the library has {} datasets ({names}); name one with --dataset",
                    many.len()
                ));
            }
        },
    };

    let id = chosen.id;
    let groups = store
        .read(move |conn| library::list_groups_in_dataset(conn, id))
        .map_err(|error| error.to_string())?;
    Ok(groups.into_iter().map(|group| group.id).collect())
}

/// The parsed command line.
///
/// Hand-rolled rather than pulled from a crate: the surface is a handful of
/// long options, and every one of them is spelled out in [`HELP`].
#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub library: Option<PathBuf>,
    pub pipeline: Option<String>,
    pub dataset: Option<String>,
    pub dataset_id: Option<DatasetId>,
    pub groups: Vec<i64>,
    pub baseline: Option<String>,
    pub assert_baseline: Option<String>,
    pub promote: Option<String>,
    pub run: Option<i64>,
    pub tolerances: Tolerances,
    pub no_cache: bool,
    pub quiet: bool,
    pub notes: Option<String>,
}

impl Args {
    /// Parses the options of a subcommand.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            let mut value = || {
                rest.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--library" => parsed.library = Some(PathBuf::from(value()?)),
                "--pipeline" => parsed.pipeline = Some(value()?),
                "--dataset" => parsed.dataset = Some(value()?),
                "--groups" => parsed.groups = number_list(&value()?)?,
                "--baseline" => parsed.baseline = Some(value()?),
                "--assert-baseline" => parsed.assert_baseline = Some(value()?),
                "--promote" => parsed.promote = Some(value()?),
                "--run" => parsed.run = Some(number(&value()?)?),
                "--notes" => parsed.notes = Some(value()?),
                "--tolerance-sample-abs" => parsed.tolerances.sample_abs = float(&value()?)?,
                "--tolerance-sample-rel" => parsed.tolerances.sample_rel = float(&value()?)?,
                "--tolerance-metric-abs" => parsed.tolerances.metric_abs = float(&value()?)?,
                "--tolerance-metric-rel" => parsed.tolerances.metric_rel = float(&value()?)?,
                "--tolerance-artifact-abs" => parsed.tolerances.artifact_abs = float(&value()?)?,
                "--allow-new-groups" => parsed.tolerances.allow_new_groups = true,
                "--no-cache" => parsed.no_cache = true,
                "--quiet" => parsed.quiet = true,
                other => return Err(format!("unknown option '{other}'")),
            }
        }
        Ok(parsed)
    }
}

fn number(text: &str) -> Result<i64, String> {
    text.trim()
        .trim_start_matches('#')
        .parse()
        .map_err(|_| format!("'{text}' is not a number"))
}

fn float(text: &str) -> Result<f64, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("'{text}' is not a number"))
}

fn number_list(text: &str) -> Result<Vec<i64>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(number)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn no_arguments_opens_the_window() {
        assert_eq!(dispatch(&[]), Invocation::Gui);
    }

    #[test]
    fn help_and_version_exit_cleanly_and_an_unknown_command_does_not() {
        assert_eq!(dispatch(&args("help")), Invocation::Exited(OK));
        assert_eq!(dispatch(&args("--version")), Invocation::Exited(OK));
        assert_eq!(dispatch(&args("frobnicate")), Invocation::Exited(USAGE));
    }

    #[test]
    fn the_options_of_a_run_parse() {
        let parsed = Args::parse(&args(
            "--library /tmp/lib.db --pipeline detector --dataset ladder \
             --assert-baseline golden --no-cache --quiet",
        ))
        .unwrap();
        assert_eq!(parsed.library, Some(PathBuf::from("/tmp/lib.db")));
        assert_eq!(parsed.pipeline.as_deref(), Some("detector"));
        assert_eq!(parsed.dataset.as_deref(), Some("ladder"));
        assert_eq!(parsed.assert_baseline.as_deref(), Some("golden"));
        assert!(parsed.no_cache);
        assert!(parsed.quiet);
        assert!(parsed.tolerances.is_exact());
    }

    #[test]
    fn groups_and_tolerances_parse() {
        let parsed = Args::parse(&args(
            "--groups 1,2,3 --tolerance-sample-rel 0.01 --tolerance-metric-abs 0.5 \
             --allow-new-groups",
        ))
        .unwrap();
        assert_eq!(parsed.groups, vec![1, 2, 3]);
        assert_eq!(parsed.tolerances.sample_rel, 0.01);
        assert_eq!(parsed.tolerances.metric_abs, 0.5);
        assert!(parsed.tolerances.allow_new_groups);
        assert!(!parsed.tolerances.is_exact());
    }

    #[test]
    fn a_malformed_command_line_says_what_is_wrong() {
        assert!(Args::parse(&args("--pipeline"))
            .unwrap_err()
            .contains("needs a value"));
        assert!(Args::parse(&args("--groups 1,two"))
            .unwrap_err()
            .contains("not a number"));
        assert!(Args::parse(&args("--wat 1"))
            .unwrap_err()
            .contains("unknown option"));
    }

    #[test]
    fn a_missing_library_is_a_usage_error_not_a_failure() {
        // Exit 1 and exit 2 mean different things to a CI job: the harness
        // being wrong is not the algorithm regressing.
        let code = run_command(&args("--library /nonexistent/lib.db --pipeline p"));
        assert_eq!(code, USAGE);
    }
}
