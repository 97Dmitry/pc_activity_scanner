#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
#[cfg(any(target_os = "windows", test))]
mod autostart;
mod collector;
mod export;
mod gpu;
mod history;
mod history_export;
mod icon;
mod model;
mod network;
mod nvidia;
mod settings;
mod tray;
mod ui;

use eframe::egui;
use std::sync::Arc;

fn main() -> eframe::Result {
    #[cfg(target_os = "windows")]
    if network::run_windows_helper_if_requested() {
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--windows-network-smoke-test"))
    {
        match network::run_windows_network_smoke_test() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("Windows network smoke test failed: {error}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--nethogs-helper")) {
        std::process::exit(network::run_nethogs_helper());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PC Activity Scanner")
            .with_inner_size(egui::vec2(1400.0, 860.0))
            .with_min_inner_size(egui::vec2(960.0, 600.0))
            .with_icon(Arc::new(icon::window())),
        ..Default::default()
    };
    eframe::run_native(
        "PC Activity Scanner",
        options,
        Box::new(|cc| Ok(Box::new(app::ActivityApp::new(cc)))),
    )
}
