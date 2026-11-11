//! Pure presentation helpers; no sampling, persistence or subprocess control.
pub mod history;
pub mod table;
use crate::model::{Snapshot, format_bytes, format_rate};
use eframe::egui;
use std::collections::VecDeque;
use std::time::Instant;

pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(92, 200, 238);
pub const MUTED: egui::Color32 = egui::Color32::from_rgb(164, 178, 197);
pub const RAM: egui::Color32 = egui::Color32::from_rgb(236, 181, 97);

pub fn configure(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(24, 32, 43);
    visuals.extreme_bg_color = egui::Color32::from_rgb(17, 24, 34);
    visuals.selection.bg_fill = egui::Color32::from_rgb(35, 77, 101);
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(10.0, 7.0);
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style.visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(43, 57, 75));
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 48, 64);
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(46, 66, 88);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(42, 80, 105);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
    });
}

pub fn live_summary(ui: &mut egui::Ui, snap: &Snapshot) {
    let total = snap.process_rows.len();
    let ready = snap.updated_at.is_some();
    ui.columns(4, |cols| {
        summary_card(
            &mut cols[0],
            "CPU",
            &format_pct(ready.then_some(snap.cpu_pct)),
            &format!("{total} процессов"),
            ACCENT,
        );
        summary_card(
            &mut cols[1],
            "Оперативная память",
            &format_optional_bytes(ready.then_some(snap.ram_used)),
            &format!("из {}", format_bytes(snap.ram_total)),
            RAM,
        );
        summary_card(
            &mut cols[2],
            "Получение",
            &format_rate(ready.then_some(snap.down_bps)),
            &format!("{} сетевых интерфейсов", snap.interface_count),
            history::NETWORK,
        );
        summary_card(
            &mut cols[3],
            "Отправка",
            &format_rate(ready.then_some(snap.up_bps)),
            "Общий трафик интерфейсов",
            history::NETWORK,
        );
    });
}

pub fn summary(ui: &mut egui::Ui, snap: &Snapshot, history: &VecDeque<(Instant, f32, f32)>) {
    live_summary(ui, snap);
    ui.add_space(6.0);
    egui::CollapsingHeader::new("История CPU и памяти")
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(ACCENT, "CPU");
                ui.colored_label(RAM, "RAM");
                if let (Some(first), Some(last)) = (history.front(), history.back()) {
                    ui.label(
                        egui::RichText::new(format!(
                            "за {} с",
                            last.0.duration_since(first.0).as_secs()
                        ))
                        .small()
                        .color(MUTED),
                    );
                }
            });
            draw_history(ui, history);
        });
    ui.add_space(8.0);
}

fn summary_card(ui: &mut egui::Ui, label: &str, value: &str, note: &str, color: egui::Color32) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_height(52.0);
        ui.set_min_width(ui.available_width());
        ui.label(egui::RichText::new(label).color(MUTED).small());
        ui.label(egui::RichText::new(value).strong().size(20.0).color(color));
        ui.label(egui::RichText::new(note).color(MUTED).small());
    });
}

fn draw_history(ui: &mut egui::Ui, history: &VecDeque<(Instant, f32, f32)>) {
    let width = ui.available_width().max(100.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 56.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, egui::Color32::from_rgb(23, 29, 40));
    for level in [0.25_f32, 0.5, 0.75] {
        let y = rect.bottom() - rect.height() * level;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.5, egui::Color32::from_gray(58)),
        );
    }
    if history.len() < 2 {
        return;
    }
    let start = history.front().unwrap().0;
    let span = history
        .back()
        .unwrap()
        .0
        .duration_since(start)
        .as_secs_f32()
        .max(0.001);
    for (component, color) in [(1_usize, ACCENT), (2_usize, RAM)] {
        let points = history
            .iter()
            .map(|(at, cpu, ram)| {
                let value = if component == 1 { *cpu } else { *ram };
                egui::pos2(
                    rect.left() + at.duration_since(start).as_secs_f32() / span * rect.width(),
                    rect.bottom() - (value.clamp(0.0, 100.0) / 100.0) * rect.height(),
                )
            })
            .collect::<Vec<_>>();
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.8, color)));
    }
}

fn format_pct(percent: Option<f32>) -> String {
    percent.map_or_else(|| "—".into(), |value| format!("{value:.1}%"))
}

fn format_optional_bytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "—".into(), format_bytes)
}

pub fn window_icon() -> egui::IconData {
    const SIDE: usize = 32;
    let mut rgba = vec![0u8; SIDE * SIDE * 4];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let index = (y * SIDE + x) * 4;
            let in_circle = ((x as isize - 15).pow(2) + (y as isize - 15).pow(2)) < 220;
            if in_circle {
                rgba[index..index + 4].copy_from_slice(&[21, 35, 53, 255]);
                if (7..25).contains(&x) && y > 23 - ((x % 9) / 2) && y < 25 {
                    rgba[index..index + 4].copy_from_slice(&[72, 198, 169, 255]);
                }
            }
        }
    }
    egui::IconData {
        rgba,
        width: SIDE as u32,
        height: SIDE as u32,
    }
}
