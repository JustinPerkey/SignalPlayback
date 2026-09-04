//! Structured logging: a rolling file in the app data directory plus stderr.
//!
//! Spans around import, generation and pipeline runs are what make a slow file
//! or a slow stage diagnosable after the fact (`docs/DESIGN.md` §14).

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Keeps the background log writer alive. Dropping it flushes and stops the
/// writer, so `main` must hold it for the life of the process.
#[derive(Debug)]
pub struct LogHandle {
    _guard: Option<WorkerGuard>,
    /// Directory the rolling log is written to, when there is one.
    pub dir: Option<PathBuf>,
}

/// Installs the global subscriber. Verbosity comes from `RUST_LOG`, defaulting
/// to `info` for our crates and `warn` for dependencies. The binary's own
/// target is its `[[bin]]` name, `signalplayback`, not the package name.
#[must_use]
pub fn init() -> LogHandle {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("warn,signalplayback=info,sp_core=info,sp_store=info,sp_csv=info")
    });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);

    let file_sink = crate::paths::log_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let appender = tracing_appender::rolling::daily(&dir, "signalplayback.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        Some((dir, writer, guard))
    });

    match file_sink {
        Some((dir, writer, guard)) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();
            LogHandle {
                _guard: Some(guard),
                dir: Some(dir),
            }
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
            LogHandle {
                _guard: None,
                dir: None,
            }
        }
    }
}
