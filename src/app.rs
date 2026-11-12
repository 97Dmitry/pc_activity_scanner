//! Desktop presentation and user commands. Sampling stays on background threads.
use crate::collector::Collector;
use crate::export::export_csv;
use crate::history::{HistoryService, types::HistoryConfig};
use crate::model::{Filters, ProcessView, Snapshot, SortOrder};
use crate::network::NetworkBackend;
use crate::settings::Settings;
use crate::tray::{Tray, TrayAction};
use crate::ui::history::{HistoryMode, HistoryUi};
use crate::ui::{self, ACCENT, MUTED};
use eframe::{self, egui};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_HISTORY: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Overview,
    Processes,
    History,
    Compare,
    Settings,
}

#[cfg(any(target_os = "windows", test))]
fn reconcile_startup(
    actual: &mut Option<crate::autostart::State>,
    draft: &mut crate::autostart::State,
    result: Result<crate::autostart::State, crate::autostart::Error>,
) -> Option<String> {
    match result {
        Ok(state) => {
            *actual = Some(state);
            *draft = state;
            None
        }
        Err(error) => {
            if let Some(state) = error.state {
                *actual = Some(state);
            }
            Some(format!("{}\n{}", error.message, error.details))
        }
    }
}

pub struct ActivityApp {
    settings: Settings,
    filters: Filters,
    process_view: ProcessView,
    sort: SortOrder,
    screen: Screen,
    collector: Collector,
    history_service: HistoryService,
    history_ui: HistoryUi,
    confirm_clear_history: bool,
    network: NetworkBackend,
    #[cfg(target_os = "windows")]
    startup: crate::autostart::Controller,
    #[cfg(target_os = "windows")]
    startup_current: Option<crate::autostart::State>,
    #[cfg(target_os = "windows")]
    startup_draft: crate::autostart::State,
    #[cfg(target_os = "windows")]
    startup_error: Option<String>,
    #[cfg(target_os = "windows")]
    startup_apply_requested: bool,
    tray: Option<Tray>,
    tray_error: Option<String>,
    exiting: bool,
    paused: bool,
    selected_pid: Option<u32>,
    detailed_metrics: bool,
    feedback: String,
    last_action_error: Option<String>,
    diagnostics_open: bool,
    network_error: Option<String>,
    network_requested_at: Option<Instant>,
    history: VecDeque<(Instant, f32, f32)>,
    last_snapshot: Option<Instant>,
    wake_stop: Arc<AtomicBool>,
}

