use crate::gpu::GpuUsage;
#[cfg(target_os = "linux")]
use crate::gpu::drm::DrmSampler;
use crate::model::{ProcessRow, Snapshot};
use crate::network::{NetworkFrame, NetworkSample};
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

/// Pure accounting logic, independently unit-tested with synthetic process lifetimes.
#[derive(Default)]
struct TrafficLedger {
    last_frame: Option<(u64, u64)>,
    totals: HashMap<(u32, u64), ProcessTrafficTotal>,
}

impl TrafficLedger {
    fn record_frame(&mut self, net: &NetworkFrame, mut start_time: impl FnMut(u32) -> Option<u64>) {
        if self.last_frame == Some((net.epoch, net.sequence)) {
            return;
        }
        self.last_frame = Some((net.epoch, net.sequence));
        for (pid, rate) in &net.rates {
            let Some(started) = start_time(*pid) else {
                continue;
            };
            if started > net.captured_at {
                continue;
            }
            let total = self.totals.entry((*pid, started)).or_default();
            let up = (rate.upload_bps.max(0.0) * net.seconds_per_sample.max(0.0)) as u64;
            let down = (rate.download_bps.max(0.0) * net.seconds_per_sample.max(0.0)) as u64;
            total.uploaded_bytes = total.uploaded_bytes.saturating_add(up);
            total.downloaded_bytes = total.downloaded_bytes.saturating_add(down);
        }
    }

    fn retain_running(&mut self, mut alive: impl FnMut(u32, u64) -> bool) {
        self.totals
            .retain(|(pid, started), _| alive(*pid, *started));
    }

    fn get(&self, pid: u32, started: u64) -> Option<ProcessTrafficTotal> {
        self.totals.get(&(pid, started)).copied()
    }
}

pub struct Collector {
    pub snapshot: Arc<RwLock<Arc<Snapshot>>>,
    interval_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    // Low bit is pause status; every transition advances the generation, including
    // a pause/resume pair occurring entirely between collector ticks.
    pause_state: Arc<AtomicU64>,
}

