//! Query orchestration and historical presentation. No database or sampling access.
use super::{ACCENT, MUTED, RAM};
use crate::history::{HistoryService, types::*};
use crate::history_export::export_history;
use crate::model::{format_bytes, format_rate};
use chrono::{DateTime, Local};
use eframe::egui;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub const GPU: egui::Color32 = egui::Color32::from_rgb(178, 153, 248);
pub const NETWORK: egui::Color32 = egui::Color32::from_rgb(95, 211, 163);
const APP_COLORS: [egui::Color32; 4] = [ACCENT, RAM, GPU, NETWORK];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HistoryMode {
    Overview,
    Applications,
    Compare,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ranking {
    Cpu,
    Contribution,
    Ram,
    Traffic,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Cpu,
    Ram,
    Gpu,
    Download,
    Upload,
}
impl Metric {
    fn value(self, p: &HistoryPoint) -> Option<f64> {
        match self {
            Self::Cpu => Some(p.cpu_avg),
            Self::Ram => Some(p.ram_avg),
            Self::Gpu => p.gpu_avg,
            Self::Download => p.down_bps,
            Self::Upload => p.up_bps,
        }
    }
    fn observed(self, point: &HistoryPoint) -> f64 {
        match self {
            Self::Gpu => point.gpu_observed_secs,
            Self::Download | Self::Upload => point.network_observed_secs,
            _ => point.duration_secs,
        }
    }
    fn format(self, value: f64) -> String {
        match self {
            Self::Cpu | Self::Gpu => format!("{value:.1}%"),
            Self::Ram => format_bytes(value.max(0.0) as u64),
            Self::Download | Self::Upload => format_rate(Some(value)),
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
struct QueryIntent {
    seconds: i64,
    end: Option<i64>,
    mode: HistoryMode,
    ids: Vec<i64>,
}
pub struct HistoryUi {
    seconds: i64,
    end: Option<i64>,
    // Every rendered value and export uses this exact query/snapshot pair.
    query: HistoryQuery,
    requested_query: HistoryQuery,
    displayed: Option<Arc<HistoryView>>,
    intent: Option<QueryIntent>,
    pending_zoom: Option<(i64, i64)>,
    last_request: Option<Instant>,
    search: String,
    ranking: Ranking,
    selected: Option<i64>,
    compare: Vec<i64>,
    compare_metric: Metric,
    export: Option<mpsc::Receiver<Result<std::path::PathBuf, String>>>,
    pub feedback: Option<String>,
}
impl Default for HistoryUi {
    fn default() -> Self {
        Self {
            seconds: 15 * 60,
            end: None,
            query: HistoryQuery::default(),
            requested_query: HistoryQuery::default(),
            displayed: None,
            intent: None,
            pending_zoom: None,
            last_request: None,
            search: String::new(),
            ranking: Ranking::Contribution,
            selected: None,
            compare: Vec::new(),
            compare_metric: Metric::Cpu,
            export: None,
            feedback: None,
        }
    }
}
impl HistoryUi {
    pub fn invalidate(&mut self) {
        self.last_request = None;
    }
    pub fn cleared(&mut self) {
        self.selected = None;
        self.compare.clear();
        self.displayed = None;
        self.invalidate();
    }
    fn request(&mut self, service: &HistoryService, mode: HistoryMode) {
        let ids = match mode {
            HistoryMode::Applications => self.selected.into_iter().collect(),
            HistoryMode::Compare => self.compare.clone(),
            HistoryMode::Overview => vec![],
        };
        let intent = QueryIntent {
            seconds: self.seconds,
            end: self.end,
            mode,
            ids,
        };
        self.set_intent(intent.clone());
        let refresh = self.last_request.is_none()
            || (self.end.is_none()
                && self
                    .last_request
                    .is_some_and(|t| t.elapsed() >= Duration::from_secs(15)));
        if !refresh {
            return;
        }
        let (from, to) = bucket_window(
            self.end.unwrap_or_else(|| chrono::Utc::now().timestamp()),
            self.seconds,
        );
        let query = HistoryQuery {
            request_id: self.requested_query.request_id.wrapping_add(1).max(1),
            from,
            to,
            application_ids: intent.ids,
        };
        if service.request(query.clone()) {
            self.requested_query = query;
            self.last_request = Some(Instant::now());
        } else {
            self.last_request = None;
        }
    }
    fn set_intent(&mut self, intent: QueryIntent) {
        if self.intent.as_ref() != Some(&intent) {
            // A real selection change must never display old data with new labels.
            self.displayed = None;
            self.last_request = None;
            self.intent = Some(intent);
        }
    }
    fn accept_view(&mut self, view: Arc<HistoryView>) {
        if self.last_request.is_some()
            && view.request_id == self.requested_query.request_id
            && view.status.ready
            && view.query_error.is_none()
        {
            self.query = self.requested_query.clone();
            self.displayed = Some(view);
        }
    }
    pub fn poll_export(&mut self) {
        if let Some(rx) = &self.export {
            match rx.try_recv() {
                Ok(result) => {
                    self.feedback = Some(match result {
                        Ok(path) => format!("История сохранена: {}", path.display()),
                        Err(e) => format!("Ошибка экспорта истории: {e}"),
                    });
                    self.export = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.feedback = Some("Экспорт истории прерван".into());
                    self.export = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui, service: &HistoryService, mode: HistoryMode) {
        self.poll_export();
        self.toolbar(ui);
        self.request(service, mode);
        let latest = service.view();
        self.accept_view(Arc::clone(&latest));
        let displayed = self.displayed.clone();
        let matching = displayed.is_some();
        let view = displayed.as_ref().unwrap_or(&latest);
        let (display_from, display_to) = if matching {
            (self.query.from, self.query.to)
        } else {
            bucket_window(
                self.end.unwrap_or_else(|| chrono::Utc::now().timestamp()),
                self.seconds,
            )
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{} — {}",
                    time(display_from, "%d.%m %H:%M"),
                    time(display_to, "%d.%m %H:%M %Z")
                ))
                .color(MUTED),
            );
            if matching {
                ui.label(
                    egui::RichText::new(format!(
                        "Записано {} из {} ({:.0}%)",
                        duration(view.data.recorded_secs),
                        duration((self.query.to - self.query.from) as f64),
                        coverage(view.data.recorded_secs, self.query.to - self.query.from)
                    ))
                    .small()
                    .color(MUTED),
                );
            }
            if ui
                .add_enabled(
                    matching && !view.data.systems.is_empty() && self.export.is_none(),
                    egui::Button::new(if self.export.is_some() {
                        "Сохранение…"
                    } else {
                        "Экспорт истории CSV"
                    }),
                )
                .clicked()
            {
                let (tx, rx) = mpsc::sync_channel(1);
                let query = self.query.clone();
                let view = Arc::clone(view);
                std::thread::spawn(move || {
                    let _ = tx.send(export_history(&query, &view));
                });
                self.export = Some(rx);
            }
        });
        if let (Some(first), Some(last)) =
            (view.data.earliest_timestamp, view.data.latest_timestamp)
        {
            ui.label(
                egui::RichText::new(format!(
                    "Доступная история: {} — {}",
                    time(first, "%d.%m %H:%M"),
                    time(last, "%d.%m %H:%M")
                ))
                .small()
                .color(MUTED),
            );
        }
        ui.add_space(8.0);
        if let Some(error) = &latest.status.error {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("История: {error}"));
        }
        if latest.status.error.is_some() && !latest.status.ready {
            ui.add_space(16.0);
            ui.heading("История недоступна; текущие процессы доступны");
            ui.label("Устраните указанную проблему и перезапустите приложение. Текущие измерения доступны на вкладке «Процессы».");
            return;
        }
        if latest.request_id == self.requested_query.request_id
            && let Some(error) = &latest.query_error
        {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("Запрос истории: {error}"));
        }
        if !matching
            && latest.request_id == self.requested_query.request_id
            && latest.query_error.is_some()
            && latest.status.ready
        {
            ui.label(
                "Не удалось загрузить выбранный период. Измените период или повторите запрос.",
            );
            if ui.button("Повторить").clicked() {
                self.invalidate();
            }
            return;
        }
        if !matching || !latest.status.ready {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Загрузка выбранного периода…");
            });
            return;
        }
        if view.data.systems.is_empty() && view.data.applications.is_empty() {
            ui.add_space(28.0);
            ui.heading("В этом периоде нет измерений");
            ui.label(if view.status.enabled { "История появляется во время работы приложения. Выберите другой период или дождитесь записи первого интервала." } else { "Запись выключена. Сохранённые периоды доступны; включить запись можно в настройках." });
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("historical-content")
            .auto_shrink([false, false])
            .show(ui, |ui| match mode {
                HistoryMode::Overview => self.overview(ui, &view.data),
                HistoryMode::Applications => self.applications(ui, &view.data, false),
                HistoryMode::Compare => self.comparison(ui, &view.data),
            });
        if let Some((from, to)) = self.pending_zoom.take() {
            self.seconds = to - from;
            self.end = Some(to);
            self.invalidate();
            ui.ctx().request_repaint();
        }
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Период:");
            egui::ComboBox::from_id_salt("history-period")
                .selected_text(duration(self.seconds as f64))
                .show_ui(ui, |ui| {
                    for (seconds, label) in [
                        (300, "5 минут"),
                        (900, "15 минут"),
                        (1800, "30 минут"),
                        (3600, "1 час"),
                        (21600, "6 часов"),
                        (43200, "12 часов"),
                        (86400, "24 часа"),
                    ] {
                        if ui
                            .selectable_value(&mut self.seconds, seconds, label)
                            .changed()
                        {
                            self.invalidate();
                        }
                    }
                });
            ui.separator();
            if ui
                .button("‹ Раньше")
                .on_hover_text("Предыдущий период той же длины")
                .clicked()
            {
                self.end =
                    Some(self.end.unwrap_or_else(|| chrono::Utc::now().timestamp()) - self.seconds);
                self.invalidate();
            }
            if ui
                .add_enabled(self.end.is_some(), egui::Button::new("Позже ›"))
                .on_hover_text("Следующий период той же длины")
                .clicked()
            {
                let next = self.end.unwrap() + self.seconds;
                self.end = (next < chrono::Utc::now().timestamp()).then_some(next);
                self.invalidate();
            }
            if ui
                .selectable_label(self.end.is_none(), "Сейчас")
                .on_hover_text("Следить за последними измерениями")
                .clicked()
            {
                self.end = None;
                self.invalidate();
            }
            if ui.button("Сброс: 15 минут").clicked() {
                self.seconds = 900;
                self.end = None;
                self.invalidate();
            }
        });
        ui.label(
            egui::RichText::new(
                "Выделите участок графика мышью, чтобы приблизить время на всех графиках.",
            )
            .color(MUTED),
        );
    }
    fn overview(&mut self, ui: &mut egui::Ui, data: &HistoryData) {
        ui.heading("Ресурсы за период");
        ui.label(egui::RichText::new("Средние значения по интервалам. Пробелы на графиках означают отсутствие измерений.").small().color(MUTED));
        let lines = [ChartLine {
            name: "CPU",
            points: &data.systems,
            color: ACCENT,
        }];
        plot(
            ui,
            "CPU",
            &lines,
            Metric::Cpu,
            &self.query,
            &mut self.pending_zoom,
            150.0,
        );
        ui.label(
            egui::RichText::new(format!(
                "Установлено памяти: {}",
                format_bytes(data.ram_total_bytes)
            ))
            .small()
            .color(MUTED),
        );
        plot(
            ui,
            "Оперативная память",
            &[ChartLine {
                name: "RAM",
                points: &data.systems,
                color: RAM,
            }],
            Metric::Ram,
            &self.query,
            &mut self.pending_zoom,
            130.0,
        );
        ui.columns(2, |cols| {
            plot(
                &mut cols[0],
                "Получение",
                &[ChartLine {
                    name: "Получение",
                    points: &data.systems,
                    color: NETWORK,
                }],
                Metric::Download,
                &self.query,
                &mut self.pending_zoom,
                110.0,
            );
            plot(
                &mut cols[1],
                "Отправка",
                &[ChartLine {
                    name: "Отправка",
                    points: &data.systems,
                    color: NETWORK,
                }],
                Metric::Upload,
                &self.query,
                &mut self.pending_zoom,
                110.0,
            );
        });
        ui.add_space(12.0);
        ui.heading("Приложения с наибольшим вкладом CPU");
        let mut apps: Vec<_> = data.applications.iter().collect();
        apps.sort_by(|a, b| b.cpu_seconds.total_cmp(&a.cpu_seconds));
        let total = apps.iter().map(|a| a.cpu_seconds).sum::<f64>();
        for app in apps.iter().take(5) {
            ui.horizontal(|ui| {
                ui.add_sized([190.0, 22.0], egui::Label::new(&app.name).truncate())
                    .on_hover_text(&app.executable);
                ui.add(
                    egui::ProgressBar::new(if total > 0.0 {
                        (app.cpu_seconds / total) as f32
                    } else {
                        0.0
                    })
                    .desired_width(180.0)
                    .fill(ACCENT)
                    .text(format!(
                        "{:.1}% вклада",
                        if total > 0.0 {
                            app.cpu_seconds / total * 100.0
                        } else {
                            0.0
                        }
                    )),
                );
                ui.label(format!(
                    "сред. {:.1}%   пик {:.1}%",
                    app.cpu_avg, app.cpu_peak
                ));
            });
        }
    }
    fn applications(&mut self, ui: &mut egui::Ui, data: &HistoryData, comparing: bool) {
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Поиск приложения или пути…")
                    .desired_width(260.0),
            );
            egui::ComboBox::from_id_salt("history-ranking")
                .selected_text(match self.ranking {
                    Ranking::Cpu => "CPU: среднее",
                    Ranking::Contribution => "CPU: вклад",
                    Ranking::Ram => "RAM: пик",
                    Ranking::Traffic => "Трафик",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.ranking, Ranking::Cpu, "CPU: среднее");
                    ui.selectable_value(&mut self.ranking, Ranking::Contribution, "CPU: вклад");
                    ui.selectable_value(&mut self.ranking, Ranking::Ram, "RAM: пик");
                    ui.selectable_value(&mut self.ranking, Ranking::Traffic, "Трафик");
                });
            ui.label(
                egui::RichText::new(format!(
                    "{} приложений, включая завершённые",
                    data.applications.len()
                ))
                .small()
                .color(MUTED),
            );
        });
        ui.label(egui::RichText::new("Средние CPU/RAM рассчитаны за время наблюдения; покрытие — доля выбранного периода.").small().color(MUTED));
        let total_cpu = data
            .applications
            .iter()
            .map(|app| app.cpu_seconds)
            .sum::<f64>();
        let query = self.search.to_lowercase();
        let mut apps: Vec<_> = data
            .applications
            .iter()
            .filter(|a| {
                a.name.to_lowercase().contains(&query)
                    || a.executable.to_lowercase().contains(&query)
            })
            .collect();
        apps.sort_by(|a, b| {
            let score = |a: &ApplicationSummary| match self.ranking {
                Ranking::Cpu => a.cpu_avg,
                Ranking::Contribution => a.cpu_seconds,
                Ranking::Ram => a.ram_peak as f64,
                Ranking::Traffic => {
                    a.downloaded_bytes.unwrap_or(0.0) + a.uploaded_bytes.unwrap_or(0.0)
                }
            };
            score(b).total_cmp(&score(a)).then_with(|| a.id.cmp(&b.id))
        });
        if !comparing
            && self.selected.is_none()
            && let Some(first) = apps.first()
        {
            self.selected = Some(first.id);
            self.invalidate();
            ui.ctx().request_repaint();
        }
        ui.add_space(6.0);
        egui::ScrollArea::horizontal()
            .id_salt("history-ranking-x")
            .show(ui, |ui| {
                ui.set_min_width(1000.0);
                use egui_extras::{Column, TableBuilder};
                TableBuilder::new(ui)
                    .id_salt("history-ranking-table")
                    .striped(true)
                    .resizable(true)
                    .max_scroll_height(if comparing { 180.0 } else { 250.0 })
                    .column(Column::initial(210.0).at_least(170.0))
                    .column(Column::initial(135.0))
                    .column(Column::initial(95.0))
                    .column(Column::initial(160.0))
                    .column(Column::initial(155.0))
                    .column(Column::initial(140.0))
                    .header(36.0, |mut header| {
                        for text in [
                            "Приложение / путь",
                            "CPU сред. / пик",
                            "Вклад CPU",
                            "RAM сред. / пик",
                            "Получено / отдано",
                            "Время / покрытие",
                        ] {
                            header.col(|ui| {
                                ui.strong(text);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(36.0, apps.len(), |mut row| {
                            let app = apps[row.index()];
                            row.col(|ui| {
                                let selected = if comparing {
                                    self.compare.contains(&app.id)
                                } else {
                                    self.selected == Some(app.id)
                                };
                                let response = ui.add_enabled(
                                    !comparing
                                        || selected
                                        || self.compare.len() < MAX_COMPARISON_APPS,
                                    egui::Button::selectable(selected, &app.name).truncate(),
                                );
                                if response
                                    .on_hover_text(if app.executable.is_empty() {
                                        "Путь недоступен; идентификация по имени"
                                    } else {
                                        &app.executable
                                    })
                                    .clicked()
                                {
                                    if comparing {
                                        if selected {
                                            self.compare.retain(|id| *id != app.id);
                                        } else {
                                            self.compare.push(app.id);
                                        }
                                    } else {
                                        self.selected = Some(app.id);
                                    }
                                    self.invalidate();
                                }
                            });
                            row.col(|ui| {
                                ui.colored_label(
                                    ACCENT,
                                    format!("{:.1} / {:.1}%", app.cpu_avg, app.cpu_peak),
                                );
                            })
                            .1
                            .on_hover_text(format!("Вклад: {:.1} CPU·с", app.cpu_seconds));
                            row.col(|ui| {
                                ui.label(format!(
                                    "{:.1}%",
                                    if total_cpu > 0.0 {
                                        app.cpu_seconds / total_cpu * 100.0
                                    } else {
                                        0.0
                                    }
                                ));
                            })
                            .1
                            .on_hover_text(
                                "Доля CPU относительно всех приложений за выбранный период",
                            );
                            row.col(|ui| {
                                ui.colored_label(
                                    RAM,
                                    format!(
                                        "{} / {}",
                                        format_bytes(app.ram_avg as u64),
                                        format_bytes(app.ram_peak)
                                    ),
                                );
                            });
                            row.col(|ui| {
                                ui.colored_label(
                                    NETWORK,
                                    format!(
                                        "{} / {}",
                                        optional_bytes(app.downloaded_bytes),
                                        optional_bytes(app.uploaded_bytes)
                                    ),
                                );
                            })
                            .1
                            .on_hover_text(if app.network_observed_secs > 0.0 {
                                format!("Сеть наблюдалась {}. Сумма относится только к измеренным интервалам.", duration(app.network_observed_secs))
                            } else {
                                "Трафик этого приложения не измерялся в выбранном периоде. Включите сбор сети в настройках; прошлый трафик восстановить нельзя.".into()
                            });
                            row.col(|ui| {
                                ui.label(format!(
                                    "{} / {:.1}%",
                                    duration(app.observed_secs),
                                    coverage(app.observed_secs, self.query.to - self.query.from)
                                ));
                            });
                        });
                    });
            });
        if apps.is_empty() {
            ui.label("Приложения не найдены. Измените поиск.");
        }
        if !comparing {
            ui.add_space(14.0);
            if let Some(id) = self.selected {
                if let Some(app) = data.applications.iter().find(|a| a.id == id) {
                    ui.heading(&app.name);
                    ui.label(
                        egui::RichText::new(format!(
                            "Наблюдалось {}; последнее измерение {}",
                            duration(app.observed_secs),
                            time(app.last_seen, "%d.%m %H:%M:%S")
                        ))
                        .small()
                        .color(MUTED),
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(if app.executable.is_empty() {
                                "Путь недоступен; идентификация по имени"
                            } else {
                                &app.executable
                            })
                            .small()
                            .color(MUTED),
                        );
                        if !app.executable.is_empty()
                            && ui.small_button("Копировать путь").clicked()
                        {
                            ui.ctx().copy_text(app.executable.clone());
                        }
                    });
                    if let Some(series) = data.series.iter().find(|s| s.application.id == id) {
                        let line = [ChartLine {
                            name: &app.name,
                            points: &series.points,
                            color: ACCENT,
                        }];
                        plot(
                            ui,
                            "CPU",
                            &line,
                            Metric::Cpu,
                            &self.query,
                            &mut self.pending_zoom,
                            125.0,
                        );
                        plot(
                            ui,
                            "Оперативная память",
                            &[ChartLine {
                                name: &app.name,
                                points: &series.points,
                                color: RAM,
                            }],
                            Metric::Ram,
                            &self.query,
                            &mut self.pending_zoom,
                            125.0,
                        );
                        ui.columns(2, |cols| {
                            plot(
                                &mut cols[0],
                                "GPU",
                                &[ChartLine {
                                    name: &app.name,
                                    points: &series.points,
                                    color: GPU,
                                }],
                                Metric::Gpu,
                                &self.query,
                                &mut self.pending_zoom,
                                105.0,
                            );
                            plot(
                                &mut cols[1],
                                "Получение",
                                &[ChartLine {
                                    name: &app.name,
                                    points: &series.points,
                                    color: NETWORK,
                                }],
                                Metric::Download,
                                &self.query,
                                &mut self.pending_zoom,
                                105.0,
                            );
                        });
                    } else {
                        ui.label("Загружаем временной ряд приложения…");
                    }
                } else {
                    ui.label("Выбранное приложение отсутствует в этом периоде. Выберите другое.");
                    if ui.small_button("Снять выбор").clicked() {
                        self.selected = None;
                        self.invalidate();
                    }
                }
            } else {
                ui.label(
                    egui::RichText::new("Выберите приложение для графиков CPU, RAM, GPU и сети.")
                        .color(MUTED),
                );
            }
        }
    }
    fn comparison(&mut self, ui: &mut egui::Ui, data: &HistoryData) {
        ui.heading("Сравнение приложений");
        ui.label(
            egui::RichText::new(
                "Выберите до четырёх приложений. Цвет закреплён за позицией в списке.",
            )
            .small()
            .color(MUTED),
        );
        let missing: Vec<_> = self
            .compare
            .iter()
            .copied()
            .filter(|id| !data.applications.iter().any(|app| app.id == *id))
            .collect();
        if !missing.is_empty() {
            self.compare.retain(|id| !missing.contains(id));
            self.invalidate();
            ui.label("Приложение без данных в этом периоде исключено из сравнения.");
        }
        self.applications(ui, data, true);
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            for (metric, label) in [
                (Metric::Cpu, "CPU"),
                (Metric::Ram, "RAM"),
                (Metric::Gpu, "GPU"),
                (Metric::Download, "Получение"),
                (Metric::Upload, "Отправка"),
            ] {
                ui.selectable_value(&mut self.compare_metric, metric, label);
            }
        });
        let lines: Vec<_> = self
            .compare
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                data.series
                    .iter()
                    .find(|s| s.application.id == *id)
                    .map(|s| ChartLine {
                        name: &s.application.name,
                        points: &s.points,
                        color: APP_COLORS[i],
                    })
            })
            .collect();
        ui.horizontal_wrapped(|ui| {
            for line in &lines {
                ui.colored_label(line.color, line.name);
            }
        });
        if self.compare.is_empty() {
            ui.add_space(25.0);
            ui.label("Выберите приложения в таблице выше.");
        } else {
            plot(
                ui,
                "Ресурсы на одной временной шкале",
                &lines,
                self.compare_metric,
                &self.query,
                &mut self.pending_zoom,
                235.0,
            );
        }
    }
}

