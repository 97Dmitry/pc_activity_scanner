//! Process I/O and block-device activity are deliberately independent sources.
use crate::metrics::{DeviceRow, Measurements, MetricKind};
use crate::model::ProcessRow;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
struct IoReading {
    lifetime: u64,
    read: u64,
    write: u64,
}

#[derive(Default)]
struct IoLedger {
    previous: HashMap<u32, IoReading>,
}

impl IoLedger {
    fn record(&mut self, pid: u32, current: IoReading, seconds: f64) -> Measurements {
        let old = self.previous.insert(pid, current);
        let mut result = Measurements::new();
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 15.0 {
            return result;
        }
        let Some(old) = old.filter(|old| old.lifetime == current.lifetime) else {
            return result;
        };
        for (kind, value, previous) in [
            (MetricKind::IoRead, current.read, old.read),
            (MetricKind::IoWrite, current.write, old.write),
        ] {
            if let Some(delta) = value.checked_sub(previous) {
                result.insert(kind, delta as f64 / seconds);
            }
        }
        result
    }
    fn retain(&mut self, running: &[(u32, u64)]) {
        let running: std::collections::HashSet<_> = running.iter().copied().collect();
        self.previous
            .retain(|pid, value| running.contains(&(*pid, value.lifetime)));
    }
}

