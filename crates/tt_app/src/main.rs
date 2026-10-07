//! trackertools v2 desktop app. See docs/v2/DESIGN.md.

// A release build is a windowed program: no console window beside it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod colors;
mod cotracker;
mod demo;
mod files;
mod icons;
mod input_probe;
mod keys;
mod layout;
mod media;
mod panels;
mod pointer;
mod project;
mod recover;
mod session;
mod reflect_ui;
mod scene;
mod setup;
mod shell;
mod style;
mod update;
mod video;

fn main() -> eframe::Result {
    // (Started again after an error: the old process has exited before the log opens.)
    recover::wait_for_old();
    setup::start_logging();
    // FFmpeg isn't part of trackertools: without it, a small setup window comes first.
    setup::apply_location();
    let ready = setup::ready();
    if !ready {
        tracing::info!("FFmpeg not found: setup first");
    }
    let (title, size, min) = if ready { ("trackertools", [1600.0, 950.0], [900.0, 560.0]) } else { ("trackertools setup", [800.0, 900.0], [560.0, 480.0]) };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size(min),
        ..Default::default()
    };
    let run = eframe::run_native("trackertools", options, Box::new(move |cc| Ok(Box::new(shell::Shell::new(cc, ready)))));
    if let Err(e) = &run {
        setup::cannot_start(&e.to_string());
    }
    run
}
