//! SignalPlayback — a desktop workbench for building a library of time-domain
//! signals and running signal-processing algorithms over them.
//!
//! This crate is the only one that knows about pixels; everything it displays
//! comes from the workspace crates below it (`docs/DESIGN.md` §4.1).

// A release build is a GUI application: no console window on Windows. Debug
// builds keep the console so `tracing` output is visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod jobs;
mod logging;
mod paths;
mod screens;
mod state;

use iced::{Size, Task};

use state::App;

fn main() -> iced::Result {
    // Held for the life of the process: dropping it stops the log writer.
    let log = logging::init();
    let log_dir = log.dir.clone();

    let result = iced::application(App::title, App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .window(iced::window::Settings {
            size: Size::new(1440.0, 900.0),
            min_size: Some(Size::new(960.0, 600.0)),
            ..iced::window::Settings::default()
        })
        .antialiasing(true)
        .run_with(move || {
            let (app, task) = App::new(log_dir, paths::default_library_file());
            (app, Task::batch([task]))
        });

    if let Err(error) = &result {
        tracing::error!(%error, "the application exited with an error");
    }
    result
}