impl ActivityApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        ui::configure(&cc.egui_ctx);
        let settings = Settings::load();
        let mut network_error = None;
        let network_requested_at = settings.network_enabled().then(Instant::now);
        let mut network = NetworkBackend::default();
        let mut feedback = String::new();
        if settings.network_enabled()
            && let Err(err) = network.enable()
        {
            network_error = Some(err);
            feedback = "Не удалось включить трафик по приложениям. Повторите попытку.".into();
        }
        let history_service = HistoryService::start(HistoryConfig {
            enabled: settings.history_enabled,
            retention_days: settings.history_retention_days,
        });
        let collector = Collector::start(
            settings.poll_interval_ms,
            network.shared(),
            history_service.recorder(),
        );
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
        #[cfg(target_os = "windows")]
        let startup = {
            let mut controller = crate::autostart::Controller::new();
            controller.query();
            controller
        };
        Self {
            #[cfg(target_os = "windows")]
            startup,
            #[cfg(target_os = "windows")]
            startup_current: None,
            #[cfg(target_os = "windows")]
            startup_draft: crate::autostart::State::default(),
            #[cfg(target_os = "windows")]
            startup_error: None,
            #[cfg(target_os = "windows")]
            startup_apply_requested: false,
            settings,
            filters: Filters::default(),
            process_view: ProcessView::default(),
            sort: SortOrder::default(),
            screen: Screen::Overview,
            collector,
            history_service,
            history_ui: HistoryUi::default(),
            confirm_clear_history: false,
            network,
            tray,
            tray_error,
            exiting: false,
            paused: false,
            selected_pid: None,
            detailed_metrics: false,
            feedback,
            last_action_error: None,
            diagnostics_open: false,
            network_error,
            network_requested_at,
            history: VecDeque::with_capacity(MAX_HISTORY),
            last_snapshot: None,
            wake_stop,
        }
    }

    fn save_settings(&mut self) {
        self.settings = self.settings.clone().validated();
        self.collector.set_interval(self.settings.poll_interval_ms);
        if !self.history_service.configure(HistoryConfig {
            enabled: self.settings.history_enabled,
            retention_days: self.settings.history_retention_days,
        }) {
            self.feedback =
                "Не удалось передать настройки записи истории; повторите изменение".into();
        }
        if let Err(error) = self.settings.save() {
            self.last_action_error = Some(format!("Saving settings: {error}"));
            self.feedback = "Не удалось сохранить настройки. Проверьте доступ к папке настроек и повторите изменение.".into();
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
        self.settings.choose_network(enabled);
        self.network_error = None;
        if enabled {
            // Restart a failed session too: enable alone is idempotent.
            self.network.disable();
            self.network_requested_at = Some(Instant::now());
            if let Err(error) = self.network.enable() {
                self.network_error = Some(error);
            }
        } else {
            self.network_requested_at = None;
            self.network.disable();
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
            ui.label(
                egui::RichText::new("Ресурсы и история")
                    .small()
                    .color(MUTED),
            );
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
            ui.selectable_value(&mut self.screen, Screen::Overview, "Обзор");
            ui.selectable_value(&mut self.screen, Screen::Processes, "Процессы");
            ui.selectable_value(&mut self.screen, Screen::History, "История");
            ui.selectable_value(&mut self.screen, Screen::Compare, "Сравнение");
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
            let history_status = &self.history_service.view().status;
            ui.separator();
            ui.colored_label(
                if history_status.error.is_some() {
                    crate::ui::RAM
                } else if self.paused || !history_status.enabled {
                    MUTED
                } else {
                    crate::ui::history::NETWORK
                },
                if history_status.error.is_some() {
                    "Ошибка истории"
                } else if !history_status.ready {
                    "История запускается…"
                } else if self.paused {
                    "Запись на паузе"
                } else if history_status.enabled {
                    "История записывается"
                } else {
                    "Запись выключена"
                },
            );
            if history_status.dropped_samples > 0 {
                ui.colored_label(
                    crate::ui::RAM,
                    format!("Пропущено измерений: {}", history_status.dropped_samples),
                );
            }
            if self.tray.is_none() {
                ui.separator();
                ui.label(egui::RichText::new("Трей недоступен").color(MUTED));
                if ui.small_button("Подробнее").clicked() {
                    self.diagnostics_open = true;
                }
            }
        });
        if !self.feedback.is_empty() {
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Скрыть сообщение").clicked() {
                    self.feedback.clear();
                }
                ui.label(&self.feedback);
                if (self.last_action_error.is_some() || self.history_ui.diagnostic_error.is_some())
                    && ui.small_button("Подробнее").clicked()
                {
                    self.diagnostics_open = true;
                }
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

    fn network_controls(&mut self, ui: &mut egui::Ui, historical: bool) {
        if self.settings.should_offer_network_setup() {
            egui::Frame::group(ui.style()).fill(ACCENT.gamma_multiply(0.09)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.heading("Полный мониторинг");
                ui.label("Узнайте, какие приложения используют интернет. Сейчас доступен только общий трафик компьютера.");
                #[cfg(target_os = "windows")]
                ui.label("Нажмите «Включить» и подтвердите запрос Windows для сбора трафика.");
                #[cfg(target_os = "linux")]
                ui.label("Для сбора трафика потребуется подтверждение прав администратора.");
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(egui::RichText::new("Включить").strong()).fill(ui.visuals().selection.bg_fill).stroke(egui::Stroke::new(1.0, ACCENT))).clicked() { self.set_network(true); }
                    if ui.button("Пока пропустить").clicked() {
                        self.settings.choose_network(false);
                        self.save_settings();
                    }
                });
            });
        } else {
            let shared = self.network.shared();
            let state = shared.lock().unwrap_or_else(|error| error.into_inner());
            let available = state.has_fresh_data(Duration::from_secs(7));
            let waiting = self.network_error.is_none()
                && (state.status.contains("Ожидание") || state.status.contains("ожидание"))
                && self.network_requested_at.is_some();
            drop(state);
            ui.horizontal_wrapped(|ui| {
                if !self.settings.network_enabled() {
                    ui.colored_label(MUTED, "Трафик по приложениям выключен");
                    if ui.small_button("Включить").clicked() {
                        self.set_network(true);
                    }
                } else if available {
                    ui.colored_label(ACCENT, "Трафик по приложениям включён");
                } else if waiting {
                    ui.spinner();
                    ui.label("Ожидаем подтверждения и первые данные трафика…");
                    if ui.small_button("Отменить").clicked() {
                        self.set_network(false);
                    }
                } else {
                    ui.colored_label(crate::ui::RAM, "Трафик по приложениям недоступен");
                    if ui.small_button("Повторить").clicked() {
                        self.set_network(true);
                    }
                    if ui.small_button("Подробнее").clicked() {
                        self.diagnostics_open = true;
                    }
                }
            });
        }
        if historical && !self.settings.network_enabled() {
            ui.label(egui::RichText::new("«—» — трафик в этом интервале не измерялся. Включение действует на новые измерения.").small().color(MUTED));
        }
        ui.add_space(8.0);
    }

    #[cfg(target_os = "windows")]
    fn poll_startup(&mut self) {
        if let Some(result) = self.startup.poll() {
            self.startup_error =
                reconcile_startup(&mut self.startup_current, &mut self.startup_draft, result);
        }
    }

    #[cfg(target_os = "windows")]
    fn startup_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Запуск вместе с Windows");
        let busy = self.startup.is_busy();
        let mut enabled = self.startup_current.is_some_and(|state| state.enabled);
        if ui
            .add_enabled(
                !busy,
                egui::Checkbox::new(&mut enabled, "Запускать при входе в Windows"),
            )
            .changed()
        {
            self.startup_draft = crate::autostart::State {
                enabled,
                elevated: false,
                needs_repair: false,
            };
            self.startup_apply_requested = true;
            self.startup.configure(self.startup_draft);
        }
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.startup_apply_requested {
                    if self.startup_draft.enabled {
                        "Подтверждаем включение автозапуска в Windows…"
                    } else {
                        "Подтверждаем выключение автозапуска в Windows…"
                    }
                } else {
                    "Проверяем состояние в Windows…"
                });
            });
        }
        if self.startup_error.is_some() && self.startup_current.is_some() {
            ui.label("Последнее подтверждённое состояние:");
        }
        match self.startup_current {
            Some(state) if state.enabled && state.elevated => {
                ui.label("Запуск с правами администратора включён");
            }
            Some(state) if state.enabled => {
                ui.label("Автозапуск включён");
            }
            Some(_) => {
                ui.label("Автозапуск выключен");
            }
            None => {
                ui.label("Состояние автозапуска ещё не подтверждено");
            }
        }
        if let Some(error) = &self.startup_error {
            ui.colored_label(
                crate::ui::RAM,
                error
                    .lines()
                    .next()
                    .unwrap_or("Не удалось настроить автозапуск"),
            );
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(if self.startup_apply_requested {
                            "Повторить изменение"
                        } else {
                            "Повторить проверку"
                        }),
                    )
                    .clicked()
                {
                    if self.startup_apply_requested {
                        self.startup.configure(self.startup_draft);
                    } else {
                        self.startup.query();
                    }
                }
                if ui.small_button("Подробнее").clicked() {
                    self.diagnostics_open = true;
                }
            });
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Проверить состояние"))
            .clicked()
        {
            self.startup_apply_requested = false;
            self.startup.query();
        }
        if let Some(actual) = self
            .startup_current
            .filter(|state| state.enabled && state.needs_repair)
        {
            ui.horizontal_wrapped(|ui| {
                ui.label("Автозапуск использует другую копию приложения");
                if ui
                    .add_enabled(!busy, egui::Button::new("Обновить автозапуск"))
                    .clicked()
                {
                    self.startup_draft = crate::autostart::State {
                        needs_repair: false,
                        ..actual
                    };
                    self.startup_apply_requested = true;
                    self.startup.configure(self.startup_draft);
                }
            });
        }
        egui::CollapsingHeader::new("Дополнительный режим").show(ui, |ui| {
            ui.label("Запуск с правами администратора: Windows установит защищённую копию приложения. Повышенные права получит всё приложение.");
            if ui.add_enabled(!busy, egui::Button::new("Настроить запуск с правами администратора…")).clicked() {
                self.startup_draft = crate::autostart::State { enabled: true, elevated: true, needs_repair: false };
                self.startup_apply_requested = true;
                self.startup.configure(self.startup_draft);
            }
        });
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Настройки");
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading("Окно и запуск");
            if ui
                .add_enabled(
                    self.tray.is_some(),
                    egui::Checkbox::new(
                        &mut self.settings.close_to_tray,
                        "При закрытии оставлять работать в трее",
                    ),
                )
                .changed()
            {
                self.save_settings();
            }
            if self.tray.is_none() {
                ui.label("Трей недоступен: закрытие окна завершит программу.");
            }
            #[cfg(target_os = "windows")]
            {
                ui.add_space(8.0);
                self.startup_settings(ui);
            }
        });
        ui.add_space(10.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading("Мониторинг");
            let previous = self.settings.poll_interval_ms;
            ui.horizontal(|ui| {
                ui.label("Обновлять каждые");
                egui::ComboBox::from_id_salt("sampling-interval")
                    .selected_text(format!("{} с", previous / 1000))
                    .show_ui(ui, |ui| {
                        for value in [1000, 2000, 5000] {
                            ui.selectable_value(
                                &mut self.settings.poll_interval_ms,
                                value,
                                format!("{} с", value / 1000),
                            );
                        }
                    });
            });
            if previous != self.settings.poll_interval_ms {
                self.save_settings();
            }
            let mut enabled = self.settings.network_enabled();
            if ui.checkbox(&mut enabled, "Трафик по приложениям").changed() {
                self.set_network(enabled);
            }
            if !enabled {
                ui.label("Включение потребует подтверждения прав для сбора трафика.");
            }
        });
        ui.add_space(10.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading("История");
            let mut changed = ui
                .checkbox(&mut self.settings.history_enabled, "Записывать историю")
                .changed();
            ui.horizontal(|ui| {
                ui.label("Хранить");
                let old = self.settings.history_retention_days;
                egui::ComboBox::from_id_salt("history-retention")
                    .selected_text(format!("{old} дн."))
                    .show_ui(ui, |ui| {
                        for days in [1, 3, 7] {
                            ui.selectable_value(
                                &mut self.settings.history_retention_days,
                                days,
                                format!("{days} дн."),
                            );
                        }
                    });
                changed |= old != self.settings.history_retention_days;
            });
            if changed {
                self.save_settings();
                self.history_ui.invalidate();
            }
            if self.history_service.view().status.error.is_some() {
                ui.horizontal(|ui| {
                    ui.colored_label(crate::ui::RAM, "Не удалось записать историю");
                    if ui.small_button("Подробнее").clicked() {
                        self.diagnostics_open = true;
                    }
                });
            }
            if self.confirm_clear_history {
                ui.colored_label(
                    crate::ui::RAM,
                    "Удалить всю историю? Восстановить её нельзя.",
                );
                ui.horizontal(|ui| {
                    if ui.button("Удалить").clicked() {
                        if self.history_service.clear() {
                            self.history_ui.cleared();
                            self.feedback = "Запрошено удаление истории".into();
                        } else {
                            self.feedback = "Не удалось удалить историю. Повторите попытку.".into();
                        }
                        self.confirm_clear_history = false;
                    }
                    if ui.button("Отмена").clicked() {
                        self.confirm_clear_history = false;
                    }
                });
            } else if ui.button("Очистить историю…").clicked() {
                self.confirm_clear_history = true;
            }
        });
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("PC Activity Scanner {}", env!("CARGO_PKG_VERSION")));
            if ui.button("Диагностика").clicked() {
                self.diagnostics_open = true;
            }
        });
    }

    fn diagnostics(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        if !self.diagnostics_open {
            return;
        }
        let history = self.history_service.view();
        let backend = self.network.shared();
        let backend = backend.lock().unwrap_or_else(|error| error.into_inner());
        #[allow(unused_mut)]
        let mut report = format!(
            "PC Activity Scanner {}\nOS: {}\nExecutable: {:?}\nNetwork preference: {}\nNetwork: {}\nCollector network: {}\nNetwork error: {}\nGPU: {}\nTray: {}\nHistory: {:?}\nHistory database: {:?}\nSettings: {:?}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::current_exe().ok(),
            self.settings.network_enabled(),
            backend.status(),
            snap.network_status,
            self.network_error.as_deref().unwrap_or("none"),
            snap.gpu_status,
            self.tray_error.as_deref().unwrap_or("available"),
            history.status,
            history.status.database_path,
            Settings::file_path()
        );
        drop(backend);
        report.push_str(&format!(
            "\nHistory query error: {:?}\nHistory UI error: {:?}\nLast action error: {:?}",
            history.query_error, self.history_ui.diagnostic_error, self.last_action_error
        ));
        #[cfg(target_os = "windows")]
        {
            report.push_str(&format!(
                "\nStartup actual: {:?}\nStartup draft: {:?}\nStartup error: {}",
                self.startup_current,
                self.startup_draft,
                self.startup_error.as_deref().unwrap_or("none")
            ));
        }
        egui::Window::new("Диагностика")
            .open(&mut self.diagnostics_open)
            .default_width(650.0)
            .default_height(400.0)
            .show(ctx, |ui| {
                ui.label("Технические сведения для проверки работы приложения");
                if ui.button("Копировать сведения").clicked() {
                    ui.ctx().copy_text(report.clone());
                }
                egui::ScrollArea::both().show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut report)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .interactive(false),
                    );
                });
            });
    }

    fn process_screen(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        ui::summary(ui, snapshot, &self.history);
        self.network_controls(ui, false);
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
                    Ok(path) => {
                        self.last_action_error = None;
                        format!("Таблица сохранена: {}", path.display())
                    }
                    Err(error) => {
                        self.last_action_error = Some(format!("Exporting process CSV: {error}"));
                        "Не удалось сохранить CSV. Проверьте доступ к папке и повторите экспорт."
                            .into()
                    }
                };
            }
            if ui.small_button("Диагностика").clicked() {
                self.diagnostics_open = true;
            }
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
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F12)) {
            self.diagnostics_open = !self.diagnostics_open;
        }
        #[cfg(target_os = "windows")]
        self.poll_startup();
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
        if std::mem::take(&mut self.history_ui.diagnostics_requested) {
            self.diagnostics_open = true;
        }
        self.diagnostics(ui.ctx(), &snap);
        egui::Panel::bottom("status-bar").show(ui, |ui| self.status_bar(ui, &snap));
        egui::Panel::top("navigation").show(ui, |ui| self.header(ui));
        self.history_ui.poll_export();
        if let Some(message) = self.history_ui.feedback.take() {
            self.last_action_error = None;
            self.feedback = message;
        }
        egui::CentralPanel::default().show(ui, |ui| match self.screen {
            Screen::Overview => {
                ui.label(
                    egui::RichText::new(if self.paused {
                        "Последнее измерение · пауза"
                    } else {
                        "Сейчас"
                    })
                    .strong()
                    .color(MUTED),
                );
                self.network_controls(ui, true);
                ui::live_summary(ui, &snap);
                ui.add_space(12.0);
                self.history_ui
                    .show(ui, &self.history_service, HistoryMode::Overview)
            }
            Screen::History => {
                self.network_controls(ui, true);
                self.history_ui
                    .show(ui, &self.history_service, HistoryMode::Applications)
            }
            Screen::Compare => {
                self.network_controls(ui, true);
                self.history_ui
                    .show(ui, &self.history_service, HistoryMode::Compare)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_startup_change_keeps_confirmed_state_and_retry_draft() {
        let confirmed = crate::autostart::State {
            enabled: false,
            elevated: false,
            needs_repair: false,
        };
        let requested = crate::autostart::State {
            enabled: true,
            ..confirmed
        };
        let mut actual = Some(confirmed);
        let mut draft = requested;
        let error = reconcile_startup(
            &mut actual,
            &mut draft,
            Err(crate::autostart::Error {
                message: "Cancelled".into(),
                details: "raw diagnostic".into(),
                state: None,
            }),
        );
        assert_eq!(actual, Some(confirmed));
        assert_eq!(draft, requested);
        assert!(error.is_some());
        assert!(reconcile_startup(&mut actual, &mut draft, Ok(requested)).is_none());
        assert_eq!(actual, Some(requested));
    }

    #[test]
    fn failed_startup_change_uses_requeried_state_without_losing_requested_retry() {
        let requested = crate::autostart::State {
            enabled: true,
            elevated: true,
            needs_repair: false,
        };
        let observed = crate::autostart::State {
            elevated: false,
            ..requested
        };
        let mut actual = None;
        let mut draft = requested;
        reconcile_startup(
            &mut actual,
            &mut draft,
            Err(crate::autostart::Error {
                message: "Failed".into(),
                details: String::new(),
                state: Some(observed),
            }),
        );
        assert_eq!(actual, Some(observed));
        assert_eq!(draft, requested);
    }
}
