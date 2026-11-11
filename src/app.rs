//! Desktop presentation and user commands. Sampling stays on background threads.
use crate::collector::Collector;
use crate::export::export_csv;
use crate::model::{Filters, ProcessRow, Snapshot, SortColumn, SortOrder, format_bytes, format_rate};
use crate::network::NetworkBackend;
use crate::settings::Settings;
use crate::tray::{Tray, TrayAction};
use eframe::{self, egui};
use egui_extras::{Column, TableBuilder};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(72, 198, 169);
const MUTED: egui::Color32 = egui::Color32::from_rgb(143, 158, 179);
const MAX_HISTORY: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Processes,
    Settings,
}

pub struct ActivityApp {
    settings: Settings,
    filters: Filters,
    sort: SortOrder,
    screen: Screen,
    collector: Collector,
    network: NetworkBackend,
    tray: Option<Tray>,
    tray_error: Option<String>,
    exiting: bool,
    paused: bool,
    selected_pid: Option<u32>,
    feedback: String,
    history: VecDeque<(Instant, f32, f32)>,
    last_snapshot: Option<Instant>,
    wake_stop: Arc<AtomicBool>,
}

impl ActivityApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let settings = Settings::load();
        let mut network = NetworkBackend::default();
        let mut feedback = String::new();
        if settings.enable_linux_traffic {
            if let Err(err) = network.enable() {
                feedback = format!("Не удалось включить сетевой мониторинг: {err}");
            }
        }
        let collector = Collector::start(settings.poll_interval_ms, network.shared());
        let (tray, tray_error) = match Tray::new() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(error)),
        };
        // eframe 0.37 runs logic-only passes when the window is hidden. Wake it so
        // tray menu events are still handled while no UI is rendered.
        let wake_stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&wake_stop);
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                ctx.request_repaint();
            }
        });
        Self {
            settings,
            filters: Filters::default(),
            sort: SortOrder::default(),
            screen: Screen::Processes,
            collector,
            network,
            tray,
            tray_error,
            exiting: false,
            paused: false,
            selected_pid: None,
            feedback,
            history: VecDeque::with_capacity(MAX_HISTORY),
            last_snapshot: None,
            wake_stop,
        }
    }

    fn save_settings(&mut self) {
        self.settings = self.settings.clone().validated();
        self.collector.set_interval(self.settings.poll_interval_ms);
        if let Err(err) = self.settings.save() {
            self.feedback = format!("Не удалось сохранить настройки: {err}");
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };
        match tray.poll() {
            Some(TrayAction::Show) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Some(TrayAction::Quit) => self.quit(ctx),
            None => {}
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.exiting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested())
            && self.settings.close_to_tray
            && self.tray.is_some()
            && !self.exiting
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn set_network(&mut self, enabled: bool) {
        if enabled {
            match self.network.enable() {
                Ok(()) => self.feedback = "Мониторинг трафика по процессам включён".into(),
                Err(err) => {
                    self.settings.enable_linux_traffic = false;
                    self.feedback = err;
                }
            }
        } else {
            self.network.disable();
            self.feedback = "Мониторинг трафика по процессам выключен".into();
        }
        self.save_settings();
    }

    fn observe(&mut self, snap: &Snapshot) {
        if let Some(updated) = snap.updated_at {
            if self.last_snapshot != Some(updated) {
                self.last_snapshot = Some(updated);
                let mem_pct = if snap.ram_total == 0 {
                    0.0
                } else {
                    snap.ram_used as f32 / snap.ram_total as f32 * 100.0
                };
                self.history.push_back((updated, snap.cpu_pct, mem_pct));
                if self.history.len() > MAX_HISTORY {
                    self.history.pop_front();
                }
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.heading(egui::RichText::new("PC ACTIVITY SCANNER").size(23.0).strong());
                ui.label(egui::RichText::new("Процессы, ресурсы и сетевой трафик в реальном времени").color(MUTED));
            });
            ui.add_space(24.0);
            ui.selectable_value(&mut self.screen, Screen::Processes, "Мониторинг");
            ui.selectable_value(&mut self.screen, Screen::Settings, "Настройки");
            ui.separator();
            if ui.button(if self.paused { "▶ Продолжить" } else { "⏸ Пауза" }).clicked() {
                self.paused = !self.paused;
                self.collector.set_paused(self.paused);
            }
            if ui.button("Выход").clicked() {
                self.quit(ui.ctx());
            }
        });
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
    }

    fn summary(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let total = snap.process_rows.len();
        ui.columns(4, |cols| {
            summary_card(&mut cols[0], "CPU", &format!("{:.1}%", snap.cpu_pct), &format!("{total} процессов"));
            summary_card(
                &mut cols[1], "ОПЕРАТИВНАЯ ПАМЯТЬ",
                &format_bytes(snap.ram_used),
                &format!("из {}", format_bytes(snap.ram_total)),
            );
            summary_card(&mut cols[2], "ЗАГРУЗКА", &format_rate(Some(snap.down_bps)),
                &format!("{} сетевых интерфейсов", snap.interface_count));
            summary_card(&mut cols[3], "ОТДАЧА", &format_rate(Some(snap.up_bps)),
                "Общий трафик интерфейсов");
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("История загрузки CPU").color(MUTED));
            ui.add_space(10.0);
            ui.label(egui::RichText::new("зелёный — CPU, серый — RAM").color(MUTED).small());
        });
        draw_history(ui, &self.history);
        ui.add_space(10.0);
    }

    fn filters(&mut self, ui: &mut egui::Ui, visible: usize, total: usize) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Процессы");
            ui.add(egui::TextEdit::singleline(&mut self.filters.query)
                .hint_text("Поиск: имя, PID, путь…").desired_width(270.0));
            ui.checkbox(&mut self.filters.only_active, "Только активные");
            ui.checkbox(&mut self.filters.only_network, "С трафиком");
            ui.checkbox(&mut self.filters.group_by_name, "Группировать по имени");
            if ui.button("Сбросить").clicked() {
                self.filters = Filters::default();
            }
        });
        ui.horizontal(|ui| {
            ui.label("CPU не менее");
            ui.add(egui::DragValue::new(&mut self.filters.minimum_cpu).range(0.0..=100.0).suffix(" %").speed(0.5));
            ui.add_space(10.0);
            ui.label("RAM не менее");
            ui.add(egui::DragValue::new(&mut self.filters.minimum_ram_mb).range(0..=65536).suffix(" МиБ").speed(10));
            ui.add_space(16.0);
            ui.label(egui::RichText::new(format!("Показано: {visible} из {total}")).color(MUTED));
        });
        ui.add_space(6.0);
    }

    fn table(&mut self, ui: &mut egui::Ui, rows: &[ProcessRow]) {
        // Horizontal scroll keeps the entire metric set available on smaller screens.
        egui::ScrollArea::horizontal().id_salt("process-table-horizontal").show(ui, |ui| {
            ui.set_min_width(1300.0);
            let mut builder = TableBuilder::new(ui)
                .id_salt("processes-table")
                .striped(true)
                .resizable(true)
                .auto_shrink([false, false]);
            for width in [210.0, 70.0, 85.0, 85.0, 95.0, 100.0, 105.0, 105.0, 110.0, 110.0] {
                builder = builder.column(Column::initial(width).at_least(54.0).resizable(true));
            }
            builder.header(30.0, |mut header| {
                for (column, label) in SortColumn::ALL {
                    header.col(|ui| {
                        let indicator = if self.sort.column == column {
                            if self.sort.descending { " ▼" } else { " ▲" }
                        } else { "" };
                        if ui.small_button(format!("{label}{indicator}")).clicked() {
                            self.sort.select(column);
                        }
                    });
                }
            }).body(|body| {
                body.rows(27.0, rows.len(), |mut table_row| {
                    let row = &rows[table_row.index()];
                    let selected = self.selected_pid == Some(row.pid) && row.pid != 0;
                    table_row.set_selected(selected);
                    table_row.col(|ui| {
                        let name = if row.count > 1 {
                            format!("{} ({})", row.name, row.count)
                        } else { row.name.clone() };
                        if ui.selectable_label(selected, name).on_hover_text(&row.executable).clicked() {
                            self.selected_pid = (row.pid != 0).then_some(row.pid);
                        }
                    });
                    table_row.col(|ui| { ui.label(if row.pid == 0 { "—".into() } else { row.pid.to_string() }); });
                    table_row.col(|ui| { ui.label(format!("{:.1}%", row.cpu_pct)); });
                    table_row.col(|ui| { ui.label(format_pct(row.gpu_pct)); });
                    table_row.col(|ui| { ui.label(format_bytes(row.ram_bytes)); });
                    table_row.col(|ui| { ui.label(format_optional_bytes(row.gpu_memory_bytes)); });
                    table_row.col(|ui| { ui.label(format_rate(row.down_bps)); });
                    table_row.col(|ui| { ui.label(format_rate(row.up_bps)); });
                    table_row.col(|ui| { ui.label(format_optional_bytes(row.downloaded_bytes)); });
                    table_row.col(|ui| { ui.label(format_optional_bytes(row.uploaded_bytes)); });
                });
            });
        });
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Настройки");
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.heading("Поведение окна");
            let available = self.tray.is_some();
            let response = ui.add_enabled(available,
                egui::Checkbox::new(&mut self.settings.close_to_tray, "При закрытии скрывать в системный трей"));
            if response.changed() { self.save_settings(); }
            if !available {
                ui.colored_label(egui::Color32::LIGHT_RED,
                    format!("Трей недоступен: {}", self.tray_error.as_deref().unwrap_or("нет системного трея")));
            } else {
                ui.label("Для полного выхода используйте меню иконки в трее или кнопку «Выход».");
            }
        });
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.heading("Сбор метрик");
            ui.horizontal(|ui| {
                ui.label("Интервал обновления:");
                egui::ComboBox::from_id_salt("sampling-interval")
                    .selected_text(format!("{} с", self.settings.poll_interval_ms / 1000))
                    .show_ui(ui, |ui| {
                        for value in [1000_u64, 2000, 5000] {
                            ui.selectable_value(&mut self.settings.poll_interval_ms, value,
                                format!("{} с", value / 1000));
                        }
                    });
                if self.settings.poll_interval_ms != self.collector.interval_ms.load(Ordering::Relaxed) {
                    self.save_settings();
                }
            });
            let previous = self.settings.enable_linux_traffic;
            ui.checkbox(&mut self.settings.enable_linux_traffic,
                "Трафик по процессам (Linux: NetHogs с правами администратора)");
            if previous != self.settings.enable_linux_traffic {
                self.set_network(self.settings.enable_linux_traffic);
            }
            ui.label("CPU/RAM и общий сетевой трафик доступны без повышения привилегий.");
            ui.label("GPU: Linux DRM fdinfo; NVIDIA pmon при поддержке драйвером.");
            ui.label("На Windows детализация GPU и сети по PID пока недоступна — это отображается как «—».");
        });
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.heading("Приватность и ограничения");
            ui.label("Сбор локальный: приложение не отправляет телеметрию на сервер.");
            ui.label("Учёт сетевого трафика каждого процесса — оценка за время работы приложения, не счётчик провайдера.");
            ui.label("Для системных процессов и GPU данные могут быть недоступны без нужных прав или поддержки драйвера.");
            if let Some(path) = Settings::file_path() {
                ui.label(format!("Настройки: {}", path.display()));
            }
        });
    }

    fn process_screen(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        self.summary(ui, snapshot);
        let rows = self.filters.apply(&snapshot.process_rows, self.sort);
        self.filters(ui, rows.len(), snapshot.process_rows.len());
        ui.horizontal(|ui| {
            if ui.button("Экспортировать CSV ↓").clicked() {
                self.feedback = match export_csv(&rows) {
                    Ok(path) => format!("Таблица сохранена: {}", path.display()),
                    Err(err) => format!("Ошибка экспорта CSV: {err}"),
                };
            }
            ui.label(egui::RichText::new(&snapshot.network_status).color(MUTED).small());
            ui.separator();
            ui.label(egui::RichText::new(&snapshot.gpu_status).color(MUTED).small());
        });
        ui.add_space(6.0);
        self.table(ui, &rows);
        if let Some(selected) = self.selected_pid {
            if let Some(row) = snapshot.process_rows.iter().find(|r| r.pid == selected) {
                ui.separator();
                ui.label(format!("Выбран PID {} · {} · {}", row.pid, row.name, row.executable));
            }
        }
    }
}

