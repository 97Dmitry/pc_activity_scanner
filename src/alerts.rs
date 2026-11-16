//! Pure rules over completed collector snapshots. No OS sampling or system control.
use crate::metrics::MetricKind;
use crate::model::{ProcessIdentity, ProcessRow, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

const MAX_RULES: usize = 16;
const MAX_STATES: usize = 32_768;
const MAX_EVENTS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AlertMetric {
    SystemCpu,
    ProcessCpu,
    Resident,
    ResidentGrowth,
    PrivateCommit,
    Gpu,
    IoRead,
    IoWrite,
}
impl AlertMetric {
    pub const ALL: [Self; 8] = [
        Self::SystemCpu,
        Self::ProcessCpu,
        Self::Resident,
        Self::ResidentGrowth,
        Self::PrivateCommit,
        Self::Gpu,
        Self::IoRead,
        Self::IoWrite,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemCpu => "Общий CPU",
            Self::ProcessCpu => "CPU процесса",
            Self::Resident => "Resident / working set",
            Self::ResidentGrowth => "Рост resident без снижения",
            Self::PrivateCommit => "Private commit",
            Self::Gpu => "GPU процесса (максимум движков)",
            Self::IoRead => "I/O чтение",
            Self::IoWrite => "I/O запись",
        }
    }
    pub fn unit(self) -> &'static str {
        match self {
            Self::SystemCpu | Self::ProcessCpu | Self::Gpu => "%",
            Self::IoRead | Self::IoWrite => "bytes/s",
            _ => "bytes",
        }
    }
    pub fn format(self, value: f64) -> String {
        match self.unit() {
            "%" => format!("{value:.1}%"),
            "bytes/s" => crate::model::format_rate(Some(value)),
            _ => crate::model::format_size(value),
        }
    }
    fn default_threshold(self) -> f64 {
        match self {
            Self::SystemCpu | Self::ProcessCpu | Self::Gpu => 90.0,
            Self::ResidentGrowth => 256.0 * 1024.0 * 1024.0,
            Self::IoRead | Self::IoWrite => 10.0 * 1024.0 * 1024.0,
            _ => 2.0 * 1024.0 * 1024.0 * 1024.0,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertRule {
    pub enabled: bool,
    pub metric: AlertMetric,
    pub threshold: f64,
    pub duration_secs: u64,
    pub cooldown_secs: u64,
}
impl Default for AlertRule {
    fn default() -> Self {
        Self {
            enabled: true,
            metric: AlertMetric::SystemCpu,
            threshold: 90.0,
            duration_secs: 30,
            cooldown_secs: 300,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertConfig {
    pub enabled: bool,
    pub desktop: bool,
    pub rules: Vec<AlertRule>,
}
impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            desktop: false,
            rules: vec![
                AlertRule::default(),
                AlertRule {
                    metric: AlertMetric::Resident,
                    threshold: AlertMetric::Resident.default_threshold(),
                    duration_secs: 60,
                    ..Default::default()
                },
                AlertRule {
                    enabled: false,
                    metric: AlertMetric::ResidentGrowth,
                    threshold: AlertMetric::ResidentGrowth.default_threshold(),
                    duration_secs: 60,
                    ..Default::default()
                },
            ],
        }
    }
}
impl AlertConfig {
    pub fn validated(mut self) -> Self {
        self.rules.truncate(MAX_RULES);
        for rule in &mut self.rules {
            if !rule.threshold.is_finite() || rule.threshold <= 0.0 {
                rule.threshold = rule.metric.default_threshold();
            }
            if rule.metric.unit() == "%" {
                rule.threshold = rule.threshold.min(100.0);
            }
            rule.duration_secs = rule.duration_secs.clamp(1, 3600);
            rule.cooldown_secs = rule.cooldown_secs.clamp(1, 86400);
        }
        self
    }
}
#[derive(Clone, Debug)]
pub struct AlertEvent {
    pub id: u64,
    pub rule_index: usize,
    pub process: Option<ProcessIdentity>,
    pub name: String,
    pub metric: AlertMetric,
    pub value: f64,
    pub threshold: f64,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_secs: f64,
}
impl AlertEvent {
    pub fn title(&self) -> String {
        format!("{}: {}", self.name, self.metric.label())
    }
    pub fn body(&self) -> String {
        let identity = self
            .process
            .map_or_else(|| "Весь компьютер".into(), |p| format!("PID {}", p.pid));
        let timestamp = |value| {
            chrono::DateTime::from_timestamp(value, 0)
                .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_else(|| value.to_string())
        };
        let value = if self.metric == AlertMetric::ResidentGrowth {
            "измеренный прирост"
        } else {
            "последнее измерение"
        };
        format!(
            "{}; {}. {}: {}; порог {}. Непрерывно {:.2} с; {} — {}.",
            self.metric.label(),
            identity,
            value,
            self.metric.format(self.value),
            self.metric.format(self.threshold),
            self.duration_secs,
            timestamp(self.started_at),
            timestamp(self.ended_at)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct StateKey {
    rule: usize,
    process: Option<ProcessIdentity>,
}
#[derive(Default)]
struct RuleState {
    previous: Option<f64>,
    anchor: Option<f64>,
    started_at: i64,
    seconds: f64,
    last_event: Option<Instant>,
}
impl RuleState {
    fn baseline(&mut self, value: Option<f64>, at: i64) {
        self.previous = value;
        self.anchor = value;
        self.started_at = at;
        self.seconds = 0.0;
    }
    fn observe(
        &mut self,
        value: Option<f64>,
        sample: &Snapshot,
        rule: &AlertRule,
        contiguous: bool,
        now: Instant,
    ) -> Option<(f64, i64, f64)> {
        let Some(current) = value else {
            self.baseline(None, sample.sampled_at);
            return None;
        };
        let Some(previous) = self.previous.filter(|_| contiguous) else {
            self.baseline(Some(current), sample.sampled_at);
            return None;
        };
        let measured = if rule.metric == AlertMetric::ResidentGrowth {
            if current < previous {
                self.baseline(Some(current), sample.sampled_at);
                return None;
            }
            self.seconds += sample.sample_seconds;
            current - self.anchor.unwrap_or(previous)
        } else {
            if previous < rule.threshold || current < rule.threshold {
                self.baseline(Some(current), sample.sampled_at);
                return None;
            }
            self.seconds += sample.sample_seconds;
            current
        };
        self.previous = Some(current);
        let cooled = self.last_event.is_none_or(|last| {
            now.checked_duration_since(last)
                .is_some_and(|duration| duration.as_secs_f64() >= rule.cooldown_secs as f64)
        });
        if measured >= rule.threshold && self.seconds >= rule.duration_secs as f64 && cooled {
            let event = (measured, self.started_at, self.seconds);
            self.last_event = Some(now);
            self.baseline(Some(current), sample.sampled_at);
            Some(event)
        } else {
            None
        }
    }
}
#[derive(Default)]
pub struct AlertEngine {
    states: HashMap<StateKey, RuleState>,
    journal: VecDeque<AlertEvent>,
    config: Option<AlertConfig>,
    last_sample: Option<(Instant, i64)>,
    next_id: u64,
    status: String,
}
impl AlertEngine {
    /// A control transition can happen and be reversed between collector ticks.
    /// Reset observation explicitly while keeping the bounded event journal.
    pub fn reset_observation(&mut self) {
        self.states.clear();
        self.last_sample = None;
        self.config = None;
    }

    pub fn evaluate(
        &mut self,
        sample: &Snapshot,
        config: &AlertConfig,
        expected_interval_secs: f64,
    ) -> Vec<AlertEvent> {
        let config = config.clone().validated();
        if self.config.as_ref() != Some(&config) {
            let previous = self.config.as_ref();
            self.states.retain(|key, state| {
                state.baseline(None, sample.sampled_at);
                config.rules.get(key.rule).is_some_and(|rule| {
                    rule.enabled
                        && previous
                            .and_then(|old| old.rules.get(key.rule))
                            .is_some_and(|old| old.metric == rule.metric)
                })
            });
            self.last_sample = None;
            self.config = Some(config.clone());
        }
        if !config.enabled {
            self.states.clear();
            self.last_sample = None;
            self.status = "Уведомления выключены".into();
            return Vec::new();
        }
        let Some(now) = sample.updated_at else {
            for state in self.states.values_mut() {
                state.baseline(None, sample.sampled_at);
            }
            self.last_sample = None;
            self.status = "Нет снимка измерений; ожидание сброшено".into();
            return Vec::new();
        };
        if self.last_sample.is_some_and(|(last, _)| last == now) {
            return Vec::new();
        }
        let contiguous = self.last_sample.is_some_and(|(last, utc)| {
            let seconds = sample.sample_seconds;
            let elapsed = now.checked_duration_since(last).map(|d| d.as_secs_f64());
            expected_interval_secs.is_finite()
                && expected_interval_secs > 0.0
                && seconds.is_finite()
                && seconds > 0.0
                && seconds <= 15.0
                && seconds <= expected_interval_secs * 1.5 + 0.25
                && elapsed.is_some_and(|actual| (actual - seconds).abs() <= 0.25)
                && sample.sampled_at >= utc
                && ((sample.sampled_at - utc) as f64 - seconds).abs() <= 1.25
        });
        self.last_sample = Some((now, sample.sampled_at));
        // A duplicated PID is ambiguous even when the two rows claim different
        // start tokens. A grouped/presentation row is never a process sample.
        let mut by_pid: HashMap<u32, Option<&ProcessRow>> = HashMap::new();
        let mut unavailable = 0_usize;
        let mut overflow = false;
        for row in sample
            .process_rows
            .iter()
            .filter(|row| row.pid != 0 && !row.is_group && row.count <= 1)
        {
            if let Some(existing) = by_pid.get_mut(&row.pid) {
                *existing = None;
                unavailable += 1;
            } else if by_pid.len() < MAX_STATES {
                if row.lifetime.is_none() {
                    unavailable += 1;
                }
                by_pid.insert(row.pid, row.lifetime.map(|_| row));
            } else {
                overflow = true;
            }
        }
        let mut rows: Vec<_> = by_pid.into_values().flatten().collect();
        rows.sort_unstable_by_key(|row| row.pid);
        let alive: std::collections::HashSet<_> = rows.iter().map(|row| row.identity()).collect();
        self.states.retain(|key, _| {
            config.rules.get(key.rule).is_some_and(|rule| rule.enabled)
                && key.process.is_none_or(|identity| alive.contains(&identity))
        });
        let mut emitted = VecDeque::new();
        let mut suppressed_events = 0_usize;
        for (index, rule) in config
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.enabled)
        {
            if rule.metric == AlertMetric::SystemCpu {
                let value = known(rule.metric, Some(sample.cpu_pct as f64));
                unavailable += usize::from(value.is_none());
                let candidate = Candidate {
                    key: StateKey {
                        rule: index,
                        process: None,
                    },
                    name: "Весь компьютер",
                    value,
                };
                let input = Evaluation {
                    sample,
                    rule,
                    contiguous,
                    now,
                };
                if let Some(event) = self.consider(candidate, input, &mut overflow) {
                    if emitted.len() == MAX_EVENTS {
                        suppressed_events += 1;
                    }
                    append_event(&mut emitted, event);
                }
            } else {
                for row in &rows {
                    let value = process_value(row, rule.metric);
                    unavailable += usize::from(value.is_none());
                    let candidate = Candidate {
                        key: StateKey {
                            rule: index,
                            process: Some(row.identity()),
                        },
                        name: &row.name,
                        value,
                    };
                    let input = Evaluation {
                        sample,
                        rule,
                        contiguous,
                        now,
                    };
                    if let Some(event) = self.consider(candidate, input, &mut overflow) {
                        if emitted.len() == MAX_EVENTS {
                            suppressed_events += 1;
                        }
                        append_event(&mut emitted, event);
                    }
                }
            }
        }
        self.status = format!(
            "Правил: {}; состояний: {}; событий в журнале: {}. {}{}{}",
            config.rules.iter().filter(|r| r.enabled).count(),
            self.states.len(),
            self.journal.len(),
            if contiguous {
                "Измерение непрерывно."
            } else {
                "Новая начальная точка: старт, пауза или разрыв измерений."
            },
            if unavailable > 0 {
                format!(" Недоступных показателей/идентичностей: {unavailable}.")
            } else {
                String::new()
            },
            if overflow || suppressed_events > 0 {
                format!(
                    " Достигнут лимит состояний/событий; часть данных пропущена ({suppressed_events} событий)."
                )
            } else {
                String::new()
            }
        );
        emitted.into_iter().collect()
    }
    fn consider(
        &mut self,
        candidate: Candidate<'_>,
        input: Evaluation<'_>,
        overflow: &mut bool,
    ) -> Option<AlertEvent> {
        let key = candidate.key;
        if !self.states.contains_key(&key) {
            candidate.value?;
            if self.states.len() >= MAX_STATES {
                *overflow = true;
                return None;
            }
            self.states.insert(key, RuleState::default());
        }
        let state = self.states.get_mut(&key)?;
        let (value, started_at, duration_secs) = state.observe(
            candidate.value,
            input.sample,
            input.rule,
            input.contiguous,
            input.now,
        )?;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let event = AlertEvent {
            id: self.next_id,
            rule_index: key.rule,
            process: key.process,
            name: candidate.name.chars().take(256).collect(),
            metric: input.rule.metric,
            value,
            threshold: input.rule.threshold,
            started_at,
            ended_at: input.sample.sampled_at,
            duration_secs,
        };
        append_event(&mut self.journal, event.clone());
        Some(event)
    }
    pub fn events(&self) -> Vec<AlertEvent> {
        self.journal.iter().cloned().collect()
    }
    pub fn status(&self) -> String {
        if self.status.is_empty() {
            "Ожидаем первое измерение правил".into()
        } else {
            self.status.clone()
        }
    }
}
struct Candidate<'a> {
    key: StateKey,
    name: &'a str,
    value: Option<f64>,
}
struct Evaluation<'a> {
    sample: &'a Snapshot,
    rule: &'a AlertRule,
    contiguous: bool,
    now: Instant,
}
fn append_event(events: &mut VecDeque<AlertEvent>, event: AlertEvent) {
    if events.len() == MAX_EVENTS {
        events.pop_front();
    }
    events.push_back(event);
}
fn known(metric: AlertMetric, value: Option<f64>) -> Option<f64> {
    value.filter(|value| {
        value.is_finite() && *value >= 0.0 && (metric.unit() != "%" || *value <= 100.0)
    })
}
fn process_value(row: &ProcessRow, metric: AlertMetric) -> Option<f64> {
    let value = match metric {
        AlertMetric::SystemCpu => None,
        AlertMetric::ProcessCpu => row.cpu_pct.map(|v| v as f64),
        AlertMetric::Resident | AlertMetric::ResidentGrowth => row.ram_bytes.map(|v| v as f64),
        AlertMetric::PrivateCommit => row.metrics.get(&MetricKind::PrivateCommit).copied(),
        AlertMetric::Gpu => row.gpu_pct.map(|v| v as f64),
        AlertMetric::IoRead => row.metrics.get(&MetricKind::IoRead).copied(),
        AlertMetric::IoWrite => row.metrics.get(&MetricKind::IoWrite).copied(),
    };
    known(metric, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn config(metric: AlertMetric, threshold: f64, duration: u64, cooldown: u64) -> AlertConfig {
        AlertConfig {
            enabled: true,
            desktop: false,
            rules: vec![AlertRule {
                enabled: true,
                metric,
                threshold,
                duration_secs: duration,
                cooldown_secs: cooldown,
            }],
        }
    }
    fn sample(
        origin: Instant,
        tick: f64,
        seconds: f64,
        value: Option<f64>,
        lifetime: u64,
    ) -> Snapshot {
        Snapshot {
            updated_at: Some(origin + Duration::from_secs_f64(tick)),
            sampled_at: tick.floor() as i64 + 1_800_000_000,
            sample_seconds: seconds,
            cpu_pct: value.unwrap_or(f64::NAN) as f32,
            process_rows: vec![ProcessRow {
                pid: 42,
                started_at: 1,
                lifetime: Some(lifetime),
                count: 1,
                name: "worker".into(),
                cpu_pct: value.map(|v| v as f32),
                ram_bytes: value.map(|v| v as u64),
                metrics: value
                    .map(|v| {
                        [
                            (MetricKind::IoRead, v),
                            (MetricKind::IoWrite, v),
                            (MetricKind::PrivateCommit, v),
                        ]
                        .into()
                    })
                    .unwrap_or_default(),
                gpu_pct: value.map(|v| v as f32),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn sustained_cpu_requires_both_endpoints_and_ignores_duplicate_snapshots() {
        let origin = Instant::now();
        let config = config(AlertMetric::ProcessCpu, 50.0, 2, 10);
        let mut engine = AlertEngine::default();
        assert!(
            engine
                .evaluate(&sample(origin, 0.0, 0.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
        let one = sample(origin, 1.0, 1.0, Some(90.0), 1);
        assert!(engine.evaluate(&one, &config, 1.0).is_empty());
        assert!(engine.evaluate(&one, &config, 1.0).is_empty());
        let events = engine.evaluate(&sample(origin, 2.0, 1.0, Some(90.0), 1), &config, 1.0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].duration_secs, 2.0);
        assert_eq!(events[0].process.unwrap().lifetime, Some(1));
    }
    #[test]
    fn pause_gap_missing_and_pid_reuse_cannot_credit_unobserved_time() {
        let origin = Instant::now();
        let config = config(AlertMetric::ProcessCpu, 50.0, 2, 10);
        let mut engine = AlertEngine::default();
        for (tick, seconds, value, lifetime) in [
            (0.0, 0.0, Some(90.0), 1),
            (1.0, 1.0, Some(90.0), 1),
            (2.0, 0.0, Some(90.0), 1),
            (3.0, 1.0, None, 1),
            (4.0, 1.0, Some(90.0), 1),
            (5.0, 1.0, Some(90.0), 2),
            (20.0, 15.0, Some(90.0), 2),
            (21.0, 1.0, Some(90.0), 2),
        ] {
            assert!(
                engine
                    .evaluate(
                        &sample(origin, tick, seconds, value, lifetime),
                        &config,
                        1.0
                    )
                    .is_empty(),
                "unexpected at {tick}"
            );
        }
        assert_eq!(
            engine
                .evaluate(&sample(origin, 22.0, 1.0, Some(90.0), 2), &config, 1.0)
                .len(),
            1
        );
    }
    #[test]
    fn cooldown_survives_unknown_data_and_uses_monotonic_not_utc_time() {
        let origin = Instant::now();
        let config = config(AlertMetric::SystemCpu, 50.0, 1, 5);
        let mut engine = AlertEngine::default();
        engine.evaluate(&sample(origin, 0.0, 0.0, Some(90.0), 1), &config, 1.0);
        assert_eq!(
            engine
                .evaluate(&sample(origin, 1.0, 1.0, Some(90.0), 1), &config, 1.0)
                .len(),
            1
        );
        engine.evaluate(&sample(origin, 2.0, 1.0, None, 1), &config, 1.0);
        for tick in 3..=5 {
            assert!(
                engine
                    .evaluate(
                        &sample(origin, tick as f64, 1.0, Some(90.0), 1),
                        &config,
                        1.0
                    )
                    .is_empty()
            );
        }
        assert_eq!(
            engine
                .evaluate(&sample(origin, 6.0, 1.0, Some(90.0), 1), &config, 1.0)
                .len(),
            1
        );
    }
    #[test]
    fn resident_growth_resets_on_decrease_and_reanchors_after_event() {
        let origin = Instant::now();
        let config = config(AlertMetric::ResidentGrowth, 10.0, 2, 1);
        let mut engine = AlertEngine::default();
        for (tick, value) in [(0.0, 100.0), (1.0, 110.0), (2.0, 105.0), (3.0, 110.0)] {
            assert!(
                engine
                    .evaluate(
                        &sample(
                            origin,
                            tick,
                            if tick == 0.0 { 0.0 } else { 1.0 },
                            Some(value),
                            1
                        ),
                        &config,
                        1.0
                    )
                    .is_empty()
            );
        }
        let events = engine.evaluate(&sample(origin, 4.0, 1.0, Some(115.0), 1), &config, 1.0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].value, 10.0);
        for tick in 5..10 {
            assert!(
                engine
                    .evaluate(
                        &sample(origin, tick as f64, 1.0, Some(115.0), 1),
                        &config,
                        1.0
                    )
                    .is_empty()
            );
        }
    }
    #[test]
    fn config_validation_and_default_opt_in_are_bounded() {
        let defaults = AlertConfig::default();
        assert!(!defaults.enabled && !defaults.desktop);
        assert_eq!(defaults.rules.len(), 3);
        let mut config = config(AlertMetric::ProcessCpu, f64::NAN, 0, u64::MAX);
        config.rules.resize(100, config.rules[0].clone());
        let config = config.validated();
        assert_eq!(config.rules.len(), MAX_RULES);
        assert_eq!(config.rules[0].threshold, 90.0);
        assert_eq!(config.rules[0].duration_secs, 1);
        assert_eq!(config.rules[0].cooldown_secs, 86400);
    }
    #[test]
    fn first_high_reading_does_not_credit_a_low_previous_endpoint() {
        let origin = Instant::now();
        let config = config(AlertMetric::SystemCpu, 50.0, 1, 5);
        let mut engine = AlertEngine::default();
        engine.evaluate(&sample(origin, 0.0, 0.0, Some(10.0), 1), &config, 1.0);
        assert!(
            engine
                .evaluate(&sample(origin, 1.0, 1.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
        assert_eq!(
            engine
                .evaluate(&sample(origin, 2.0, 1.0, Some(90.0), 1), &config, 1.0)
                .len(),
            1
        );
    }
    #[test]
    fn fractional_samples_in_one_utc_second_are_valid_but_rollback_breaks_pending() {
        let origin = Instant::now();
        let config = config(AlertMetric::SystemCpu, 50.0, 1, 1);
        let mut engine = AlertEngine::default();
        engine.evaluate(&sample(origin, 0.0, 0.0, Some(90.0), 1), &config, 0.5);
        assert!(
            engine
                .evaluate(&sample(origin, 0.5, 0.5, Some(90.0), 1), &config, 0.5)
                .is_empty()
        );
        assert_eq!(
            engine
                .evaluate(&sample(origin, 1.0, 0.5, Some(90.0), 1), &config, 0.5)
                .len(),
            1
        );
        let mut rollback = sample(origin, 1.5, 0.5, Some(90.0), 1);
        rollback.sampled_at -= 100;
        assert!(engine.evaluate(&rollback, &config, 0.5).is_empty());
        let mut next = sample(origin, 2.0, 0.5, Some(90.0), 1);
        next.sampled_at -= 100;
        assert!(engine.evaluate(&next, &config, 0.5).is_empty());
        next = sample(origin, 2.5, 0.5, Some(90.0), 1);
        next.sampled_at -= 100;
        assert_eq!(engine.evaluate(&next, &config, 0.5).len(), 1);
    }
    #[test]
    fn changing_rules_or_global_disable_resets_pending_but_keeps_journal() {
        let origin = Instant::now();
        let mut config = config(AlertMetric::SystemCpu, 50.0, 2, 1);
        let mut engine = AlertEngine::default();
        engine.evaluate(&sample(origin, 0.0, 0.0, Some(90.0), 1), &config, 1.0);
        engine.evaluate(&sample(origin, 1.0, 1.0, Some(90.0), 1), &config, 1.0);
        config.rules[0].threshold = 60.0;
        assert!(
            engine
                .evaluate(&sample(origin, 2.0, 1.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
        assert!(
            engine
                .evaluate(&sample(origin, 3.0, 1.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
        assert_eq!(
            engine
                .evaluate(&sample(origin, 4.0, 1.0, Some(90.0), 1), &config, 1.0)
                .len(),
            1
        );
        config.enabled = false;
        assert!(
            engine
                .evaluate(&sample(origin, 5.0, 1.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
        assert_eq!(engine.events().len(), 1);
        config.enabled = true;
        assert!(
            engine
                .evaluate(&sample(origin, 6.0, 1.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
        assert!(
            engine
                .evaluate(&sample(origin, 7.0, 1.0, Some(90.0), 1), &config, 1.0)
                .is_empty()
        );
    }
    #[test]
    fn only_raw_exact_process_lifetimes_are_eligible_and_metrics_are_independent() {
        let origin = Instant::now();
        let config = config(AlertMetric::PrivateCommit, 4096.0, 1, 1);
        let mut engine = AlertEngine::default();
        let mut s = sample(origin, 0.0, 0.0, None, 1);
        s.process_rows[0]
            .metrics
            .insert(MetricKind::PrivateCommit, 8192.0);
        engine.evaluate(&s, &config, 1.0);
        s.updated_at = Some(origin + Duration::from_secs(1));
        s.sampled_at += 1;
        s.sample_seconds = 1.0;
        assert_eq!(engine.evaluate(&s, &config, 1.0).len(), 1);
        for mode in 0..3 {
            let mut engine = AlertEngine::default();
            let mut s = sample(origin, 0.0, 0.0, Some(90.0), 1);
            if mode == 0 {
                s.process_rows[0].is_group = true;
                s.process_rows[0].count = 3;
            }
            if mode == 1 {
                s.process_rows[0].lifetime = None;
            }
            if mode == 2 {
                s.process_rows.push(s.process_rows[0].clone());
            }
            let config = config_for_cpu();
            engine.evaluate(&s, &config, 1.0);
            s.updated_at = Some(origin + Duration::from_secs(1));
            s.sampled_at += 1;
            s.sample_seconds = 1.0;
            assert!(engine.evaluate(&s, &config, 1.0).is_empty());
        }
    }
    fn config_for_cpu() -> AlertConfig {
        config(AlertMetric::ProcessCpu, 50.0, 1, 1)
    }
    #[test]
    fn native_units_and_event_text_preserve_measured_fact_and_identity() {
        assert_eq!(AlertMetric::IoWrite.unit(), "bytes/s");
        assert_eq!(AlertMetric::ResidentGrowth.unit(), "bytes");
        let origin = Instant::now();
        let config = config(AlertMetric::IoRead, 4096.0, 1, 1);
        let mut engine = AlertEngine::default();
        engine.evaluate(&sample(origin, 0.0, 0.0, Some(8192.0), 1), &config, 1.0);
        let events = engine.evaluate(&sample(origin, 1.0, 1.0, Some(8192.0), 1), &config, 1.0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].value, 8192.0);
        let text = events[0].body();
        assert!(text.contains("PID 42"));
        assert!(
            text.contains(
                &chrono::DateTime::from_timestamp(1_800_000_000, 0)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )
        );
        assert!(!text.contains("native token"));
        assert!(!text.contains("причина") && !text.contains("утечк"));
    }

    #[test]
    fn state_and_event_storage_are_bounded_and_overflow_is_visible() {
        let origin = Instant::now();
        let config = config_for_cpu();
        let mut engine = AlertEngine::default();
        let mut s = sample(origin, 0.0, 0.0, Some(90.0), 1);
        let row = s.process_rows[0].clone();
        s.process_rows = (1..=MAX_STATES as u32 + 3)
            .map(|pid| ProcessRow { pid, ..row.clone() })
            .collect();
        assert!(engine.evaluate(&s, &config, 1.0).is_empty());
        s.updated_at = Some(origin + Duration::from_secs(1));
        s.sampled_at += 1;
        s.sample_seconds = 1.0;
        let events = engine.evaluate(&s, &config, 1.0);
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(engine.events().len(), MAX_EVENTS);
        assert!(engine.status().contains("лимит"));
        assert!(
            engine
                .events()
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
    }
    #[test]
    fn exactly_full_event_batch_does_not_report_lost_events() {
        let origin = Instant::now();
        let mut config = config_for_cpu();
        config.rules.push(AlertRule {
            metric: AlertMetric::SystemCpu,
            threshold: 50.0,
            duration_secs: 1,
            cooldown_secs: 1,
            ..Default::default()
        });
        let mut s = sample(origin, 0.0, 0.0, Some(90.0), 1);
        let row = s.process_rows[0].clone();
        s.process_rows = (1..100)
            .map(|pid| ProcessRow { pid, ..row.clone() })
            .collect();
        let mut engine = AlertEngine::default();
        engine.evaluate(&s, &config, 1.0);
        s.updated_at = Some(origin + Duration::from_secs(1));
        s.sampled_at += 1;
        s.sample_seconds = 1.0;
        assert_eq!(engine.evaluate(&s, &config, 1.0).len(), 100);
        assert!(!engine.status().contains("пропущена"));
    }
    #[test]
    fn native_read_only_resident_measurement_triggers_rule_without_load_experiment() {
        let pid = std::process::id();
        let mut rows = vec![ProcessRow {
            pid,
            count: 1,
            ..Default::default()
        }];
        let mut source = crate::process_details::ProcessDetails::default();
        source.sample(&mut rows, 1, 0.0);
        rows.retain(|row| row.pid == pid);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ram_bytes.is_some_and(|bytes| bytes > 0));
        assert!(rows[0].lifetime.is_some());
        let first_at = Instant::now();
        let config = config(AlertMetric::Resident, 1.0, 1, 5);
        let mut engine = AlertEngine::default();
        let mut s = Snapshot {
            updated_at: Some(first_at),
            sampled_at: chrono::Utc::now().timestamp(),
            process_rows: rows,
            sample_seconds: 0.0,
            ..Default::default()
        };
        assert!(engine.evaluate(&s, &config, 2.0).is_empty());
        std::thread::sleep(Duration::from_millis(1100));
        source.sample(&mut s.process_rows, 1, 1.1);
        s.process_rows.retain(|row| row.pid == pid);
        let second_at = Instant::now();
        s.updated_at = Some(second_at);
        s.sampled_at = chrono::Utc::now().timestamp();
        s.sample_seconds = second_at.duration_since(first_at).as_secs_f64();
        let events = engine.evaluate(&s, &config, 2.0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].process.unwrap(), s.process_rows[0].identity());
        assert_eq!(events[0].value, s.process_rows[0].ram_bytes.unwrap() as f64);
        eprintln!(
            "Native alert resident PID {pid}: {} bytes for {:.3}s, token {:?}",
            events[0].value,
            events[0].duration_secs,
            events[0].process.unwrap().lifetime
        );
    }
}
