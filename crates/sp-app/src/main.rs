//! SignalPlayback — a desktop workbench for building a library of time-domain
//! signals and running signal-processing algorithms over them.
//!
//! This crate is the only one that knows about pixels; everything it displays
//! comes from the workspace crates below it (`docs/DESIGN.md` §4.1).

// A release build is a GUI application: no console window on Windows. Debug
// builds keep the console so `tracing` output is visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod jobs;
mod logging;
mod paths;
mod screens;
mod settings;
mod state;
mod theme;
mod typography;
mod ui;
mod widgets;

use iced::{Size, Task};

use state::App;

fn main() -> iced::Result {
    // Held for the life of the process: dropping it stops the log writer.
    let log = logging::init();
    let log_dir = log.dir.clone();

    // A subcommand runs headlessly and exits; the window opens only when
    // there is none (§10.4). The log guard is dropped by `exit` unwinding
    // nothing, so the CLI flushes through `drop(log)` first.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let cli::Invocation::Exited(code) = cli::dispatch(&args) {
        drop(log);
        std::process::exit(code);
    }

    // The application carries its own fonts rather than taking whatever the
    // platform offers, so a column of samples lays out the same on every
    // machine (`src/typography.rs`).
    let mut application = iced::application(App::title, App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .default_font(typography::BODY);
    for face in typography::FACES {
        application = application.font(face);
    }

    let result = application
        .window(iced::window::Settings {
            size: Size::new(1440.0, 900.0),
            min_size: Some(Size::new(960.0, 600.0)),
            ..iced::window::Settings::default()
        })
        .antialiasing(true)
        .run_with(move || {
            let (app, task) = App::new(
                log_dir,
                paths::settings_file(),
                paths::default_library_file(),
            );
            (app, Task::batch([task]))
        });

    if let Err(error) = &result {
        tracing::error!(%error, "the application exited with an error");
    }
    result
}
