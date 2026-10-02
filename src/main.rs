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
    // Every file given on the command line opens in its own tab.
    let paths: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    let mut viewport = eframe::egui::ViewportBuilder::default();
    if let Some(icon) = ui::brand::window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport: viewport
            .with_title("Ochre")
            .with_app_id("ochre")
            .with_inner_size([1200.0, 900.0])
            .with_min_inner_size([480.0, 360.0])
            .with_drag_and_drop(true),
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native("Ochre", options, Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx, paths)))))
}