struct ChartLine<'a> {
    name: &'a str,
    points: &'a [HistoryPoint],
    color: egui::Color32,
}
fn time(timestamp: i64, pattern: &str) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|dt| dt.with_timezone(&Local).format(pattern).to_string())
        .unwrap_or_else(|| "—".into())
}
fn duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds >= 3600 {
        format!("{} ч {:02} м", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{} м {:02} с", seconds / 60, seconds % 60)
    }
}
fn bucket_window(end: i64, seconds: i64) -> (i64, i64) {
    let to = end.div_euclid(BUCKET_SECONDS) * BUCKET_SECONDS;
    (to - seconds, to)
}
fn zoom_window(start: i64, end: i64) -> (i64, i64) {
    let from = start.min(end).div_euclid(BUCKET_SECONDS) * BUCKET_SECONDS;
    let to = start.max(end).div_euclid(BUCKET_SECONDS) * BUCKET_SECONDS
        + i64::from(start.max(end).rem_euclid(BUCKET_SECONDS) != 0) * BUCKET_SECONDS;
    (from, (from + 60).max(to).min(from + 86400))
}
fn coverage(observed: f64, seconds: i64) -> f64 {
    (observed / seconds.max(1) as f64 * 100.0).clamp(0.0, 100.0)
}
fn optional_bytes(bytes: Option<f64>) -> String {
    bytes.map_or_else(|| "—".into(), |n| format_bytes(n.max(0.0) as u64))
}

