//! Desktop presentation and user commands. Sampling stays on background threads.
use crate::collector::Collector;
use crate::export::export_csv;
use crate::history::{HistoryService, types::HistoryConfig};
use crate::model::{Filters, ProcessIdentity, ProcessView, Snapshot, SortOrder};
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
    connections: crate::connections::Connections,
    connections_open: bool,
    alerts_open: bool,
    last_alert_id: Option<u64>,
    alert_toast: Option<(Instant, crate::alerts::AlertEvent)>,
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
    exit_requested_at: Option<Instant>,
    paused: bool,
    selected_process: Option<ProcessIdentity>,
    detailed_metrics: bool,
    feedback: String,
    process_export: Option<std::sync::mpsc::Receiver<Result<std::path::PathBuf, String>>>,
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
            settings.alerts.clone(),
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
            connections: crate::connections::Connections::default(),
            connections_open: false,
            alerts_open: false,
            last_alert_id: None,
            alert_toast: None,
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
            exit_requested_at: None,
            paused: false,
            selected_process: None,
            detailed_metrics: false,
            feedback,
            process_export: None,
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
        self.collector
            .set_alert_config(self.settings.alerts.clone());
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

    fn poll_process_export(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.process_export else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Export worker disconnected".into()),
        };
        self.process_export = None;
        match result {
            Ok(path) => {
                self.last_action_error = None;
                self.feedback = format!("Таблица сохранена: {}", path.display());
            }
            Err(error) => {
                self.last_action_error = Some(error);
                self.feedback = "Не удалось сохранить CSV. Подробности в диагностике.".into();
            }
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
        if !self.exiting {
            self.exiting = true;
            self.exit_requested_at = Some(Instant::now());
            self.alert_toast = None;
            self.connections.select(None);
            self.collector.shutdown();
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn shutdown_ready(&self) -> bool {
        self.collector.is_stopped()
            || self
                .exit_requested_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested()) {
            if self.settings.close_to_tray && self.tray.is_some() && !self.exiting {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.quit(ctx);
                if !self.shutdown_ready() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
            }
        }
        if self.exiting {
            if self.shutdown_ready() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
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
                    if self.paused {
                        self.alert_toast = None;
                    }
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
            let count = self
                .collector
                .alerts
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .events
                .len();
            if ui.button(format!("События ({count})")).clicked() {
                self.alerts_open = true;
            }
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
            ui.checkbox(&mut self.filters.tree, "Дерево процессов");
            ui.add_enabled(
                !self.filters.tree,
                egui::Checkbox::new(&mut self.filters.group_by_name, "Группировать по имени"),
            );
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
            ui.checkbox(&mut self.detailed_metrics, "Подробные метрики");
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

    fn alert_settings(&mut self, ui: &mut egui::Ui) {
        use crate::alerts::{AlertMetric, AlertRule};
        let previous = self.settings.alerts.clone();
        let mut show_journal = false;
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.heading("Уведомления о длительной нагрузке");
            let config = &mut self.settings.alerts;
            ui.checkbox(&mut config.enabled, "Включить правила уведомлений");
            ui.add_enabled(config.enabled, egui::Checkbox::new(&mut config.desktop, "Системные уведомления рабочего стола"));
            ui.small("Правила по процессам проверяют каждый отдельный PID и время жизни, независимо от поиска/группировки. 100% CPU — мощность всего хоста.");
            let mut remove = None;
            for (index, rule) in config.rules.iter_mut().enumerate() {
                ui.push_id(("alert-rule",index), |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut rule.enabled, format!("Правило {}", index+1));
                        let old_metric = rule.metric;
                        egui::ComboBox::from_id_salt("metric").selected_text(rule.metric.label()).show_ui(ui, |ui| {
                            for metric in AlertMetric::ALL { ui.selectable_value(&mut rule.metric, metric, metric.label()); }
                        });
                        if old_metric != rule.metric { rule.threshold = match rule.metric {
                            AlertMetric::SystemCpu | AlertMetric::ProcessCpu | AlertMetric::Gpu => 90.0,
                            AlertMetric::ResidentGrowth => 256.0*1024.0*1024.0,
                            AlertMetric::IoRead | AlertMetric::IoWrite => 10.0*1024.0*1024.0,
                            _ => 2.0*1024.0*1024.0*1024.0,
                        }; }
                        if ui.small_button("Удалить").clicked() { remove = Some(index); }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Порог");
                        let percent = rule.metric.unit() == "%";
                        let scale = if percent { 1.0 } else { 1024.0*1024.0 };
                        let mut shown = rule.threshold/scale;
                        let suffix = match rule.metric.unit() { "%" => " %", "bytes/s" => " МиБ/с", _ => " МиБ" };
                        if ui.add(egui::DragValue::new(&mut shown).range(0.1..=if percent {100.0} else {1_000_000_000.0}).suffix(suffix).speed(if percent {0.5} else {1.0})).changed() { rule.threshold = shown*scale; }
                        ui.label("Длительность"); ui.add(egui::DragValue::new(&mut rule.duration_secs).range(1..=3600).suffix(" с"));
                        ui.label("Повтор не чаще"); ui.add(egui::DragValue::new(&mut rule.cooldown_secs).range(1..=86400).suffix(" с"));
                    });
                    if rule.metric == AlertMetric::ResidentGrowth { ui.small("Рост resident без снижения: равные значения допускаются; снижение начинает новый интервал. После события нужен новый рост."); }
                    if matches!(rule.metric, AlertMetric::IoRead | AlertMetric::IoWrite) { ui.small("Windows I/O включает file/network/device, а не физический трафик конкретного накопителя."); }
                    ui.separator();
                });
            }
            if let Some(index) = remove { config.rules.remove(index); }
            ui.horizontal(|ui| {
                if ui.add_enabled(config.rules.len()<16, egui::Button::new("Добавить правило")).clicked() { config.rules.push(AlertRule::default()); }
                if ui.button("Журнал событий").clicked() { show_journal = true; }
            });
            ui.small("Паузы, пропуски и неизвестные данные прерывают подтверждение. Событие сообщает измерение и интервал, а не доказанную причину тормозов.");
        });
        if self.settings.alerts != previous {
            self.save_settings();
        }
        if show_journal {
            self.alerts_open = true;
        }
        let report = self
            .collector
            .alerts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        ui.label(if self.settings.alerts.enabled {
            report.status.as_str()
        } else {
            "Правила выключены"
        });
        let delivery = self.collector.notification_status();
        ui.small(format!(
            "Рабочий стол: принято службой {}, пропущено {}",
            delivery.accepted, delivery.dropped
        ));
        if let Some(error) = delivery.error {
            ui.label(format!(
                "Системное уведомление недоступно: {error}. События остаются в журнале."
            ));
        }
    }

    fn alert_windows(&mut self, ctx: &egui::Context) {
        let report = self
            .collector
            .alerts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(event) = report.events.last()
            && self.last_alert_id != Some(event.id)
        {
            self.last_alert_id = Some(event.id);
            if self.settings.alerts.enabled && !self.paused {
                self.alert_toast = Some((Instant::now(), event.clone()));
            }
        }
        let mut show_journal = false;
        let mut dismiss = false;
        if let Some((at, event)) = &self.alert_toast
            && at.elapsed() < Duration::from_secs(8)
            && !self.paused
        {
            egui::Area::new(egui::Id::unique("resource-alert-toast"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-15.0, -50.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::window(ui.style()).show(ui, |ui| {
                        ui.set_max_width(480.0);
                        ui.strong(event.title());
                        ui.label(event.body());
                        ui.horizontal(|ui| {
                            if ui.button("Журнал").clicked() {
                                show_journal = true;
                            }
                            if ui.small_button("Закрыть").clicked() {
                                dismiss = true;
                            }
                        });
                    });
                });
        }
        if dismiss
            || self
                .alert_toast
                .as_ref()
                .is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(8))
        {
            self.alert_toast = None;
        }
        if show_journal {
            self.alerts_open = true;
        }
        egui::Window::new("События нагрузки").open(&mut self.alerts_open).default_width(760.0).default_height(420.0).show(ctx, |ui| {
            ui.small("Последние 100 событий текущего сеанса. Настройки правил — на вкладке «Настройки». Содержимое журнала не записывается в SQLite.");
            ui.label(&report.status);
            if report.events.is_empty() { ui.label("Событий пока нет. Уведомления по умолчанию выключены."); }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for event in report.events.iter().rev() {
                    let local = |stamp| chrono::DateTime::from_timestamp(stamp,0).map(|t| t.with_timezone(&chrono::Local).format("%d.%m %H:%M:%S").to_string()).unwrap_or_else(|| stamp.to_string());
                    egui::CollapsingHeader::new(format!("{} · {} · {}", local(event.ended_at), event.name, event.metric.label())).id_salt(event.id).show(ui, |ui| {
                        ui.label(event.body());
                        ui.label(format!("Интервал {} — {} ({:.1} с)", local(event.started_at), local(event.ended_at), event.duration_secs));
                        ui.small(format!("Правило {}; порог {}",event.rule_index+1,event.metric.format(event.threshold)));
                        if let Some(process) = event.process { ui.label(format!("PID {}, запуск {}", process.pid, local(process.started_at as i64))); }
                    });
                }
            });
        });
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Настройки");
        self.alert_settings(ui);
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

    fn connection_window(&mut self, ctx: &egui::Context, snapshot: &Snapshot) {
        let selected = self.selected_process.filter(|identity| {
            snapshot
                .process_rows
                .iter()
                .any(|row| row.identity() == *identity)
        });
        self.connections
            .select(if self.connections_open && !self.paused {
                selected
            } else {
                None
            });
        if !self.connections_open {
            return;
        }
        let view = self.connections.view();
        egui::Window::new("Соединения выбранного процесса")
            .id(egui::Id::unique("process-connections"))
            .open(&mut self.connections_open)
            .default_width(820.0).default_height(320.0)
            .show(ctx, |ui| {
                let Some(identity) = selected else { ui.label("Выберите отдельный процесс на вкладке «Процессы». Завершённый процесс не заменяется новым с тем же PID."); return; };
                ui.strong(format!("PID {} · начало {}", identity.pid, identity.started_at));
                ui.small("TCP/UDP, IPv4/IPv6. Без DNS, содержимого пакетов и неподтверждённых скоростей соединений.");
                if self.paused { ui.label("Пауза: соединения не обновляются."); return; }
                if view.identity != Some(identity) { ui.spinner(); return; }
                ui.label(&view.status);
                if view.updated_at.is_none_or(|at| at.elapsed() > Duration::from_secs(7)) {
                    ui.label("Ожидаем свежий результат."); return;
                }
                use egui_extras::{Column, TableBuilder};
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.set_min_width(720.0);
                    TableBuilder::new(ui).id_salt("connection-table").striped(true)
                        .column(Column::initial(70.0)).column(Column::remainder()).column(Column::remainder()).column(Column::initial(130.0))
                        .header(26.0, |mut header| { for title in ["Протокол", "Локальный адрес", "Удалённый адрес", "TCP состояние"] { header.col(|ui| { ui.strong(title); }); } })
                        .body(|body| body.rows(25.0, view.rows.len(), |mut row| {
                            let connection = &view.rows[row.index()];
                            for text in [connection.protocol.as_str(), &connection.local, connection.remote.as_deref().unwrap_or("—"), connection.state.as_deref().unwrap_or("—")] {
                                row.col(|ui| { ui.add(egui::Label::new(text).truncate()).on_hover_text(text); });
                            }
                        }));
                });
            });
        if !self.connections_open {
            self.connections.select(None);
        }
    }

    fn process_screen(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        ui::summary(ui, snapshot, &self.history);
        self.network_controls(ui, false);
        self.filters(ui);
        if self.filters.tree {
            ui.small("Дерево родства. Поиск и пороги отбирают процессы; скрытый или завершившийся родитель не добавляется.");
        } else if self.filters.group_by_name {
            ui.small("Нажмите группу для раскрытия. Поиск отбирает участников; пороги относятся к сводке группы.");
        }
        if !snapshot.process_status.is_empty() {
            ui.label(&snapshot.process_status);
        }
        let rows = self.process_view.rows(snapshot, &self.filters, self.sort);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{}: {} / {} процессов",
                    "Показано строк",
                    rows.len(),
                    snapshot.process_rows.len()
                ))
                .color(MUTED),
            );
            if ui
                .add_enabled(!rows.is_empty() && self.process_export.is_none(), egui::Button::new(if self.process_export.is_some() { "Сохранение…" } else { "Экспорт CSV" }))
                .on_hover_text("Видимые строки. В раскрытых группах row_kind=group — сводка, row_kind=process — участник; не складывайте сводку с участниками.")
                .clicked()
            {
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                let export_rows = Arc::clone(&rows);
                std::thread::spawn(move || { let _ = tx.send(export_csv(&export_rows)); });
                self.process_export = Some(rx);
            }
            if ui.small_button("Диагностика").clicked() {
                self.diagnostics_open = true;
            }
        });
        if let Some(selected) = self.selected_process {
            if let Some(row) = snapshot
                .process_rows
                .iter()
                .find(|r| r.identity() == selected)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("PID {}: {}", row.pid, row.name));
                    ui.label(row.parent.map_or_else(
                        || "Родитель: неизвестен / завершён".into(),
                        |parent| format!("Родитель PID {}", parent.pid),
                    ));
                    if !row.executable.is_empty() && ui.small_button("Копировать путь").clicked()
                    {
                        ui.ctx().copy_text(row.executable.clone());
                    }
                    if ui.button("Соединения").clicked() {
                        self.connections_open = true;
                    }
                    if ui.small_button("Снять выделение").clicked() {
                        self.selected_process = None;
                    }
                });
            } else {
                self.selected_process = None;
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
                &mut self.selected_process,
                self.detailed_metrics,
                &mut self.process_view,
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
        self.poll_process_export();
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
        if self.exiting {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.spinner();
                ui.label("Завершаем сбор и доставку уведомлений…");
            });
            ui.ctx().request_repaint_after(Duration::from_millis(50));
            return;
        }
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
        self.connection_window(ui.ctx(), &snap);
        self.alert_windows(ui.ctx());
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
