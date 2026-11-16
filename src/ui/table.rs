use crate::model::{ProcessIdentity, ProcessRow, ProcessView, SortColumn, SortOrder, format_rate};
use eframe::egui;
use egui_extras::{Column, TableBuilder};

use super::{ACCENT, MUTED, format_optional_bytes, format_pct};

pub fn process_table(
    ui: &mut egui::Ui,
    rows: &[ProcessRow],
    sort: &mut SortOrder,
    selected_process: &mut Option<ProcessIdentity>,
    detailed: bool,
    view: &mut ProcessView,
) {
    let columns = SortColumn::ALL
        .into_iter()
        .filter(|(column, _)| {
            detailed
                || matches!(
                    column,
                    SortColumn::Name
                        | SortColumn::Pid
                        | SortColumn::Cpu
                        | SortColumn::Ram
                        | SortColumn::Down
                        | SortColumn::Up
                        | SortColumn::Metric(
                            crate::metrics::MetricKind::IoRead
                                | crate::metrics::MetricKind::IoWrite
                        )
                )
        })
        .collect::<Vec<_>>();
    egui::ScrollArea::horizontal()
        .id_salt("process-table-horizontal")
        .show(ui, |ui| {
            ui.set_min_width(if detailed { 1240.0 } else { 740.0 });
            let mut builder = TableBuilder::new(ui)
                .id_salt(("processes-table", detailed))
                .striped(true)
                .resizable(true)
                .auto_shrink([false, false]);
            for (column, _) in &columns {
                builder = builder.column(match column {
                    // Recompute on window resize instead of retaining a wide-screen name column.
                    SortColumn::Name => Column::remainder()
                        .at_least(210.0)
                        .resizable(false)
                        .clip(true),
                    SortColumn::Pid => Column::initial(65.0).at_least(50.0),
                    _ => Column::initial(105.0).at_least(74.0),
                });
            }
            builder
                .header(36.0, |mut header| {
                    for &(column, label) in &columns {
                        header.col(|ui| {
                            let active = sort.column == column;
                            let response = ui.button(if active {
                                format!("{label}   ")
                            } else {
                                label.into()
                            });
                            if active {
                                let x = response.rect.right() - 9.0;
                                let y = response.rect.center().y;
                                let direction = if sort.descending { 1.0 } else { -1.0 };
                                ui.painter().add(egui::Shape::convex_polygon(
                                    vec![
                                        egui::pos2(x - 3.0, y - 2.0 * direction),
                                        egui::pos2(x + 3.0, y - 2.0 * direction),
                                        egui::pos2(x, y + 3.0 * direction),
                                    ],
                                    ACCENT,
                                    egui::Stroke::NONE,
                                ));
                            }
                            if column == SortColumn::Ram { response.clone().on_hover_text("Resident RAM: Linux VmRSS / Windows working set. Включает общие страницы; сумма группы не равна уникальной физической памяти. Недоступное значение — неизвестно."); }
                            if column == SortColumn::Cpu { response.clone().on_hover_text("CPU нормализован по всем логическим процессорам хоста: 100% — вся мощность. Первый интервал и недоступные счётчики неизвестны."); }
                            if column == SortColumn::Gpu { response.clone().on_hover_text("Максимум доступных GPU-движков процесса. Для группы — максимум участников; проценты движков не суммируются."); }
                            if let SortColumn::Metric(kind) = column {
                                response.clone().on_hover_text(kind.description());
                            }
                            if response.clicked() {
                                sort.select(column);
                            }
                        });
                    }
                })
                .body(|body| {
                    body.rows(32.0, rows.len(), |mut table_row| {
                        let row = &rows[table_row.index()];
                        let selected = *selected_process == Some(row.identity()) && !row.is_group;
                        table_row.set_selected(selected);
                        for &(column, _) in &columns {
                            table_row.col(|ui| {
                                if column == SortColumn::Name {
                                    let name = if row.count > 1 {
                                        format!("{} ({})", row.name, row.count)
                                    } else {
                                        row.name.clone()
                                    };
                                    let prefix = if row.is_group { if view.is_group_expanded(&row.name) { "▼ " } else { "▶ " } } else { "" };
                                    let name = format!("{}{}{}", "  ".repeat(row.view_depth.min(16)), prefix, name);
                                    let response = ui
                                        .add(egui::Button::selectable(selected, &name).truncate())
                                        .on_hover_text(format!("{name}\n{}", row.executable));
                                    if response.clicked() {
                                        if row.is_group { view.toggle_group(&row.name); ui.ctx().request_repaint(); }
                                        else { *selected_process = Some(row.identity()); }
                                    }
                                    response.context_menu(|ui| {
                                        if ui.button("Копировать имя").clicked() {
                                            ui.ctx().copy_text(row.name.clone());
                                            ui.close();
                                        }
                                        if row.pid != 0 && ui.button("Копировать PID").clicked()
                                        {
                                            ui.ctx().copy_text(row.pid.to_string());
                                            ui.close();
                                        }
                                        if !row.executable.is_empty()
                                            && ui.button("Копировать путь").clicked()
                                        {
                                            ui.ctx().copy_text(row.executable.clone());
                                            ui.close();
                                        }
                                    });
                                    return;
                                }
                                let text = match column {
                                    SortColumn::Name => unreachable!(),
                                    SortColumn::Pid => {
                                        if row.pid == 0 {
                                            "—".into()
                                        } else {
                                            row.pid.to_string()
                                        }
                                    }
                                    SortColumn::Metric(kind) => {
                                        kind.format(row.metrics.get(&kind).copied())
                                    }
                                    SortColumn::Cpu => format_pct(row.cpu_pct),
                                    SortColumn::Gpu => format_pct(row.gpu_pct),
                                    SortColumn::Ram => format_optional_bytes(row.ram_bytes),
                                    SortColumn::GpuMemory => {
                                        format_optional_bytes(row.gpu_memory_bytes)
                                    }
                                    SortColumn::Down => format_rate(row.down_bps),
                                    SortColumn::Up => format_rate(row.up_bps),
                                    SortColumn::TotalDown => {
                                        format_optional_bytes(row.downloaded_bytes)
                                    }
                                    SortColumn::TotalUp => {
                                        format_optional_bytes(row.uploaded_bytes)
                                    }
                                };
                                let color = if column == SortColumn::Cpu && row.cpu_pct.is_some_and(|value| value >= 10.0) {
                                    ACCENT
                                } else if text == "—" {
                                    MUTED
                                } else {
                                    ui.visuals().text_color()
                                };
                                ui.label(egui::RichText::new(text).monospace().color(color));
                            });
                        }
                    });
                });
        });
}
