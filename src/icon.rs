//! Pre-rendered local artwork; no runtime image decoding or file access.
use eframe::egui::IconData;

pub fn window() -> IconData {
    IconData {
        rgba: include_bytes!("../assets/icons/app.rgba").to_vec(),
        width: 128,
        height: 128,
    }
}

pub fn tray() -> IconData {
    IconData {
        rgba: include_bytes!("../assets/icons/tray.rgba").to_vec(),
        width: 32,
        height: 32,
    }
}
