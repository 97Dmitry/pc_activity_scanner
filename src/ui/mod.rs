//! Pure presentation helpers; no sampling, persistence or subprocess control.
pub mod history;
pub mod table;
use crate::model::{Snapshot, format_bytes, format_rate};
use eframe::egui;
use std::collections::VecDeque;
use std::time::Instant;

pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(92, 200, 238);
pub const MUTED: egui::Color32 = egui::Color32::from_rgb(190, 203, 220);
pub const RAM: egui::Color32 = egui::Color32::from_rgb(236, 181, 97);

pub fn configure(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(24, 32, 43);
    visuals.extreme_bg_color = egui::Color32::from_rgb(17, 24, 34);
    // Keep labels readable in every widget state, including secondary text.
    let text = egui::Color32::from_rgb(235, 241, 249);
    visuals.weak_text_color = Some(MUTED);
    visuals.widgets.noninteractive.fg_stroke.color = text;
    visuals.widgets.inactive.fg_stroke.color = text;
    visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.open.fg_stroke.color = egui::Color32::WHITE;
    visuals.faint_bg_color = egui::Color32::from_rgb(31, 42, 56);
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
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(14.0));
        style.visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(72, 91, 114));
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 48, 64);
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(46, 66, 88);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(42, 80, 105);
        style.visuals.widgets.inactive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(83, 104, 130));
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
    egui::CollapsingHeader::new(format!("Логические CPU ({})", snap.logical_cpu_count))
        .show(ui, |ui| {
            let mean = crate::model::logical_cpu_average(&snap.logical_cpus);
            ui.label(format!("Среднее логических CPU: {} · общий CPU: {}", format_pct(mean), format_pct(ready.then_some(snap.cpu_pct))));
            ui.small("Каждый логический CPU: 0–100%. CPU процессов нормализован по мощности всего хоста. Небольшие отличия среднего от общего возможны из-за округления и интервалов ОС.");
            if snap.logical_cpus.len() < snap.logical_cpu_count { ui.label(format!("Показаны {} из {} логических CPU", snap.logical_cpus.len(), snap.logical_cpu_count)); }
            let columns = ((ui.available_width()/100.0) as usize).clamp(2,16);
            egui::ScrollArea::vertical().id_salt("logical-cpu-scroll").max_height(180.0).show_rows(ui, 50.0, snap.logical_cpus.len().div_ceil(columns), |ui, visible| {
                egui::Grid::new("logical-cpu-grid").num_columns(columns).show(ui, |ui| {
                    for index in visible.start*columns .. (visible.end*columns).min(snap.logical_cpus.len()) {
                        let usage = &snap.logical_cpus[index];
                        ui.vertical(|ui| {
                            ui.small(format!("CPU {index}: {}", format_pct(*usage)));
                            if let Some(value) = usage { ui.add(egui::ProgressBar::new(value/100.0).desired_width(86.0).fill(ACCENT)); }
                            else { ui.monospace("—"); }
                        });
                        if (index+1)%columns == 0 { ui.end_row(); }
                    }
                });
            });
        });
    egui::CollapsingHeader::new("Температуры и thermal-состояния")
        .show(ui, |ui| {
            ui.label(&snap.thermal.status);
            let Some(updated) = snap.thermal.updated_at.filter(|at| at.elapsed().as_secs() <= 12) else {
                ui.label("Свежий источник температур недоступен. Отсутствие показателя не равно нулю."); return;
            };
            ui.small(format!("Обновлено {:.1} с назад. Thermal-события — за последний интервал источника, не доказательство текущего троттлинга.", updated.elapsed().as_secs_f64()));
            egui::ScrollArea::vertical().id_salt("thermal-rows").max_height(200.0).show_rows(ui, 26.0, snap.thermal.readings.len(), |ui, visible| {
                egui::Grid::new("thermal-grid").striped(true).show(ui, |ui| {
                    for row in &snap.thermal.readings[visible] {
                        ui.label(&row.label).on_hover_text(format!("{}\n{}", row.id, row.source));
                        ui.monospace(row.celsius.map_or_else(|| "—".into(), |v| format!("{v:.1} °C")));
                        ui.label(match row.throttled { Some(true) => "Thermal flag: активен", Some(false) => "Thermal flag: неактивен", None => "Thermal flag: —" });
                        ui.label(row.thermal_events.map_or_else(|| "Новые thermal-события: —".into(), |v| format!("Новые thermal-события: {v}")));
                        ui.end_row();
                    }
                });
            });
            ui.small("ACPI-зона не обязательно CPU. Высокая температура или низкая частота сами по себе не объявляются троттлингом.");
        });
    egui::CollapsingHeader::new("Накопители: физическая активность")
        .show(ui, |ui| {
            ui.label(&snap.disk_status);
            let mut disks = snap.devices.iter().collect::<Vec<_>>();
            let key = crate::metrics::MetricKind::DiskActive;
            disks.sort_by(|a,b| b.metrics.get(&key).copied().unwrap_or(-1.0).total_cmp(&a.metrics.get(&key).copied().unwrap_or(-1.0)));
            egui::Grid::new("physical-disks").striped(true).show(ui, |ui| {
                ui.label("Устройство"); ui.label("Чтение/с"); ui.label("Запись/с"); ui.label("Активность"); ui.end_row();
                for disk in disks {
                    ui.label(&disk.name).on_hover_text(&disk.id);
                    for kind in [crate::metrics::MetricKind::IoRead, crate::metrics::MetricKind::IoWrite, key] {
                        ui.label(kind.format(disk.metrics.get(&kind).copied()));
                    }
                    ui.end_row();
                }
            });
            ui.small("I/O процесса и активность накопителя — разные источники. Связь PID → диск не выводится предположительно.");
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