#[derive(Default)]
pub struct DiskSampler {
    processes: IoLedger,
    devices: platform::DeviceSampler,
    sampled_at: Option<Instant>,
}
impl DiskSampler {
    pub fn sample(&mut self, rows: &mut [ProcessRow], seconds: f64) -> (Vec<DeviceRow>, String) {
        let now = Instant::now();
        // The collector's interval defines continuity. The actual source interval
        // defines rates, so a slow source cannot charge its delay to the next tick.
        let elapsed = self
            .sampled_at
            .map_or(0.0, |at| now.duration_since(at).as_secs_f64());
        self.sampled_at = Some(now);
        let elapsed = if seconds.is_finite() && seconds > 0.0 && seconds <= 15.0 {
            elapsed
        } else {
            0.0
        };
        let mut running = Vec::with_capacity(rows.len());
        let mut unavailable = 0;
        for row in rows {
            row.metrics.remove(&MetricKind::IoRead);
            row.metrics.remove(&MetricKind::IoWrite);
            if let Some(reading) = platform::read_process(row) {
                row.lifetime = Some(reading.lifetime);
                row.metrics
                    .extend(self.processes.record(row.pid, reading, elapsed));
                running.push((row.pid, reading.lifetime));
            } else {
                unavailable += 1;
            }
        }
        self.processes.retain(&running);
        let (devices, device_status) = self.devices.sample(elapsed);
        (
            devices,
            format!(
                "{}; I/O процессов недоступен: {unavailable}. {device_status}",
                platform::PROCESS_SOURCE
            ),
        )
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::fs;
    use std::path::Path;
    pub const PROCESS_SOURCE: &str = "Linux /proc/PID/io: storage I/O, не файловые syscall-байты";

    unsafe extern "C" {
        fn sysconf(name: i32) -> std::ffi::c_long;
    }
    fn clock_ticks() -> Option<u64> {
        static TICKS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        // _SC_CLK_TCK is 2 on Linux (glibc/musl); it is not the scheduler HZ.
        *TICKS.get_or_init(|| u64::try_from(unsafe { sysconf(2) }).ok().filter(|v| *v > 0))
    }
    fn stat_start(text: &str) -> Option<u64> {
        text.rsplit_once(") ")?
            .1
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }
    fn parse_io(text: &str) -> Option<(u64, u64)> {
        let mut read = None;
        let mut write = None;
        for line in text.lines() {
            let (key, value) = line.split_once(':')?;
            match key {
                "read_bytes" => read = value.trim().parse().ok(),
                "write_bytes" => write = value.trim().parse().ok(),
                _ => {}
            }
        }
        Some((read?, write?))
    }
    pub fn read_process(row: &ProcessRow) -> Option<IoReading> {
        static BOOT_TIME: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let boot_time = *BOOT_TIME.get_or_init(sysinfo::System::boot_time);
        let root = format!("/proc/{}", row.pid);
        let lifetime = stat_start(&fs::read_to_string(format!("{root}/stat")).ok()?)?;
        if row.lifetime.is_some_and(|token| token != lifetime) {
            return None;
        }
        if row.lifetime.is_none()
            && boot_time.checked_add(lifetime / clock_ticks()?)? != row.started_at
        {
            return None;
        }
        let (read, write) = parse_io(&fs::read_to_string(format!("{root}/io")).ok()?)?;
        // The process may exit/reuse its PID between the two procfs opens.
        if stat_start(&fs::read_to_string(format!("{root}/stat")).ok()?)? != lifetime {
            return None;
        }
        Some(IoReading {
            lifetime,
            read,
            write,
        })
    }
    #[derive(Clone, Copy, Debug)]
    struct DeviceCounters {
        read_sectors: u64,
        write_sectors: u64,
        busy_ms: u64,
    }
    fn parse_diskstats(text: &str) -> Vec<(String, String, DeviceCounters)> {
        text.lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                Some((
                    fields.get(2)?.to_string(),
                    format!("{}:{}", fields.first()?, fields.get(1)?),
                    DeviceCounters {
                        read_sectors: fields.get(5)?.parse().ok()?,
                        write_sectors: fields.get(9)?.parse().ok()?,
                        busy_ms: fields.get(12)?.parse().ok()?,
                    },
                ))
            })
            .collect()
    }
    #[derive(Default)]
    pub struct DeviceSampler {
        previous: HashMap<String, DeviceCounters>,
    }
    impl DeviceSampler {
        pub fn sample(&mut self, seconds: f64) -> (Vec<DeviceRow>, String) {
            let Ok(text) = fs::read_to_string("/proc/diskstats") else {
                self.previous.clear();
                return (
                    Vec::new(),
                    "Источник накопителей /proc/diskstats недоступен".into(),
                );
            };
            let previous = std::mem::take(&mut self.previous);
            let mut rows = Vec::new();
            for (name, dev, current) in parse_diskstats(&text) {
                if rows.len() >= 256 {
                    break;
                }
                let root = Path::new("/sys/block").join(&name);
                // /sys/block has only whole devices. A device link excludes RAM/
                // loop devices; no stacked devices are added atop their leaves.
                if !root.join("device").exists()
                    || fs::read_dir(root.join("slaves"))
                        .ok()
                        .is_some_and(|mut entries| entries.next().is_some())
                {
                    continue;
                }
                let sequence = fs::read_to_string(root.join("diskseq")).unwrap_or_default();
                let identity = format!("{dev}:{}:{}", name, sequence.trim());
                let mut metrics = Measurements::new();
                if seconds.is_finite()
                    && seconds > 0.0
                    && seconds <= 15.0
                    && let Some(old) = previous.get(&identity)
                {
                    for (kind, delta, scale) in [
                        (
                            MetricKind::IoRead,
                            current.read_sectors.checked_sub(old.read_sectors),
                            512.0,
                        ),
                        (
                            MetricKind::IoWrite,
                            current.write_sectors.checked_sub(old.write_sectors),
                            512.0,
                        ),
                        (
                            MetricKind::DiskActive,
                            current.busy_ms.checked_sub(old.busy_ms),
                            0.1,
                        ),
                    ] {
                        if let Some(delta) = delta {
                            let value = delta as f64 * scale / seconds;
                            metrics.insert(
                                kind,
                                if kind == MetricKind::DiskActive {
                                    value.min(100.0)
                                } else {
                                    value
                                },
                            );
                        }
                    }
                }
                self.previous.insert(identity.clone(), current);
                rows.push(DeviceRow {
                    id: format!("disk:{identity}"),
                    name,
                    metrics,
                });
            }
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            (
                rows,
                "Накопители: leaf block devices /proc/diskstats (в VM — виртуальные устройства)"
                    .into(),
            )
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn parser_keeps_storage_not_syscall_bytes_and_handles_process_names() {
            assert_eq!(
                parse_io(
                    "rchar: 99999\nwchar: 8888\nread_bytes: 42\nwrite_bytes: 84\ncancelled_write_bytes: 10"
                ),
                Some((42, 84))
            );
            assert_eq!(parse_io("rchar: 9"), None);
            let mut fields = vec!["S".to_owned(); 20];
            fields[19] = "123456".into();
            assert_eq!(
                stat_start(&format!("42 (name ) with spaces) {}", fields.join(" "))),
                Some(123456)
            );
        }
        #[test]
        fn diskstats_indices_are_512_byte_sectors_and_busy_milliseconds() {
            let values = parse_diskstats("8 0 sda 10 11 12 13 14 15 16 17 18 19 20 21 22");
            assert_eq!(values.len(), 1);
            assert_eq!(values[0].2.read_sectors, 12);
            assert_eq!(values[0].2.write_sectors, 16);
            assert_eq!(values[0].2.busy_ms, 19);
            assert!(parse_diskstats("8 0 sda 1").is_empty());
        }
        #[test]
        fn native_linux_reads_self_lifetime_and_bounded_real_write() {
            use std::io::Write;
            let mut system = sysinfo::System::new();
            let pid = sysinfo::Pid::from_u32(std::process::id());
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            let process = system.process(pid).unwrap();
            let row = ProcessRow {
                pid: pid.as_u32(),
                started_at: process.start_time(),
                ..Default::default()
            };
            let before = read_process(&row).expect("self /proc/PID/io must be readable");
            let mut fixture = tempfile::NamedTempFile::new_in(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
            )
            .unwrap();
            fixture.write_all(&[42_u8; 65536]).unwrap();
            fixture.as_file().sync_all().unwrap();
            let after = read_process(&row).unwrap();
            assert_eq!(before.lifetime, after.lifetime);
            assert!(
                after.write >= before.write + 65536,
                "storage write counter must reflect fixture write"
            );
            eprintln!(
                "Linux self I/O write delta={} bytes; lifetime={}",
                after.write - before.write,
                after.lifetime
            );
            let mut wrong = row;
            wrong.lifetime = Some(after.lifetime + 1);
            assert!(read_process(&wrong).is_none());
            let mut devices = DeviceSampler::default();
            let (first, status) = devices.sample(0.0);
            eprintln!("Linux device source: {status}; devices={}", first.len());
            assert!(first.iter().all(|d| d.metrics.is_empty()));
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessIoCounters, GetProcessTimes, IO_COUNTERS, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    pub const PROCESS_SOURCE: &str =
        "Windows Process I/O: file/network/device bytes, не физический disk I/O";
    pub fn read_process(row: &ProcessRow) -> Option<IoReading> {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, row.pid);
            if process.is_null() {
                return None;
            }
            let mut created: FILETIME = std::mem::zeroed();
            let mut exited: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            let mut counters: IO_COUNTERS = std::mem::zeroed();
            let valid = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user)
                != 0
                && GetProcessIoCounters(process, &mut counters) != 0;
            CloseHandle(process);
            if !valid {
                return None;
            }
            let lifetime = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
            if row.lifetime.is_some_and(|token| token != lifetime)
                || row.lifetime.is_none()
                    && lifetime
                        .checked_div(10_000_000)?
                        .checked_sub(11_644_473_600)?
                        != row.started_at
            {
                return None;
            }
            Some(IoReading {
                lifetime,
                read: counters.ReadTransferCount,
                write: counters.WriteTransferCount,
            })
        }
    }
    #[derive(Default)]
    pub struct DeviceSampler {
        query: Option<crate::pdh::Query>,
        retry_after: Option<Instant>,
        warm: bool,
    }
    impl DeviceSampler {
        pub fn sample(&mut self, seconds: f64) -> (Vec<DeviceRow>, String) {
            if self.query.is_none() {
                if self
                    .retry_after
                    .is_some_and(|at| at.elapsed().as_secs() < 30)
                {
                    return (
                        Vec::new(),
                        "PDH PhysicalDisk недоступен; повтор через 30 с".into(),
                    );
                }
                match crate::pdh::Query::new(&[
                    r"\PhysicalDisk(*)\Disk Read Bytes/sec",
                    r"\PhysicalDisk(*)\Disk Write Bytes/sec",
                    r"\PhysicalDisk(*)\% Idle Time",
                ]) {
                    Ok(query) => self.query = Some(query),
                    Err(error) => {
                        self.retry_after = Some(Instant::now());
                        return (Vec::new(), error);
                    }
                }
            }
            let query = self.query.as_mut().unwrap();
            if let Err(error) = query.collect() {
                self.query = None;
                self.retry_after = Some(Instant::now());
                self.warm = false;
                return (Vec::new(), format!("PDH PhysicalDisk: {error}"));
            }
            let valid = self.warm && seconds > 0.0 && seconds <= 15.0;
            self.warm = true;
            if !valid {
                return (
                    Vec::new(),
                    "PDH PhysicalDisk: ожидаем второй непрерывный замер".into(),
                );
            }
            let mut rows: HashMap<String, Measurements> = HashMap::new();
            let mut errors = Vec::new();
            for (index, kind) in [
                MetricKind::IoRead,
                MetricKind::IoWrite,
                MetricKind::DiskActive,
            ]
            .into_iter()
            .enumerate()
            {
                match query.values(index) {
                    Ok(values) => {
                        for (name, value) in values {
                            if name.eq_ignore_ascii_case("_total") || name.is_empty() {
                                continue;
                            }
                            if rows.len() >= 256 && !rows.contains_key(&name) {
                                continue;
                            }
                            let value = if kind == MetricKind::DiskActive {
                                100.0 - value.clamp(0.0, 100.0)
                            } else {
                                value
                            };
                            rows.entry(name).or_default().insert(kind, value);
                        }
                    }
                    Err(error) => errors.push(error),
                }
            }
            let mut rows: Vec<_> = rows
                .into_iter()
                .map(|(name, metrics)| DeviceRow {
                    id: format!("disk:windows:{name}"),
                    name,
                    metrics,
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            (
                rows,
                if errors.is_empty() {
                    "PDH PhysicalDisk: скорость и 100 − % Idle Time; _Total исключён".into()
                } else {
                    format!("PDH PhysicalDisk: {}", errors.join("; "))
                },
            )
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn native_windows_process_io_reads_real_file_write_without_elevation() {
            use std::io::Write;
            let mut system = sysinfo::System::new();
            let pid = sysinfo::Pid::from_u32(std::process::id());
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            let row = ProcessRow {
                pid: pid.as_u32(),
                started_at: system.process(pid).unwrap().start_time(),
                ..Default::default()
            };
            let before = read_process(&row).expect("self Process I/O must be readable");
            let mut fixture = tempfile::NamedTempFile::new().unwrap();
            fixture.write_all(&[42_u8; 65536]).unwrap();
            fixture.as_file().sync_all().unwrap();
            let after = read_process(&row).unwrap();
            assert_eq!(before.lifetime, after.lifetime);
            assert!(after.write >= before.write + 65536);
            eprintln!(
                "Windows self Process I/O write delta={} bytes; lifetime={}",
                after.write - before.write,
                after.lifetime
            );
            let mut wrong = row;
            wrong.lifetime = Some(after.lifetime + 1);
            assert!(read_process(&wrong).is_none());
        }
        #[test]
        fn native_windows_physical_disk_reports_available_source_or_explicit_failure() {
            let mut sampler = DeviceSampler::default();
            let _ = sampler.sample(0.0);
            std::thread::sleep(std::time::Duration::from_millis(1100));
            let (rows, status) = sampler.sample(1.1);
            eprintln!("Windows PhysicalDisk: {status}; devices={rows:?}");
            assert!(
                rows.iter()
                    .all(|row| !row.name.eq_ignore_ascii_case("_Total"))
            );
            assert!(!status.is_empty());
            if !rows.is_empty() {
                assert!(
                    rows.iter()
                        .any(|row| row.metrics.contains_key(&MetricKind::IoRead))
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(lifetime: u64, read: u64, write: u64) -> IoReading {
        IoReading {
            lifetime,
            read,
            write,
        }
    }
    #[test]
    fn io_deltas_reset_unknown_reuse_exit_and_pauses() {
        let mut ledger = IoLedger::default();
        assert!(ledger.record(1, reading(100, 1024, 2048), 0.0).is_empty());
        let rates = ledger.record(1, reading(100, 2048, 4096), 2.0);
        assert_eq!(rates.get(&MetricKind::IoRead), Some(&512.0));
        assert_eq!(rates.get(&MetricKind::IoWrite), Some(&1024.0));
        let reset = ledger.record(1, reading(100, 0, 8192), 2.0);
        assert!(!reset.contains_key(&MetricKind::IoRead));
        assert_eq!(reset.get(&MetricKind::IoWrite), Some(&2048.0));
        assert!(ledger.record(1, reading(101, 400, 800), 1.0).is_empty());
        ledger.retain(&[]);
        assert!(ledger.record(1, reading(101, 500, 900), 1.0).is_empty());
        assert!(ledger.record(1, reading(101, 600, 1000), 0.0).is_empty());
        assert_eq!(
            ledger.record(1, reading(101, 700, 1100), 1.0)[&MetricKind::IoRead],
            100.0
        );
        assert!(ledger.record(1, reading(101, 800, 1200), 16.0).is_empty());
    }
    #[test]
    fn missing_sample_does_not_bridge_the_gap() {
        let mut ledger = IoLedger::default();
        ledger.record(1, reading(100, 100, 200), 0.0);
        ledger.retain(&[]);
        assert!(ledger.record(1, reading(100, 900, 1000), 1.0).is_empty());
    }
}
