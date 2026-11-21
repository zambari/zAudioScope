// Release builds on Windows are GUI apps: no console window behind them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod modules;
mod pitch;
mod relaunch;
mod ui;

/// eframe uses this as the folder for saved window and UI state.
const APP_ID: &str = "zAudioScope";

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("zAudioScope"),
        ..Default::default()
    };
    eframe::run_native(
        APP_ID,
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx, cc.storage)))),
    )
}