// A missing metric or a missing 30-second bucket breaks the line. No interpolation
// across application shutdown, recording pause, or disabled metric collection.
fn segments(points: &[HistoryPoint], metric: Metric) -> Vec<Vec<(i64, f64)>> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    let mut previous = None;
    let mut previous_complete = false;
    for point in points {
        let value = metric.value(point).filter(|v| v.is_finite());
        let complete = metric.observed(point) >= BUCKET_SECONDS as f64 - 0.01;
        if (value.is_none()
            || (previous.is_some() && (!previous_complete || !complete))
            || previous
                .is_some_and(|t| point.timestamp - t > BUCKET_SECONDS || point.timestamp <= t))
            && !current.is_empty()
        {
            result.push(std::mem::take(&mut current));
        }
        if let Some(value) = value {
            current.push((point.timestamp, value));
        }
        previous = Some(point.timestamp);
        previous_complete = complete;
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}
fn axis_max(metric: Metric, values: impl Iterator<Item = f64>) -> f64 {
    if matches!(metric, Metric::Cpu | Metric::Gpu) {
        100.0
    } else {
        values
            .filter(|value| value.is_finite())
            .fold(1.0_f64, f64::max)
            * 1.05
    }
}
fn hovered_point(points: &[HistoryPoint], cursor_time: i64) -> Option<&HistoryPoint> {
    let bucket = cursor_time.div_euclid(BUCKET_SECONDS) * BUCKET_SECONDS;
    points.iter().find(|point| point.timestamp == bucket)
}
fn axis_time_format(span: i64) -> &'static str {
    if span < 5 * 60 { "%H:%M:%S" } else { "%H:%M" }
}
fn plot(
    ui: &mut egui::Ui,
    title: &str,
    lines: &[ChartLine<'_>],
    metric: Metric,
    query: &HistoryQuery,
    zoom: &mut Option<(i64, i64)>,
    height: f32,
) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.strong(title);
        if let Some(last) = lines
            .first()
            .and_then(|l| l.points.iter().rev().find_map(|p| metric.value(p)))
        {
            ui.label(egui::RichText::new(metric.format(last)).color(lines[0].color));
        }
    });
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(160.0), height + 39.0),
        egui::Sense::click_and_drag(),
    );
    let area = egui::Rect::from_min_max(
        rect.min + egui::vec2(94.0, 9.0),
        rect.max - egui::vec2(12.0, 30.0),
    );
    let painter = ui.painter_at(rect);
    let graph_painter = painter.with_clip_rect(area);
    painter.rect_filled(area, 4.0, egui::Color32::from_rgb(17, 25, 37));
    let max = axis_max(
        metric,
        lines
            .iter()
            .flat_map(|line| line.points)
            .filter_map(|point| metric.value(point)),
    );
    let span = (query.to - query.from).max(1) as f64;
    let position = |t: i64, v: f64| {
        egui::pos2(
            area.left() + ((t - query.from) as f64 / span) as f32 * area.width(),
            area.bottom() - (v / max).clamp(0.0, 1.0) as f32 * area.height(),
        )
    };
    for fraction in [0.0, 0.5, 1.0] {
        let y = area.bottom() - fraction * area.height();
        painter.line_segment(
            [egui::pos2(area.left(), y), egui::pos2(area.right(), y)],
            egui::Stroke::new(0.5, egui::Color32::from_rgb(47, 61, 79)),
        );
        painter.text(
            egui::pos2(area.left() - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            metric.format(fraction as f64 * max),
            egui::FontId::proportional(14.0),
            MUTED,
        );
    }
    let tick_count = ((area.width() / 100.0).floor() as usize).clamp(1, 4);
    for tick in 0..=tick_count {
        let fraction = tick as f32 / tick_count as f32;
        let x = area.left() + fraction * area.width();
        painter.text(
            egui::pos2(x, area.bottom() + 8.0),
            if fraction == 0.0 {
                egui::Align2::LEFT_TOP
            } else if fraction == 1.0 {
                egui::Align2::RIGHT_TOP
            } else {
                egui::Align2::CENTER_TOP
            },
            time(
                query.from + (span * fraction as f64) as i64,
                axis_time_format(query.to - query.from),
            ),
            egui::FontId::proportional(14.0),
            MUTED,
        );
    }
    let mut has_values = false;
    for line in lines {
        for segment in segments(line.points, metric) {
            has_values = true;
            if segment.len() == 1 {
                graph_painter.circle_filled(position(segment[0].0, segment[0].1), 2.0, line.color);
            } else {
                graph_painter.add(egui::Shape::line(
                    segment.into_iter().map(|(t, v)| position(t, v)).collect(),
                    egui::Stroke::new(2.0, line.color),
                ));
            }
        }
    }
    if !has_values {
        painter.text(
            area.center(),
            egui::Align2::CENTER_CENTER,
            "Нет доступных измерений",
            egui::FontId::proportional(14.0),
            MUTED,
        );
    }
    let timestamp_at = |x: f32| {
        query.from + (((x - area.left()) / area.width()).clamp(0.0, 1.0) as f64 * span) as i64
    };
    let drag_id = response.id.with("time-selection");
    if response.drag_started_by(egui::PointerButton::Primary)
        && let Some(pointer) = ui
            .input(|input| input.pointer.press_origin())
            .filter(|p| area.contains(*p))
    {
        ui.ctx()
            .data_mut(|data| data.insert_temp(drag_id, timestamp_at(pointer.x)));
    }
    let drag_start = ui.ctx().data(|data| data.get_temp::<i64>(drag_id));
    if let Some(start) = drag_start
        && (response.dragged_by(egui::PointerButton::Primary)
            || response.drag_stopped_by(egui::PointerButton::Primary))
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let end = timestamp_at(pointer.x);
        graph_painter.rect_filled(
            egui::Rect::from_x_y_ranges(
                position(start.min(end), 0.0).x..=position(start.max(end), 0.0).x,
                area.y_range(),
            ),
            0.0,
            ACCENT.linear_multiply(0.2),
        );
        if response.drag_stopped_by(egui::PointerButton::Primary)
            && (position(start, 0.0).x - pointer.x).abs() >= 5.0
        {
            let (from, to) = zoom_window(start, end);
            let to = to.min(query.to);
            *zoom = Some((from.min(to - 60).max(query.from), to));
        }
    }
    if response.drag_stopped_by(egui::PointerButton::Primary) {
        ui.ctx().data_mut(|data| data.remove::<i64>(drag_id));
    }
    if let Some(pointer) = response.hover_pos().filter(|p| area.contains(*p)) {
        let timestamp =
            query.from + (((pointer.x - area.left()) / area.width()) as f64 * span) as i64;
        let bucket = timestamp.div_euclid(BUCKET_SECONDS) * BUCKET_SECONDS;
        let crosshair_x = position(bucket, 0.0).x;
        painter.line_segment(
            [
                egui::pos2(crosshair_x, area.top()),
                egui::pos2(crosshair_x, area.bottom()),
            ],
            egui::Stroke::new(0.8, MUTED),
        );
        response.on_hover_ui(|ui| {
            ui.strong(format!(
                "Интервал {} — {}",
                time(bucket, "%d.%m.%Y %H:%M:%S"),
                time(bucket + BUCKET_SECONDS, "%H:%M:%S %Z")
            ));
            for line in lines {
                let point = hovered_point(line.points, timestamp);
                ui.colored_label(
                    line.color,
                    format!(
                        "{}: {}",
                        line.name,
                        point
                            .and_then(|p| metric.value(p))
                            .map_or_else(|| "— нет измерения".into(), |v| metric.format(v))
                    ),
                );
                if let Some(p) = point {
                    ui.label(
                        egui::RichText::new(format!(
                            "Наблюдалось {:.1} с из 30 с",
                            match metric {
                                Metric::Gpu => p.gpu_observed_secs,
                                Metric::Download | Metric::Upload => p.network_observed_secs,
                                _ => p.duration_secs,
                            }
                        ))
                        .small()
                        .color(MUTED),
                    );
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refresh_keeps_displayed_query_and_snapshot_until_matching_result_arrives() {
        let mut history = HistoryUi {
            last_request: Some(Instant::now()),
            ..Default::default()
        };
        history.requested_query = HistoryQuery {
            request_id: 1,
            from: 0,
            to: 900,
            application_ids: vec![],
        };
        history.accept_view(Arc::new(HistoryView {
            request_id: 1,
            status: HistoryStatus {
                ready: true,
                ..Default::default()
            },
            ..Default::default()
        }));
        history.requested_query = HistoryQuery {
            request_id: 2,
            from: 30,
            to: 930,
            application_ids: vec![],
        };
        history.accept_view(Arc::new(HistoryView {
            request_id: 1,
            ..Default::default()
        }));
        assert_eq!(history.query.to, 900);
        assert_eq!(history.displayed.as_ref().unwrap().request_id, 1);
        history.accept_view(Arc::new(HistoryView {
            request_id: 2,
            status: HistoryStatus {
                ready: true,
                ..Default::default()
            },
            ..Default::default()
        }));
        assert_eq!(history.query.to, 930);
        assert_eq!(history.displayed.as_ref().unwrap().request_id, 2);
    }
    #[test]
    fn changed_selection_discards_cached_data_and_rejects_old_results() {
        let initial = QueryIntent {
            seconds: 900,
            end: None,
            mode: HistoryMode::Overview,
            ids: vec![],
        };
        for changed in [
            QueryIntent {
                seconds: 300,
                ..initial.clone()
            },
            QueryIntent {
                end: Some(900),
                ..initial.clone()
            },
            QueryIntent {
                mode: HistoryMode::Applications,
                ..initial.clone()
            },
            QueryIntent {
                ids: vec![7],
                ..initial.clone()
            },
        ] {
            let mut history = HistoryUi {
                intent: Some(initial.clone()),
                last_request: Some(Instant::now()),
                requested_query: HistoryQuery {
                    request_id: 1,
                    ..Default::default()
                },
                displayed: Some(Arc::new(HistoryView::default())),
                ..Default::default()
            };
            history.set_intent(initial.clone());
            assert!(history.displayed.is_some());
            history.set_intent(changed);
            assert!(history.displayed.is_none());
            history.accept_view(Arc::new(HistoryView {
                request_id: 1,
                status: HistoryStatus {
                    ready: true,
                    ..Default::default()
                },
                ..Default::default()
            }));
            assert!(history.displayed.is_none());
        }
    }
    #[test]
    fn successful_empty_query_is_accepted_despite_an_independent_recording_warning() {
        let mut history = HistoryUi {
            last_request: Some(Instant::now()),
            requested_query: HistoryQuery {
                request_id: 3,
                from: 0,
                to: 900,
                ..Default::default()
            },
            ..Default::default()
        };
        history.accept_view(Arc::new(HistoryView {
            request_id: 3,
            status: HistoryStatus {
                ready: true,
                error: Some("recording warning".into()),
                ..Default::default()
            },
            ..Default::default()
        }));
        assert_eq!(history.query.request_id, 3);
        assert!(history.displayed.is_some());
    }
    #[test]
    fn failed_refresh_preserves_displayed_data_without_accepting_failed_query() {
        let displayed = Arc::new(HistoryView {
            request_id: 1,
            ..Default::default()
        });
        let mut history = HistoryUi {
            last_request: Some(Instant::now()),
            query: HistoryQuery {
                request_id: 1,
                ..Default::default()
            },
            requested_query: HistoryQuery {
                request_id: 2,
                ..Default::default()
            },
            displayed: Some(displayed.clone()),
            ..Default::default()
        };
        history.accept_view(Arc::new(HistoryView {
            request_id: 2,
            query_error: Some("query failed".into()),
            status: HistoryStatus {
                ready: true,
                ..Default::default()
            },
            ..Default::default()
        }));
        assert!(Arc::ptr_eq(history.displayed.as_ref().unwrap(), &displayed));
        assert_eq!(history.query.request_id, 1);
    }
    #[test]
    fn graph_drag_selects_time_from_press_origin_and_waits_for_release() {
        let context = egui::Context::default();
        let mut zoom = None;
        let query = HistoryQuery {
            from: 0,
            to: 900,
            ..Default::default()
        };
        let mut frame = |events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = context.run_ui(input, |root_ui| {
                egui::CentralPanel::default().show(root_ui, |ui| {
                    plot(ui, "CPU", &[], Metric::Cpu, &query, &mut zoom, 150.0);
                });
            });
            output.textures_delta.clear();
            zoom
        };
        frame(vec![]);
        let press = egui::pos2(200.0, 90.0);
        let release = egui::pos2(500.0, 90.0);
        frame(vec![
            egui::Event::PointerMoved(press),
            egui::Event::PointerButton {
                pos: press,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        assert!(frame(vec![egui::Event::PointerMoved(release)]).is_none());
        let (from, to) = frame(vec![egui::Event::PointerButton {
            pos: release,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }])
        .expect("drag must zoom on release");
        assert!(
            from < 200,
            "selection starts at the press, rather than the first drag frame"
        );
        assert!(to - from > 250 && to - from < 400);
        assert_eq!(from % BUCKET_SECONDS, 0);
        assert_eq!(to % BUCKET_SECONDS, 0);
    }
    #[test]
    fn short_zoom_axis_labels_include_seconds() {
        assert_eq!(axis_time_format(60), "%H:%M:%S");
        assert_eq!(axis_time_format(299), "%H:%M:%S");
        assert_eq!(axis_time_format(300), "%H:%M");
    }
    #[test]
    fn zoom_is_bucket_aligned_bounded_and_direction_independent() {
        assert_eq!(zoom_window(185, 65), zoom_window(65, 185));
        assert_eq!(zoom_window(65, 185), (60, 210));
        assert_eq!(zoom_window(95, 96), (90, 150));
        let (from, to) = zoom_window(0, 100_000);
        assert_eq!(to - from, 24 * 3600);
    }
    #[test]
    fn percent_axes_remain_zero_to_full_machine_capacity() {
        assert_eq!(axis_max(Metric::Cpu, [23.0, 99.9].into_iter()), 100.0);
        assert_eq!(axis_max(Metric::Gpu, [0.0, 85.0].into_iter()), 100.0);
        assert_eq!(axis_max(Metric::Cpu, std::iter::empty()), 100.0);
        assert_eq!(axis_max(Metric::Ram, [200.0].into_iter()), 210.0);
    }
    #[test]
    fn hover_uses_real_bucket_timestamp_and_never_a_nearest_point_across_a_gap() {
        let points = vec![
            HistoryPoint {
                timestamp: 30,
                cpu_avg: 12.0,
                ..Default::default()
            },
            HistoryPoint {
                timestamp: 90,
                cpu_avg: 20.0,
                ..Default::default()
            },
        ];
        let point = hovered_point(&points, 45).unwrap();
        assert_eq!(point.timestamp, 30);
        assert_eq!(point.cpu_avg, 12.0);
        assert!(hovered_point(&points, 60).is_none());
        assert_eq!(hovered_point(&points, 90).unwrap().timestamp, 90);
    }
    #[test]
    fn query_windows_cover_exact_ranges_with_no_partial_right_bucket() {
        for seconds in [300, 900, 1800, 3600, 21600, 43200, 86400] {
            let (from, to) = bucket_window(1_700_000_023, seconds);
            assert_eq!(to % BUCKET_SECONDS, 0);
            assert!(to <= 1_700_000_023);
            assert!(1_700_000_023 - to < BUCKET_SECONDS);
            assert_eq!(to - from, seconds);
        }
    }
    #[test]
    fn offline_and_unknown_intervals_never_join_chart_segments() {
        let points = vec![
            HistoryPoint {
                timestamp: 0,
                duration_secs: 30.0,
                gpu_observed_secs: 30.0,
                cpu_avg: 1.0,
                gpu_avg: Some(1.0),
                ..Default::default()
            },
            HistoryPoint {
                timestamp: 30,
                duration_secs: 30.0,
                cpu_avg: 2.0,
                ..Default::default()
            },
            HistoryPoint {
                timestamp: 90,
                duration_secs: 30.0,
                gpu_observed_secs: 30.0,
                cpu_avg: 3.0,
                gpu_avg: Some(3.0),
                ..Default::default()
            },
        ];
        assert_eq!(
            segments(&points, Metric::Cpu),
            vec![vec![(0, 1.0), (30, 2.0)], vec![(90, 3.0)]]
        );
        assert_eq!(
            segments(&points, Metric::Gpu),
            vec![vec![(0, 1.0)], vec![(90, 3.0)]]
        );
    }
    #[test]
    fn partially_observed_buckets_are_isolated_instead_of_bridging_unknown_time() {
        let points = vec![
            HistoryPoint {
                timestamp: 0,
                duration_secs: 30.0,
                ..Default::default()
            },
            HistoryPoint {
                timestamp: 30,
                duration_secs: 12.0,
                ..Default::default()
            },
            HistoryPoint {
                timestamp: 60,
                duration_secs: 30.0,
                ..Default::default()
            },
        ];
        assert_eq!(
            segments(&points, Metric::Cpu)
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            vec![1, 1, 1]
        );
    }
    #[test]
    fn nonfinite_values_break_charts_and_coverage_is_bounded() {
        let points = vec![HistoryPoint {
            timestamp: 0,
            cpu_avg: f64::NAN,
            ..Default::default()
        }];
        assert!(segments(&points, Metric::Cpu).is_empty());
        assert_eq!(coverage(60.0, 30), 100.0);
        assert_eq!(coverage(15.0, 30), 50.0);
        assert_eq!(time(0, "%Y"), "1970");
    }
}
