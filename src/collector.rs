use crate::gpu::GpuUsage;
#[cfg(target_os = "linux")]
use crate::gpu::drm::DrmSampler;
use crate::model::{ProcessRow, Snapshot};
use crate::network::NetworkSample;
use crate::nvidia::{NvidiaState, start_nvidia_sampler};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Networks, ProcessesToUpdate, System};

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
    fn record_frame(&mut self, net: &NetworkSample, mut start_time: impl FnMut(u32) -> Option<u64>) {
        if !net.is_available || self.last_frame == Some((net.epoch, net.sequence)) {
            return;
        }
        self.last_frame = Some((net.epoch, net.sequence));
        for (pid, rate) in &net.rates {
            let Some(started) = start_time(*pid) else { continue };
            let total = self.totals.entry((*pid, started)).or_default();
            let up = (rate.upload_bps.max(0.0) * net.seconds_per_sample.max(0.0)) as u64;
            let down = (rate.download_bps.max(0.0) * net.seconds_per_sample.max(0.0)) as u64;
            total.uploaded_bytes = total.uploaded_bytes.saturating_add(up);
            total.downloaded_bytes = total.downloaded_bytes.saturating_add(down);
        }
    }

    fn retain_running(&mut self, mut alive: impl FnMut(u32, u64) -> bool) {
        self.totals.retain(|(pid, started), _| alive(*pid, *started));
    }

    fn get(&self, pid: u32, started: u64) -> Option<ProcessTrafficTotal> {
        self.totals.get(&(pid, started)).copied()
    }
}

pub struct Collector {
    pub snapshot: Arc<RwLock<Snapshot>>,
    pub interval_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl Collector {
    pub fn start(interval: u64, traffic: Arc<Mutex<NetworkSample>>) -> Self {
        let snapshot = Arc::new(RwLock::new(Snapshot::default()));
        let interval_ms = Arc::new(AtomicU64::new(interval));
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let nvidia_state = Arc::new(RwLock::new(NvidiaState::default()));
        start_nvidia_sampler(Arc::clone(&stop), Arc::clone(&nvidia_state));
        let shared_snapshot = Arc::clone(&snapshot);
        let shared_stop = Arc::clone(&stop);
        let shared_paused = Arc::clone(&paused);
        let shared_interval = Arc::clone(&interval_ms);
        thread::spawn(move || {
            run_collector(
                shared_snapshot,
                shared_interval,
                shared_stop,
                shared_paused,
                traffic,
                nvidia_state,
            )
        });
        Self {
            snapshot,
            interval_ms,
            stop,
            paused,
        }
    }

    pub fn set_paused(&self, value: bool) {
        self.paused.store(value, Ordering::Relaxed);
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
    output: Arc<RwLock<Snapshot>>,
    interval: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    net_state: Arc<Mutex<NetworkSample>>,
    nvidia_state: Arc<RwLock<NvidiaState>>,
) {
    let mut system = System::new_all();
    let mut networks = Networks::new_with_refreshed_list();
    let mut last_refresh = Instant::now();
    let mut traffic = TrafficLedger::default();
    #[cfg(target_os = "linux")]
    let mut drm = DrmSampler::new();
    while !stop.load(Ordering::Relaxed) {
        if paused.load(Ordering::Relaxed) {
            last_refresh = Instant::now();
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let tick = Instant::now();
        let elapsed_seconds = last_refresh.elapsed().as_secs_f64().max(0.001);
        last_refresh = Instant::now();
        system.refresh_cpu_usage();
        system.refresh_memory();
        system.refresh_processes(ProcessesToUpdate::All, true);
        networks.refresh(true);
        let net = net_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let pids = system.processes().keys().map(|pid| pid.as_u32()).collect::<Vec<_>>();

        #[cfg(target_os = "linux")]
        let mut gpu = drm.sample(&pids);
        #[cfg(not(target_os = "linux"))]
        let mut gpu: HashMap<u32, GpuUsage> = HashMap::new();
        let nvidia = nvidia_state.read().unwrap_or_else(|e| e.into_inner()).clone();
        if nvidia
            .updated_at
            .is_some_and(|last| last.elapsed() < Duration::from_secs(8))
        {
            for (pid, value) in nvidia.data {
                gpu.entry(pid).or_default().combine(value);
            }
        }

        // A frame's (epoch, sequence) is unique across toggles and collector ticks.
        traffic.record_frame(&net, |pid| {
            system.process(sysinfo::Pid::from_u32(pid)).map(|p| p.start_time())
        });
        traffic.retain_running(|pid, started| {
            system.process(sysinfo::Pid::from_u32(pid))
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
                count: 1,
                name: process.name().to_string_lossy().to_string(),
                executable: process.exe().map_or_else(String::new, |p| p.display().to_string()),
                cpu_pct: (process.cpu_usage() / cores).clamp(0.0, 100.0),
                gpu_pct: gpu_usage.utilization_pct,
                gpu_memory_bytes: gpu_usage.memory_bytes,
                ram_bytes: process.memory(),
                down_bps: net.is_available.then_some(
                    network_rate.map_or(0.0, |entry| entry.download_bps),
                ),
                up_bps: net.is_available.then_some(
                    network_rate.map_or(0.0, |entry| entry.upload_bps),
                ),
                downloaded_bytes: net.is_available.then_some(
                    total.map_or(0, |entry| entry.downloaded_bytes),
                ),
                uploaded_bytes: net.is_available.then_some(
                    total.map_or(0, |entry| entry.uploaded_bytes),
                ),
            });
        }
        let mut rx_bps = 0.0;
        let mut tx_bps = 0.0;
        let mut interface_count = 0;
        for (name, data) in &networks {
            if name == "lo" || name.to_lowercase().contains("loopback") {
                continue;
            }
            rx_bps += data.received() as f64 / elapsed_seconds;
            tx_bps += data.transmitted() as f64 / elapsed_seconds;
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
            process_rows: processes,
            cpu_pct: system.global_cpu_usage().clamp(0.0, 100.0),
            ram_total: system.total_memory(),
            ram_used: system.used_memory(),
            down_bps: rx_bps,
            up_bps: tx_bps,
            interface_count,
            network_status: net.status,
            gpu_status,
            updated_at: Some(Instant::now()),
        };
        *output.write().unwrap_or_else(|e| e.into_inner()) = next;
        let remaining = Duration::from_millis(interval.load(Ordering::Relaxed))
            .saturating_sub(tick.elapsed());
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

    fn frame(epoch: u64, sequence: u64, bytes_per_second: f64) -> NetworkSample {
        let mut net = NetworkSample {
            epoch,
            sequence,
            is_available: true,
            seconds_per_sample: 2.0,
            ..NetworkSample::default()
        };
        net.rates.insert(42, NetworkRate {
            upload_bps: bytes_per_second,
            download_bps: bytes_per_second / 2.0,
        });
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
}