impl Collector {
    pub fn start(
        interval: u64,
        traffic: Arc<Mutex<NetworkSample>>,
        recorder: crate::history::HistoryRecorder,
    ) -> Self {
        let snapshot = Arc::new(RwLock::new(Arc::new(Snapshot::default())));
        let interval_ms = Arc::new(AtomicU64::new(interval));
        let stop = Arc::new(AtomicBool::new(false));
        let pause_state = Arc::new(AtomicU64::new(0));
        let nvidia_state = Arc::new(RwLock::new(NvidiaState::default()));
        start_nvidia_sampler(Arc::clone(&stop), Arc::clone(&nvidia_state));
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
                nvidia_state,
                recorder,
            )
        });
        Self {
            snapshot,
            interval_ms,
            stop,
            pause_state,
        }
    }

    pub fn set_paused(&self, value: bool) {
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
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_collector(
    output: Arc<RwLock<Arc<Snapshot>>>,
    interval: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    pause_state: Arc<AtomicU64>,
    net_state: Arc<Mutex<NetworkSample>>,
    nvidia_state: Arc<RwLock<NvidiaState>>,
    recorder: crate::history::HistoryRecorder,
) {
    let mut system = System::new();
    // We display process CPU, memory and executable paths, not thread or disk I/O data.
    let process_refresh = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    system.refresh_cpu_usage();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh);
    thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    let mut networks = Networks::new_with_refreshed_list();
    let mut clock = SamplingClock::new(Instant::now());
    let mut traffic = TrafficLedger::default();
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
        #[cfg(not(target_os = "linux"))]
        let mut gpu: HashMap<u32, GpuUsage> = HashMap::new();
        let nvidia = nvidia_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if nvidia
            .updated_at
            .is_some_and(|last| last.elapsed() < Duration::from_secs(8))
        {
            for (pid, value) in nvidia.data {
                gpu.entry(pid).or_default().combine(value);
            }
        }

        // A frame's (epoch, sequence) is unique across toggles and collector ticks.
        for frame in &frames {
            traffic.record_frame(frame, |pid| {
                system
                    .process(sysinfo::Pid::from_u32(pid))
                    .map(|p| p.start_time())
            });
        }
        traffic.retain_running(|pid, started| {
            system
                .process(sysinfo::Pid::from_u32(pid))
                .is_some_and(|p| p.start_time() == started)
        });

        let cores = system.cpus().len().max(1) as f32;
        let mut processes = Vec::with_capacity(system.processes().len());
        for (pid, process) in system.processes() {
            let pid_number = pid.as_u32();
            let gpu_usage = gpu.get(&pid_number).copied().unwrap_or(GpuUsage::default());
            let network_rate = net.rates.get(&pid_number);
            let total = traffic.get(pid_number, process.start_time());
            processes.push(ProcessRow {
                pid: pid_number,
                started_at: process.start_time(),
                count: 1,
                name: process.name().to_string_lossy().to_string(),
                executable: process
                    .exe()
                    .map_or_else(String::new, |p| p.display().to_string()),
                cpu_pct: (process.cpu_usage() / cores).clamp(0.0, 100.0),
                gpu_pct: gpu_usage.utilization_pct,
                gpu_memory_bytes: gpu_usage.memory_bytes,
                ram_bytes: process.memory(),
                down_bps: net
                    .is_available
                    .then_some(network_rate.map_or(0.0, |entry| entry.download_bps)),
                up_bps: net
                    .is_available
                    .then_some(network_rate.map_or(0.0, |entry| entry.upload_bps)),
                downloaded_bytes: net
                    .is_available
                    .then_some(total.map_or(0, |entry| entry.downloaded_bytes)),
                uploaded_bytes: net
                    .is_available
                    .then_some(total.map_or(0, |entry| entry.uploaded_bytes)),
            });
        }
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
        let next = Snapshot {
            sampled_at: timing.sampled_at,
            sample_seconds: timing.sample_seconds,
            process_rows: processes,
            cpu_pct: system.global_cpu_usage().clamp(0.0, 100.0),
            ram_total: system.total_memory(),
            ram_used: system.used_memory(),
            down_bps: rx_bps,
            up_bps: tx_bps,
            interface_count,
            network_status: net.status(),
            gpu_status,
            updated_at: Some(captured_at),
        };
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
            interval_ms: Arc::new(AtomicU64::new(2000)),
            stop: Arc::new(AtomicBool::new(false)),
            pause_state: Arc::new(AtomicU64::new(0)),
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
    fn only_count_unique_frames_and_separate_reused_pids() {
        let mut ledger = TrafficLedger::default();
        let sample = frame(1, 1, 100.0);
        ledger.record_frame(&sample, |_| Some(123));
        ledger.record_frame(&sample, |_| Some(123));
        assert_eq!(ledger.get(42, 123).unwrap().uploaded_bytes, 200);
        assert_eq!(ledger.get(42, 123).unwrap().downloaded_bytes, 100);
        ledger.retain_running(|_, start| start != 123);
        ledger.record_frame(&frame(1, 2, 50.0), |_| Some(456));
        assert!(ledger.get(42, 123).is_none());
        assert_eq!(ledger.get(42, 456).unwrap().uploaded_bytes, 100);
    }

    #[test]
    fn a_new_session_epoch_can_reuse_sequence_number() {
        let mut ledger = TrafficLedger::default();
        ledger.record_frame(&frame(1, 1, 1.0), |_| Some(1));
        ledger.record_frame(&frame(2, 1, 1.0), |_| Some(1));
        assert_eq!(ledger.get(42, 1).unwrap().uploaded_bytes, 4);
    }

    #[test]
    fn buffered_frame_does_not_charge_a_process_started_after_capture() {
        let mut ledger = TrafficLedger::default();
        ledger.record_frame(&frame(1, 1, 50.0), |_| Some(1001));
        assert!(ledger.get(42, 1001).is_none());
    }
}
