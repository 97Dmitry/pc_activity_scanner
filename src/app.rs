//! Desktop presentation and user commands. Sampling stays on background threads.
use crate::collector::Collector;
use crate::export::export_csv;
use crate::model::{Filters, ProcessView, Snapshot, SortOrder};
use crate::network::NetworkBackend;
use crate::settings::Settings;
use crate::tray::{Tray, TrayAction};
use crate::ui::{self, ACCENT, MUTED};
use eframe::{self, egui};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_HISTORY: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Processes,
    Settings,
}

pub struct ActivityApp {
    settings: Settings,
    filters: Filters,
    process_view: ProcessView,
    sort: SortOrder,
    screen: Screen,
    collector: Collector,
    network: NetworkBackend,
    tray: Option<Tray>,
    tray_error: Option<String>,
    exiting: bool,
    paused: bool,
    selected_pid: Option<u32>,
    detailed_metrics: bool,
    feedback: String,
    history: VecDeque<(Instant, f32, f32)>,
    last_snapshot: Option<Instant>,
    wake_stop: Arc<AtomicBool>,
}

impl ActivityApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        ui::configure(&cc.egui_ctx);
        let mut settings = Settings::load();
        let mut network = NetworkBackend::default();
        let mut feedback = String::new();
        if settings.enable_linux_traffic
            && let Err(err) = network.enable()
        {
            settings.enable_linux_traffic = false;
            let _ = settings.save();
            feedback = format!("Не удалось включить сетевой мониторинг: {err}");
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
            process_view: ProcessView::default(),
            sort: SortOrder::default(),
            screen: Screen::Processes,
            collector,
            network,
            tray,
            tray_error,
            exiting: false,
            paused: false,
            selected_pid: None,
            detailed_metrics: false,
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
        if let Some(updated) = snap.updated_at
            && self.last_snapshot != Some(updated)
        {
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

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("PC Activity Scanner").size(20.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Выход").clicked() {
                    self.quit(ui.ctx());
                }
                if ui
                    .button(if self.paused {
                        "Продолжить"
                    } else {
                        "Пауза"
                    })
                    .clicked()
                {
                    self.paused = !self.paused;
                    self.collector.set_paused(self.paused);
                }
                ui.label(
                    egui::RichText::new(format!(
                        "Опрос: {} с",
                        self.settings.poll_interval_ms / 1000
                    ))
                    .small()
                    .color(MUTED),
                );
            });
        });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.screen, Screen::Processes, "Мониторинг");
            ui.selectable_value(&mut self.screen, Screen::Settings, "Настройки");
            ui.separator();
            ui.label(
                egui::RichText::new("Локальный монитор ресурсов")
                    .small()
                    .color(MUTED),
            );
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        ui.horizontal_wrapped(|ui| {
            let (status, color) = if self.paused {
                ("Пауза".to_owned(), crate::ui::RAM)
            } else if let Some(updated) = snapshot.updated_at {
                let age = updated.elapsed().as_secs();
                let stale = age > self.settings.poll_interval_ms / 1000 + 3;
                (
                    format!(
                        "{} · обновлено {} с назад",
                        if stale {
                            "Данные устарели"
                        } else {
                            "Мониторинг"
                        },
                        age
                    ),
                    if stale { crate::ui::RAM } else { ACCENT },
                )
            } else {
                ("Ожидание первого измерения…".to_owned(), MUTED)
            };
            ui.colored_label(color, status);
            if self.tray.is_none() {
                ui.separator();
                ui.label(egui::RichText::new("Трей недоступен").color(MUTED))
                    .on_hover_text(self.tray_error.as_deref().unwrap_or("нет системного трея"));
            }
        });
        if !self.feedback.is_empty() {
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Скрыть сообщение").clicked() {
                    self.feedback.clear();
                }
                ui.label(&self.feedback);
            });
        }
    }

    fn filters(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Процессы");
            ui.add(
                egui::TextEdit::singleline(&mut self.filters.query)
                    .hint_text("Поиск: имя, PID, путь…")
                    .desired_width(270.0),
            );
            ui.checkbox(&mut self.filters.only_active, "Только активные");
            ui.checkbox(&mut self.filters.only_network, "С трафиком");
            ui.checkbox(&mut self.filters.group_by_name, "Группировать по имени");
            if ui.button("Сбросить").clicked() {
                self.filters = Filters::default();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("CPU не менее");
            ui.add(
                egui::DragValue::new(&mut self.filters.minimum_cpu)
                    .range(0.0..=100.0)
                    .suffix(" %")
                    .speed(0.5),
            );
            ui.add_space(10.0);
            ui.label("RAM не менее");
            ui.add(
                egui::DragValue::new(&mut self.filters.minimum_ram_mb)
                    .range(0..=65536)
                    .suffix(" МиБ")
                    .speed(10),
            );
            ui.add_space(16.0);
            ui.checkbox(&mut self.detailed_metrics, "GPU и итоги трафика");
        });
        ui.add_space(6.0);
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Настройки");
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading("Поведение окна");
            let available = self.tray.is_some();
            let response = ui.add_enabled(
                available,
                egui::Checkbox::new(
                    &mut self.settings.close_to_tray,
                    "При закрытии скрывать в системный трей",
                ),
            );
            if response.changed() {
                self.save_settings();
            }
            if !available {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "Системный трей недоступен. Закрытие окна завершит программу.",
                )
                .on_hover_text(self.tray_error.as_deref().unwrap_or("нет системного трея"));
            } else {
                ui.label("Для полного выхода используйте меню иконки в трее или кнопку «Выход».");
            }
        });
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading("Сбор метрик");
            let previous_interval = self.settings.poll_interval_ms;
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
                if self.settings.poll_interval_ms != previous_interval {
                    self.save_settings();
                }
            });
            let previous = self.settings.enable_linux_traffic;
            ui.add_enabled(cfg!(target_os = "linux"), egui::Checkbox::new(&mut self.settings.enable_linux_traffic,
                "Трафик по процессам (Linux: NetHogs с правами администратора)"));
            if previous != self.settings.enable_linux_traffic {
                self.set_network(self.settings.enable_linux_traffic);
            }
            ui.label("CPU/RAM и общий сетевой трафик доступны без повышения привилегий.");
            ui.label("GPU: Linux DRM fdinfo; NVIDIA pmon при поддержке драйвером.");
            ui.label("На Windows детализация GPU и сети по PID пока недоступна — это отображается как «—».");
        });
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
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
        ui::summary(ui, snapshot, &self.history);
        self.filters(ui);
        let rows = self.process_view.rows(snapshot, &self.filters, self.sort);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{}: {} / {} процессов",
                    if self.filters.group_by_name {
                        "Групп"
                    } else {
                        "Показано"
                    },
                    rows.len(),
                    snapshot.process_rows.len()
                ))
                .color(MUTED),
            );
            if ui
                .add_enabled(!rows.is_empty(), egui::Button::new("Экспорт CSV"))
                .clicked()
            {
                self.feedback = match export_csv(&rows) {
                    Ok(path) => format!("Таблица сохранена: {}", path.display()),
                    Err(err) => format!("Ошибка экспорта CSV: {err}"),
                };
            }
            egui::CollapsingHeader::new("Доступность метрик").show(ui, |ui| {
                ui.label(&snapshot.network_status);
                ui.label(&snapshot.gpu_status);
                ui.label(
                    "— означает отсутствие данных. Общий трафик не распределяется по процессам.",
                );
            });
        });
        if let Some(selected) = self.selected_pid {
            if let Some(row) = snapshot.process_rows.iter().find(|r| r.pid == selected) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("PID {}: {}", row.pid, row.name));
                    if !row.executable.is_empty() && ui.small_button("Копировать путь").clicked()
                    {
                        ui.ctx().copy_text(row.executable.clone());
                    }
                    if ui.small_button("Снять выделение").clicked() {
                        self.selected_pid = None;
                    }
                });
            } else {
                self.selected_pid = None;
            }
        }
        ui.add_space(4.0);
        if snapshot.updated_at.is_none() {
            ui.spinner();
            ui.label("Собираем первое измерение ресурсов…");
        } else if rows.is_empty() {
            ui.add_space(24.0);
            ui.heading("Процессы не найдены");
            ui.label("Измените поисковый запрос или сбросьте фильтры.");
            if ui.button("Сбросить фильтры").clicked() {
                self.filters = Filters::default();
            }
        } else {
            ui::table::process_table(
                ui,
                &rows,
                &mut self.sort,
                &mut self.selected_pid,
                self.detailed_metrics,
            );
        }
    }
}

impl eframe::App for ActivityApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
        self.handle_close(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self
            .collector
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        self.observe(&snap);
        egui::Panel::bottom("status-bar").show(ui, |ui| self.status_bar(ui, &snap));
        egui::Panel::top("navigation").show(ui, |ui| self.header(ui));
        egui::CentralPanel::default().show(ui, |ui| match self.screen {
            Screen::Processes => self.process_screen(ui, &snap),
            Screen::Settings => {
                egui::ScrollArea::vertical().show(ui, |ui| self.settings_screen(ui));
            }
        });
    }
}

impl Drop for ActivityApp {
    fn drop(&mut self) {
        self.wake_stop.store(true, Ordering::Relaxed);
    }
}