impl eframe::App for ActivityApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
        self.handle_close(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.collector.snapshot.read()
            .unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        self.observe(&snap);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(8.0);
            self.header(ui);
            match self.screen {
                Screen::Processes => self.process_screen(ui, &snap),
                Screen::Settings => self.settings_screen(ui),
            }
            if !self.feedback.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(&self.feedback).color(ACCENT));
            }
            if self.paused { ui.label(egui::RichText::new("Обновление данных приостановлено").color(MUTED)); }
        });
    }
}

impl Drop for ActivityApp {
    fn drop(&mut self) {
        self.wake_stop.store(true, Ordering::Relaxed);
    }
}

fn summary_card(ui: &mut egui::Ui, label: &str, value: &str, note: &str) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_height(86.0);
        ui.label(egui::RichText::new(label).color(MUTED).small());
        ui.label(egui::RichText::new(value).strong().size(23.0).color(ACCENT));
        ui.label(egui::RichText::new(note).color(MUTED).small());
    });
}

fn draw_history(ui: &mut egui::Ui, history: &VecDeque<(Instant, f32, f32)>) {
    let width = ui.available_width().max(100.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 70.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, egui::Color32::from_rgb(23, 29, 40));
    for level in [0.25_f32, 0.5, 0.75] {
        let y = rect.bottom() - rect.height() * level;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.5, egui::Color32::from_gray(58)),
        );
    }
    if history.len() < 2 { return; }
    let len = history.len();
    for (component, color) in [(1_usize, ACCENT), (2_usize, MUTED)] {
        let points = history.iter().enumerate().map(|(i, (_, cpu, ram))| {
            let value = if component == 1 { *cpu } else { *ram };
            egui::pos2(rect.left() + i as f32 / (len - 1) as f32 * rect.width(),
                rect.bottom() - (value.clamp(0.0, 100.0) / 100.0) * rect.height())
        }).collect::<Vec<_>>();
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
    egui::IconData { rgba, width: SIDE as u32, height: SIDE as u32 }
}
