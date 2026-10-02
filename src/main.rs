#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annot;
mod app;
mod config;
mod doc;
mod pdf;
mod search;
mod ui;
mod viewer;

fn main() -> eframe::Result {
    let path = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Ochre")
            .with_app_id("ochre")
            .with_inner_size([1200.0, 900.0])
            .with_min_inner_size([480.0, 360.0])
            .with_drag_and_drop(true),
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native("Ochre", options, Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx, path)))))
}
