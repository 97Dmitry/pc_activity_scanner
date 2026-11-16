#[cfg(target_os = "linux")]
use crate::gpu::drm::DrmSampler;
use crate::model::{ProcessRow, Snapshot};
use crate::network::{NetworkFrame, NetworkSample};
#[cfg(target_os = "linux")]
use crate::nvidia::{NvidiaState, start_nvidia_sampler};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Networks, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

struct SampleTiming {
    sampled_at: i64,
    sample_seconds: f64,
    network_seconds: f64,
}

struct SamplingClock {
    last_network_sample: Instant,
    last_history_sample: Option<(Instant, u64)>,
}

impl SamplingClock {
    fn new(now: Instant) -> Self {
        Self {
            last_network_sample: now,
            last_history_sample: None,
        }
    }

    fn capture(&mut self, now: Instant, wall: SystemTime, pause_state: u64) -> SampleTiming {
        let elapsed = now.duration_since(self.last_network_sample).as_secs_f64();
        self.last_network_sample = now;
        let sample_seconds = self.last_history_sample.map_or(0.0, |(last, state)| {
            if state == pause_state && pause_state & 1 == 0 {
                now.duration_since(last).as_secs_f64()
            } else {
                0.0
            }
        });
        self.last_history_sample = (pause_state & 1 == 0).then_some((now, pause_state));
        SampleTiming {
            sampled_at: wall
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            sample_seconds,
            network_seconds: elapsed.max(0.001),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct ProcessTrafficTotal {
    uploaded_bytes: u64,
    downloaded_bytes: u64,
}

#[derive(Clone, Copy)]
struct FrameCapture {
    epoch: u64,
    sequence: u64,
    captured_at: u64,
    seconds: f64,
}
impl FrameCapture {
    fn process_predates_interval(self, started: u64) -> bool {
        self.seconds.is_finite()
            && self.seconds > 0.0
            && self.seconds <= 15.0
            && started
                .checked_add(self.seconds.ceil() as u64)
                .and_then(|value| value.checked_add(1))
                .is_some_and(|earliest| earliest <= self.captured_at)
    }
}

/// Exact native identities isolate totals even if a PID is reused within one
/// UTC second. Linux NetHogs additionally needs a conservative full-frame guard.
#[derive(Default)]
struct TrafficLedger {
    latest: Option<FrameCapture>,
    strict_wire: bool,
    totals: HashMap<(u32, u64, u64), ProcessTrafficTotal>,
}

impl TrafficLedger {
    fn new(strict_wire: bool) -> Self {
        Self {
            strict_wire,
            ..Self::default()
        }
    }

    fn record_frame(
        &mut self,
        net: &NetworkFrame,
        mut start_time: impl FnMut(u32) -> Option<(u64, Option<u64>)>,
    ) {
        if self
            .latest
            .is_some_and(|last| last.epoch == net.epoch && net.sequence <= last.sequence)
        {
            return;
        }
        let capture = FrameCapture {
            epoch: net.epoch,
            sequence: net.sequence,
            captured_at: net.captured_at,
            seconds: net.seconds_per_sample,
        };
        self.latest = Some(capture);
        for (pid, rate) in &net.rates {
            let Some((started, Some(created))) = start_time(*pid) else {
                continue;
            };
            if started > net.captured_at {
                continue;
            }
            match net.process_lifetimes.get(pid).copied() {
                Some(wire_created) if wire_created == created => {}
                Some(_) => continue,
                None if self.strict_wire || !capture.process_predates_interval(started) => continue,
                None => {}
            }
            let (up, down) = if let Some(exact) = net.exact_bytes.get(pid).copied() {
                exact
            } else {
                if !rate.upload_bps.is_finite()
                    || !rate.download_bps.is_finite()
                    || rate.upload_bps < 0.0
                    || rate.download_bps < 0.0
                    || !net.seconds_per_sample.is_finite()
                    || net.seconds_per_sample <= 0.0
                {
                    continue;
                }
                (
                    (rate.upload_bps * net.seconds_per_sample) as u64,
                    (rate.download_bps * net.seconds_per_sample) as u64,
                )
            };
            let total = self.totals.entry((*pid, started, created)).or_default();
            total.uploaded_bytes = total.uploaded_bytes.saturating_add(up);
            total.downloaded_bytes = total.downloaded_bytes.saturating_add(down);
        }
    }

    fn retain_running(&mut self, mut alive: impl FnMut(u32, u64, u64) -> bool) {
        self.totals
            .retain(|(pid, started, created), _| alive(*pid, *started, *created));
    }

    fn get(&self, pid: u32, started: u64, created: u64) -> Option<ProcessTrafficTotal> {
        self.totals.get(&(pid, started, created)).copied()
    }

    #[cfg(any(target_os = "linux", test))]
    fn pid_estimate_available(
        &self,
        row: &ProcessRow,
        net: &NetworkSample,
        current: Option<u64>,
    ) -> bool {
        row.lifetime.is_some()
            && row.lifetime == current
            && net.has_fresh_data(Duration::from_secs(7))
            && self.latest.is_some_and(|frame| {
                frame.epoch == net.epoch
                    && frame.sequence == net.sequence
                    && frame.process_predates_interval(row.started_at)
            })
    }
}

fn traffic_identity(pid: u32) -> Option<u64> {
    crate::process_details::process_lifetime(pid)
}

#[derive(Clone, Debug, Default)]
pub struct AlertReport {
    pub events: Vec<crate::alerts::AlertEvent>,
    pub status: String,
}

#[derive(Clone)]
struct AlertPreferences {
    revision: u64,
    config: crate::alerts::AlertConfig,
}

struct AlertRuntime {
    config: Arc<RwLock<AlertPreferences>>,
    revision: u64,
    output: Arc<RwLock<Arc<AlertReport>>>,
    desktop: crate::desktop_notifications::DesktopNotifications,
    engine: crate::alerts::AlertEngine,
}
impl AlertRuntime {
    fn record(&mut self, sample: &Snapshot, expected_interval_ms: u64) {
        let preferences = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if preferences.revision != self.revision {
            self.engine.reset_observation();
            self.revision = preferences.revision;
        }
        let config = preferences.config;
        let events = self
            .engine
            .evaluate(sample, &config, expected_interval_ms as f64 / 1000.0);
        if config.enabled && config.desktop {
            for event in events {
                self.desktop.notify(event.title(), event.body());
            }
        }
        *self.output.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(AlertReport {
            events: self.engine.events(),
            status: self.engine.status(),
        });
    }
}

pub struct Collector {
    pub snapshot: Arc<RwLock<Arc<Snapshot>>>,
    pub alerts: Arc<RwLock<Arc<AlertReport>>>,
    alert_config: Arc<RwLock<AlertPreferences>>,
    desktop_notifications: crate::desktop_notifications::DesktopNotifications,
    interval_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    // Low bit is pause status; every transition advances the generation, including
    // a pause/resume pair occurring entirely between collector ticks.
    pause_state: Arc<AtomicU64>,
    exited: Arc<AtomicBool>,
}

impl Collector {
    pub fn start(
        interval: u64,
        traffic: Arc<Mutex<NetworkSample>>,
        recorder: crate::history::HistoryRecorder,
        alert_config: crate::alerts::AlertConfig,
    ) -> Self {
        let snapshot = Arc::new(RwLock::new(Arc::new(Snapshot::default())));
        let interval_ms = Arc::new(AtomicU64::new(interval));
        let stop = Arc::new(AtomicBool::new(false));
        let pause_state = Arc::new(AtomicU64::new(0));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let alert_config = alert_config.validated();
        let desktop_notifications =
            crate::desktop_notifications::DesktopNotifications::start(Arc::clone(&stop));
        desktop_notifications.set_enabled(alert_config.enabled && alert_config.desktop);
        let alert_config = Arc::new(RwLock::new(AlertPreferences {
            revision: 0,
            config: alert_config,
        }));
        let alerts = Arc::new(RwLock::new(Arc::new(AlertReport::default())));
        let alert_runtime = AlertRuntime {
            config: Arc::clone(&alert_config),
            revision: 0,
            output: Arc::clone(&alerts),
            desktop: desktop_notifications.clone(),
            engine: crate::alerts::AlertEngine::default(),
        };
        let shared_snapshot = Arc::clone(&snapshot);
        let shared_stop = Arc::clone(&stop);
        let shared_pause_state = Arc::clone(&pause_state);
        let shared_interval = Arc::clone(&interval_ms);
        thread::spawn(move || {
            run_collector(
                shared_snapshot,
                shared_interval,
                shared_stop,
                shared_pause_state,
                traffic,
                recorder,
                alert_runtime,
            );
            worker_exited.store(true, Ordering::Release);
        });
        Self {
            snapshot,
            alerts,
            alert_config,
            desktop_notifications,
            exited,
            interval_ms,
            stop,
            pause_state,
        }
    }

    pub fn shutdown(&self) {
        self.desktop_notifications.set_enabled(false);
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn is_stopped(&self) -> bool {
        self.exited.load(Ordering::Acquire) && self.desktop_notifications.is_stopped()
    }

    pub fn set_alert_config(&self, config: crate::alerts::AlertConfig) {
        let config = config.validated();
        self.desktop_notifications.set_enabled(
            config.enabled && config.desktop && self.pause_state.load(Ordering::Relaxed) & 1 == 0,
        );
        let mut preferences = self.alert_config.write().unwrap_or_else(|e| e.into_inner());
        if preferences.config != config {
            preferences.revision = preferences.revision.wrapping_add(1);
            preferences.config = config;
        }
    }

    pub fn notification_status(&self) -> crate::desktop_notifications::DeliveryStatus {
        self.desktop_notifications.status()
    }

    pub fn set_paused(&self, value: bool) {
        let config = self.alert_config.read().unwrap_or_else(|e| e.into_inner());
        self.desktop_notifications
            .set_enabled(!value && config.config.enabled && config.config.desktop);
        drop(config);
        let _ = self
            .pause_state
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                ((current & 1 != 0) != value).then(|| current.wrapping_add(1))
            });
    }

    pub fn set_interval(&self, millis: u64) {
        self.interval_ms.store(millis, Ordering::Relaxed);
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_collector(
    output: Arc<RwLock<Arc<Snapshot>>>,
    interval: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    pause_state: Arc<AtomicU64>,
    net_state: Arc<Mutex<NetworkSample>>,
    recorder: crate::history::HistoryRecorder,
    mut alerts: AlertRuntime,
) {
    #[cfg(target_os = "linux")]
    let nvidia_state = {
        let state = Arc::new(RwLock::new(NvidiaState::default()));
        start_nvidia_sampler(Arc::clone(&stop), Arc::clone(&state));
        state
    };
    let thermal = crate::thermal::ThermalSampler::start(Arc::clone(&stop));
    let mut system = System::new();
    let mut disks = crate::disk::DiskSampler::default();
    #[cfg(target_os = "windows")]
    let mut windows_gpu = crate::windows_gpu::WindowsGpuSampler::default();
    let mut details = crate::process_details::ProcessDetails::default();
    // Native detail sources validate availability and lifetimes; sysinfo supplies process discovery.
    let process_refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    system.refresh_cpu_usage();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh);
    thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    let mut networks = Networks::new_with_refreshed_list();
    let mut clock = SamplingClock::new(Instant::now());
    let mut traffic = TrafficLedger::new(cfg!(target_os = "windows"));
    #[cfg(target_os = "linux")]
    let mut drm = DrmSampler::new();
    while !stop.load(Ordering::Relaxed) {
        if pause_state.load(Ordering::Relaxed) & 1 != 0 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let tick = Instant::now();
        system.refresh_cpu_usage();
        system.refresh_memory();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh);
        networks.refresh(true);
        let captured_at = Instant::now();
        let timing = clock.capture(
            captured_at,
            SystemTime::now(),
            pause_state.load(Ordering::Relaxed),
        );
        let (mut net, frames) = {
            let mut state = net_state.lock().unwrap_or_else(|e| e.into_inner());
            let frames = state.take_frames();
            (state.clone(), frames)
        };
        net.expire_if_stale(Duration::from_secs(7));
        #[cfg(target_os = "linux")]
        let pids = system
            .processes()
            .keys()
            .map(|pid| pid.as_u32())
            .collect::<Vec<_>>();

        #[cfg(target_os = "linux")]
        let mut gpu = drm.sample(&pids);
        #[cfg(target_os = "linux")]
        let nvidia = nvidia_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        #[cfg(target_os = "linux")]
        if nvidia
            .updated_at
            .is_some_and(|last| last.elapsed() < Duration::from_secs(8))
        {
            for (pid, value) in nvidia.data {
                gpu.entry(pid).or_default().combine(value);
            }
        }

        let mut processes = Vec::with_capacity(system.processes().len());
        for (pid, process) in system.processes() {
            processes.push(ProcessRow {
                pid: pid.as_u32(),
                started_at: process.start_time(),
                count: 1,
                parent_pid: process.parent().map(|pid| pid.as_u32()),
                name: process.name().to_string_lossy().to_string(),
                executable: process
                    .exe()
                    .map_or_else(String::new, |p| p.display().to_string()),
                ..Default::default()
            });
        }
        let process_status =
            details.sample(&mut processes, system.cpus().len(), timing.sample_seconds);
        // Sample native identity after authoritative detail metadata. Query once
        // per current row, rather than reopening every PID for every queued frame.
        let identities: HashMap<_, _> = processes
            .iter()
            .filter_map(|row| {
                let token = row.lifetime?;
                (traffic_identity(row.pid) == Some(token))
                    .then_some((row.pid, (row.started_at, token)))
            })
            .collect();
        // A frame's epoch/sequence is consumed once, including buffered frames
        // accumulated while GUI collection was paused or slower than capture.
        for frame in &frames {
            traffic.record_frame(frame, |pid| {
                identities
                    .get(&pid)
                    .map(|(started, token)| (*started, Some(*token)))
            });
        }
        traffic
            .retain_running(|pid, started, token| identities.get(&pid) == Some(&(started, token)));
        for row in &mut processes {
            let current = identities.get(&row.pid).map(|(_, token)| *token);
            let network_rate = net.rates.get(&row.pid);
            #[cfg(target_os = "windows")]
            let available = net.has_fresh_data(Duration::from_secs(7))
                && current.is_some()
                && row.lifetime == current
                && net.process_lifetimes.get(&row.pid).copied() == current
                && network_rate.is_some();
            #[cfg(target_os = "linux")]
            let available = traffic.pid_estimate_available(row, &net, current);
            let available = available
                && network_rate.is_none_or(|rate| {
                    rate.download_bps.is_finite()
                        && rate.upload_bps.is_finite()
                        && rate.download_bps >= 0.0
                        && rate.upload_bps >= 0.0
                });
            if available {
                row.down_bps = Some(network_rate.map_or(0.0, |rate| rate.download_bps));
                row.up_bps = Some(network_rate.map_or(0.0, |rate| rate.upload_bps));
                let total = current.and_then(|token| traffic.get(row.pid, row.started_at, token));
                row.downloaded_bytes = Some(total.map_or(0, |value| value.downloaded_bytes));
                row.uploaded_bytes = Some(total.map_or(0, |value| value.uploaded_bytes));
            }
        }
        #[cfg(target_os = "linux")]
        for row in &mut processes {
            if let Some(usage) = gpu
                .get(&row.pid)
                .filter(|usage| usage.lifetime.is_some() && usage.lifetime == row.lifetime)
            {
                row.gpu_pct = usage.utilization_pct;
                row.gpu_memory_bytes = usage.memory_bytes;
            }
        }
        let (devices, disk_status) = disks.sample(&mut processes, timing.sample_seconds);
        let mut rx_bps = 0.0;
        let mut tx_bps = 0.0;
        let mut interface_count = 0;
        for (name, data) in &networks {
            if name == "lo" || name.to_lowercase().contains("loopback") {
                continue;
            }
            rx_bps += data.received() as f64 / timing.network_seconds;
            tx_bps += data.transmitted() as f64 / timing.network_seconds;
            interface_count += 1;
        }
        #[cfg(target_os = "windows")]
        let gpu_status = windows_gpu.sample(&mut processes, timing.sample_seconds);
        #[cfg(not(target_os = "windows"))]
        let gpu_status = if cfg!(target_os = "linux") {
            if !gpu.is_empty() {
                "DRM / NVIDIA (если доступен)".to_string()
            } else if nvidia.status.is_empty() {
                "GPU: драйвер не предоставляет метрики по PID".to_string()
            } else {
                nvidia.status
            }
        } else {
            "GPU по PID для этой ОС ещё не реализован".to_string()
        };
        crate::model::resolve_parents(&mut processes);
        let next = Snapshot {
            sampled_at: timing.sampled_at,
            sample_seconds: timing.sample_seconds,
            process_rows: processes,
            process_status,
            devices,
            disk_status,
            cpu_pct: system.global_cpu_usage().clamp(0.0, 100.0),
            thermal: thermal.snapshot(),
            logical_cpu_count: system.cpus().len(),
            logical_cpus: system
                .cpus()
                .iter()
                .take(4096)
                .map(|cpu| {
                    (timing.sample_seconds > 0.0 && timing.sample_seconds <= 15.0)
                        .then_some(cpu.cpu_usage())
                        .filter(|value| value.is_finite())
                        .map(|value| value.clamp(0.0, 100.0))
                })
                .collect(),
            ram_total: system.total_memory(),
            ram_used: system.used_memory(),
            down_bps: rx_bps,
            up_bps: tx_bps,
            interface_count,
            network_status: net.status(),
            gpu_status,
            updated_at: Some(captured_at),
        };
        alerts.record(&next, interval.load(Ordering::Relaxed));
        let next = Arc::new(next);
        *output.write().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&next);
        recorder.record(next);
        let remaining =
            Duration::from_millis(interval.load(Ordering::Relaxed)).saturating_sub(tick.elapsed());
        let mut left = remaining;
        while !left.is_zero() && !stop.load(Ordering::Relaxed) {
            let step = left.min(Duration::from_millis(100));
            thread::sleep(step);
            left = left.saturating_sub(step);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::NetworkRate;

    #[test]
    fn disabling_then_enabling_rules_between_samples_restarts_duration() {
        use crate::alerts::{AlertConfig, AlertEngine, AlertMetric, AlertRule};
        let config = AlertConfig {
            enabled: true,
            desktop: false,
            rules: vec![AlertRule {
                enabled: true,
                metric: AlertMetric::SystemCpu,
                threshold: 50.0,
                duration_secs: 3,
                cooldown_secs: 1,
            }],
        };
        let shared_config = Arc::new(RwLock::new(AlertPreferences {
            revision: 0,
            config: config.clone(),
        }));
        let output = Arc::new(RwLock::new(Arc::new(AlertReport::default())));
        let stop = Arc::new(AtomicBool::new(true));
        let desktop = crate::desktop_notifications::DesktopNotifications::start(Arc::clone(&stop));
        let mut runtime = AlertRuntime {
            config: Arc::clone(&shared_config),
            revision: 0,
            output: Arc::clone(&output),
            desktop: desktop.clone(),
            engine: AlertEngine::default(),
        };
        let collector = Collector {
            snapshot: Arc::default(),
            alerts: Arc::clone(&output),
            alert_config: shared_config,
            desktop_notifications: desktop,
            interval_ms: Arc::new(AtomicU64::new(1000)),
            stop,
            pause_state: Arc::new(AtomicU64::new(0)),
            exited: Arc::new(AtomicBool::new(true)),
        };
        let origin = Instant::now();
        let sample = |second: u64| Snapshot {
            sampled_at: 1_800_000_000 + second as i64,
            sample_seconds: if second == 0 { 0.0 } else { 1.0 },
            cpu_pct: 90.0,
            updated_at: Some(origin + Duration::from_secs(second)),
            ..Default::default()
        };
        runtime.record(&sample(0), 1000);
        runtime.record(&sample(1), 1000);
        let mut off = config.clone();
        off.enabled = false;
        collector.set_alert_config(off);
        collector.set_alert_config(config);
        runtime.record(&sample(2), 1000);
        runtime.record(&sample(3), 1000);
        assert!(
            output.read().unwrap().events.is_empty(),
            "off/on must discard the old pending interval"
        );
        runtime.record(&sample(4), 1000);
        let unchanged = collector.alert_config.read().unwrap().config.clone();
        collector.set_alert_config(unchanged);
        runtime.record(&sample(5), 1000);
        let report = output.read().unwrap();
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.events[0].started_at, 1_800_000_002);
        assert_eq!(report.events[0].duration_secs, 3.0);
    }

    #[test]
    fn native_collector_delivers_lifetime_io_and_parent_without_gui() {
        let dir = tempfile::tempdir().unwrap();
        let service = crate::history::HistoryService::start_at(
            dir.path().join("collector.sqlite3"),
            crate::history::types::HistoryConfig {
                enabled: false,
                retention_days: 1,
            },
        );
        let collector = Collector::start(
            1000,
            Arc::new(Mutex::new(NetworkSample::default())),
            service.recorder(),
            crate::alerts::AlertConfig::default(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = collector.snapshot.read().unwrap().clone();
            if snapshot.sample_seconds > 0.0 {
                let own = snapshot
                    .process_rows
                    .iter()
                    .find(|r| r.pid == std::process::id())
                    .expect("collector must observe its own process");
                assert!(
                    own.lifetime.is_some(),
                    "own exact lifetime unavailable: {}",
                    snapshot.disk_status
                );
                assert!(
                    own.metrics
                        .contains_key(&crate::metrics::MetricKind::IoRead)
                );
                assert!(
                    own.metrics
                        .contains_key(&crate::metrics::MetricKind::IoWrite)
                );
                assert!(own.parent_pid.is_some());
                if let Some(parent) = snapshot
                    .process_rows
                    .iter()
                    .find(|p| Some(p.pid) == own.parent_pid && p.lifetime.is_some())
                {
                    assert_eq!(own.parent, Some(parent.identity()));
                }
                assert_eq!(
                    snapshot.logical_cpus.len(),
                    snapshot.logical_cpu_count.min(4096)
                );
                let logical_mean = crate::model::logical_cpu_average(&snapshot.logical_cpus)
                    .expect("logical CPU measurement unavailable");
                eprintln!(
                    "Logical CPUs: {}, mean={logical_mean:.3}%, global={:.3}%",
                    snapshot.logical_cpu_count, snapshot.cpu_pct
                );
                eprintln!(
                    "Native collector: processes={}, own={:?}, parent={:?}, devices={}",
                    snapshot.process_rows.len(),
                    own.identity(),
                    own.parent,
                    snapshot.devices.len()
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "collector did not publish two samples"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            collector.alerts.read().unwrap().events.is_empty(),
            "default rules must stay off on real samples"
        );
        collector.shutdown();
        let stopped_deadline = Instant::now() + Duration::from_secs(3);
        while !collector.is_stopped() && Instant::now() < stopped_deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            collector.is_stopped(),
            "collector and desktop delivery must acknowledge shutdown"
        );
    }

    #[test]
    fn timestamps_use_capture_time_but_durations_use_monotonic_time() {
        let start = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut clock = SamplingClock::new(start);
        let initial = clock.capture(start + Duration::from_millis(10), wall, 0);
        assert_eq!(initial.sampled_at, 1_700_000_000);
        assert_eq!(initial.sample_seconds, 0.0);
        let next = clock.capture(
            start + Duration::from_millis(1260),
            wall - Duration::from_secs(60),
            0,
        );
        assert_eq!(next.sampled_at, 1_699_999_940);
        assert_eq!(next.sample_seconds, 1.25);
        assert_eq!(next.network_seconds, 1.25);
    }

    #[test]
    fn resume_skips_history_interval_but_keeps_network_elapsed_time() {
        let start = Instant::now();
        let mut clock = SamplingClock::new(start);
        clock.capture(start, UNIX_EPOCH, 0);
        let resumed = clock.capture(start + Duration::from_secs(30), UNIX_EPOCH, 2);
        assert_eq!(resumed.sample_seconds, 0.0);
        assert_eq!(resumed.network_seconds, 30.0);
        let next = clock.capture(start + Duration::from_millis(31500), UNIX_EPOCH, 2);
        assert_eq!(next.sample_seconds, 1.5);
    }

    #[test]
    fn a_sample_captured_during_pause_breaks_history_continuity() {
        let start = Instant::now();
        let mut clock = SamplingClock::new(start);
        clock.capture(start, UNIX_EPOCH, 0);
        let paused = clock.capture(start + Duration::from_secs(2), UNIX_EPOCH, 1);
        assert_eq!(paused.sample_seconds, 0.0);
        let resumed = clock.capture(start + Duration::from_secs(4), UNIX_EPOCH, 2);
        assert_eq!(resumed.sample_seconds, 0.0);
    }

    #[test]
    fn pause_and_resume_between_ticks_still_skip_the_interrupted_interval() {
        let collector = Collector {
            snapshot: Arc::default(),
            alerts: Arc::default(),
            alert_config: Arc::new(RwLock::new(AlertPreferences {
                revision: 0,
                config: crate::alerts::AlertConfig::default(),
            })),
            desktop_notifications: crate::desktop_notifications::DesktopNotifications::start(
                Arc::new(AtomicBool::new(true)),
            ),
            interval_ms: Arc::new(AtomicU64::new(2000)),
            stop: Arc::new(AtomicBool::new(false)),
            pause_state: Arc::new(AtomicU64::new(0)),
            exited: Arc::new(AtomicBool::new(false)),
        };
        let start = Instant::now();
        let mut clock = SamplingClock::new(start);
        clock.capture(
            start,
            UNIX_EPOCH,
            collector.pause_state.load(Ordering::Relaxed),
        );
        collector.set_paused(true);
        collector.set_paused(false);
        let resumed = clock.capture(
            start + Duration::from_secs(2),
            UNIX_EPOCH,
            collector.pause_state.load(Ordering::Relaxed),
        );
        assert_eq!(resumed.sample_seconds, 0.0);
        assert_eq!(resumed.network_seconds, 2.0);
        // Repeating the same unpaused setting must not discard a valid interval.
        collector.set_paused(false);
        let next = clock.capture(
            start + Duration::from_secs(4),
            UNIX_EPOCH,
            collector.pause_state.load(Ordering::Relaxed),
        );
        assert_eq!(next.sample_seconds, 2.0);
    }

    fn frame(epoch: u64, sequence: u64, bytes_per_second: f64) -> NetworkFrame {
        let mut net = NetworkFrame {
            epoch,
            sequence,
            seconds_per_sample: 2.0,
            captured_at: 1000,
            rates: HashMap::new(),
            process_lifetimes: HashMap::new(),
            exact_bytes: HashMap::new(),
        };
        net.rates.insert(
            42,
            NetworkRate {
                upload_bps: bytes_per_second,
                download_bps: bytes_per_second / 2.0,
            },
        );
        net
    }

    #[test]
    fn native_lifetime_prevents_same_second_pid_reuse_and_preserves_exact_bytes() {
        let mut ledger = TrafficLedger::default();
        let mut old = frame(1, 1, 0.0);
        old.process_lifetimes.insert(42, 123_000_001);
        old.exact_bytes.insert(42, (u64::MAX - 8, 17));
        ledger.record_frame(&old, |_| Some((123, Some(123_000_002))));
        assert!(ledger.get(42, 123, 123_000_001).is_none());
        ledger.record_frame(&frame(1, 2, 0.0), |_| Some((123, None)));
        old.sequence = 3;
        ledger.record_frame(&old, |_| Some((123, Some(123_000_001))));
        assert_eq!(
            ledger.get(42, 123, 123_000_001).unwrap().uploaded_bytes,
            u64::MAX - 8
        );
        assert_eq!(
            ledger.get(42, 123, 123_000_001).unwrap().downloaded_bytes,
            17
        );
        ledger.retain_running(|_, _, token| token == 123_000_002);
        assert!(ledger.get(42, 123, 123_000_001).is_none());
    }

    #[test]
    fn only_count_unique_frames_and_separate_reused_pids() {
        let mut ledger = TrafficLedger::default();
        let sample = frame(1, 1, 100.0);
        ledger.record_frame(&sample, |_| Some((123, Some(123_000_001))));
        ledger.record_frame(&sample, |_| Some((123, Some(123_000_001))));
        assert_eq!(
            ledger.get(42, 123, 123_000_001).unwrap().uploaded_bytes,
            200
        );
        assert_eq!(
            ledger.get(42, 123, 123_000_001).unwrap().downloaded_bytes,
            100
        );
        ledger.retain_running(|_, start, _| start != 123);
        ledger.record_frame(&frame(1, 2, 50.0), |_| Some((456, Some(456_000_001))));
        assert!(ledger.get(42, 123, 123_000_001).is_none());
        assert_eq!(
            ledger.get(42, 456, 456_000_001).unwrap().uploaded_bytes,
            100
        );
    }

    #[test]
    fn a_new_session_epoch_can_reuse_sequence_number() {
        let mut ledger = TrafficLedger::default();
        ledger.record_frame(&frame(1, 1, 1.0), |_| Some((1, Some(1_000_001))));
        ledger.record_frame(&frame(2, 1, 1.0), |_| Some((1, Some(1_000_001))));
        assert_eq!(ledger.get(42, 1, 1_000_001).unwrap().uploaded_bytes, 4);
    }

    #[test]
    fn buffered_frame_does_not_charge_a_process_started_after_capture() {
        let mut ledger = TrafficLedger::default();
        ledger.record_frame(&frame(1, 1, 50.0), |_| Some((1001, Some(1_001_000_001))));
        assert!(ledger.get(42, 1001, 1_001_000_001).is_none());
    }
    #[test]
    fn linux_pid_only_frames_require_native_identity_and_process_birth_before_whole_interval() {
        let mut ledger = TrafficLedger::default();
        let sample = frame(1, 1, 100.0);
        // Captured at 1000, two seconds of rates, plus one-second timestamp uncertainty.
        ledger.record_frame(&sample, |_| Some((997, Some(1234))));
        assert_eq!(
            ledger.get(42, 997, 1234).map(|v| v.uploaded_bytes),
            Some(200)
        );
        let mut too_new = frame(1, 2, 100.0);
        too_new.captured_at = 1000;
        ledger.record_frame(&too_new, |_| Some((998, Some(1235))));
        assert!(ledger.get(42, 998, 1235).is_none());
        ledger.record_frame(&frame(1, 3, 100.0), |_| Some((900, None)));
        assert!(ledger.get(42, 900, 900).is_none());
    }
    #[test]
    fn pid_only_same_second_reuse_never_inherits_totals_and_out_of_order_frames_are_not_recounted()
    {
        let mut ledger = TrafficLedger::new(false);
        ledger.record_frame(&frame(1, 1, 100.0), |_| Some((123, Some(100))));
        ledger.record_frame(&frame(1, 2, 50.0), |_| Some((123, Some(101))));
        assert_eq!(ledger.get(42, 123, 100).unwrap().uploaded_bytes, 200);
        assert_eq!(ledger.get(42, 123, 101).unwrap().uploaded_bytes, 100);
        ledger.retain_running(|pid, started, token| pid == 42 && started == 123 && token == 101);
        assert!(ledger.get(42, 123, 100).is_none());
        ledger.record_frame(&frame(1, 1, 100.0), |_| Some((123, Some(101))));
        assert_eq!(ledger.get(42, 123, 101).unwrap().uploaded_bytes, 100);
    }

    #[test]
    fn native_wire_identity_is_required_and_zero_exact_bytes_are_preserved() {
        let mut ledger = TrafficLedger::new(true);
        ledger.record_frame(&frame(1, 1, 100.0), |_| Some((123, Some(100))));
        assert!(ledger.get(42, 123, 100).is_none());
        let mut wire = frame(1, 2, 9000.0);
        wire.process_lifetimes.insert(42, 100);
        wire.exact_bytes.insert(42, (0, 0));
        ledger.record_frame(&wire, |_| Some((123, Some(101))));
        assert!(ledger.get(42, 123, 101).is_none());
        wire.sequence = 3;
        ledger.record_frame(&wire, |_| Some((123, Some(100))));
        assert_eq!(ledger.get(42, 123, 100).unwrap().uploaded_bytes, 0);
        assert_eq!(ledger.get(42, 123, 100).unwrap().downloaded_bytes, 0);
    }

    #[test]
    fn live_pid_rates_require_exact_current_identity_matching_frame_and_complete_birth_window() {
        let mut ledger = TrafficLedger::new(false);
        ledger.record_frame(&frame(1, 1, 100.0), |_| Some((997, Some(100))));
        let mut row = ProcessRow {
            pid: 42,
            started_at: 997,
            lifetime: Some(100),
            ..Default::default()
        };
        let mut net = NetworkSample::default();
        net.epoch = 1;
        net.sequence = 1;
        net.is_available = true;
        net.last_frame_at = Some(Instant::now());
        assert!(ledger.pid_estimate_available(&row, &net, Some(100)));
        assert!(!ledger.pid_estimate_available(&row, &net, None));
        assert!(!ledger.pid_estimate_available(&row, &net, Some(101)));
        row.started_at = 998;
        assert!(!ledger.pid_estimate_available(&row, &net, Some(100)));
        row.started_at = 997;
        net.sequence = 2;
        assert!(!ledger.pid_estimate_available(&row, &net, Some(100)));
        net.sequence = 1;
        net.epoch = 2;
        assert!(!ledger.pid_estimate_available(&row, &net, Some(100)));
        net.epoch = 1;
        net.last_frame_at = Some(Instant::now() - Duration::from_secs(8));
        assert!(!ledger.pid_estimate_available(&row, &net, Some(100)));
    }

    #[test]
    fn buffered_frames_during_pause_count_once_in_session_totals() {
        let mut ledger = TrafficLedger::new(false);
        for sequence in 1..=3 {
            let mut buffered = frame(1, sequence, 100.0);
            buffered.captured_at = 1000 + sequence * 2;
            ledger.record_frame(&buffered, |_| Some((900, Some(100))));
        }
        assert_eq!(ledger.get(42, 900, 100).unwrap().uploaded_bytes, 600);
        ledger.record_frame(&frame(1, 3, 100.0), |_| Some((900, Some(100))));
        assert_eq!(ledger.get(42, 900, 100).unwrap().uploaded_bytes, 600);
    }
}
