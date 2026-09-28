//! trackertools v2 desktop app. See docs/v2/DESIGN.md.

mod input_probe;
mod keys;
mod layout;
mod media;
mod panels;
mod project;
mod session;
mod reflect_ui;
mod shell;
mod style;
mod video;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("trackertools")
            .with_inner_size([1600.0, 950.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("trackertools", options, Box::new(|cc| Ok(Box::new(shell::Shell::new(cc)))))
}
