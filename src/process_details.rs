//! Authoritative per-process CPU/memory availability and native lifetime metadata.
use crate::metrics::MetricKind;
use crate::model::ProcessRow;
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::time::Instant;

/// Exact native creation token, read without sampling resources or elevation.
pub fn process_lifetime(pid: u32) -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        crate::network::process_creation_time(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux::lifetime(pid)
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Counters {
    pub cpu: Option<u64>,
    pub minor: Option<u64>,
    pub major: Option<u64>,
    pub total: Option<u64>,
}
#[derive(Default)]
pub(crate) struct CpuFaultLedger {
    previous: HashMap<u32, (u64, Counters)>,
}
impl CpuFaultLedger {
    pub(crate) fn apply(
        &mut self,
        row: &mut ProcessRow,
        counters: Counters,
        seconds: f64,
        frequency: f64,
        cores: usize,
    ) {
        row.cpu_pct = None;
        for key in [
            MetricKind::MinorFaults,
            MetricKind::MajorFaults,
            MetricKind::PageFaults,
        ] {
            row.metrics.remove(&key);
        }
        let Some(token) = row.lifetime else {
            self.previous.remove(&row.pid);
            return;
        };
        let previous = self
            .previous
            .insert(row.pid, (token, counters))
            .filter(|(old, _)| *old == token)
            .map(|(_, counters)| counters);
        if !seconds.is_finite()
            || !(0.0..=15.0).contains(&seconds)
            || seconds == 0.0
            || !frequency.is_finite()
            || frequency <= 0.0
        {
            return;
        }
        let Some(previous) = previous else {
            return;
        };
        let delta = |value: Option<u64>, old: Option<u64>| {
            value
                .zip(old)
                .and_then(|(value, old)| value.checked_sub(old))
        };
        row.cpu_pct = delta(counters.cpu, previous.cpu).map(|ticks| {
            (ticks as f64 / frequency / seconds / cores.max(1) as f64 * 100.0).clamp(0.0, 100.0)
                as f32
        });
        for (kind, value, old) in [
            (MetricKind::MinorFaults, counters.minor, previous.minor),
            (MetricKind::MajorFaults, counters.major, previous.major),
            (MetricKind::PageFaults, counters.total, previous.total),
        ] {
            if let Some(delta) = delta(value, old) {
                row.metrics.insert(kind, delta as f64 / seconds);
            }
        }
    }
    pub(crate) fn retain(&mut self, rows: &[ProcessRow]) {
        let identities: std::collections::HashSet<_> = rows
            .iter()
            .filter_map(|row| row.lifetime.map(|token| (row.pid, token)))
            .collect();
        self.previous
            .retain(|pid, (token, _)| identities.contains(&(*pid, *token)));
    }
}

#[derive(Default)]
pub struct ProcessDetails {
    #[cfg(target_os = "linux")]
    ledger: CpuFaultLedger,
    #[cfg(target_os = "linux")]
    sampled_at: Option<Instant>,
    #[cfg(target_os = "windows")]
    native: crate::windows_processes::ProcessSampler,
}
impl ProcessDetails {
    pub fn sample(&mut self, rows: &mut Vec<ProcessRow>, cores: usize, seconds: f64) -> String {
        #[cfg(target_os = "windows")]
        {
            self.native.sample(rows, cores, seconds)
        }
        #[cfg(target_os = "linux")]
        {
            let now = Instant::now();
            let elapsed = self
                .sampled_at
                .map_or(0.0, |at| now.duration_since(at).as_secs_f64());
            self.sampled_at = Some(now);
            let elapsed = if seconds.is_finite() && seconds > 0.0 && seconds <= 15.0 {
                elapsed
            } else {
                0.0
            };
            let frequency = linux::clock_ticks()
                .map(|value| value as f64)
                .unwrap_or(0.0);
            let mut unavailable = 0;
            rows.retain_mut(|row| {
                row.cpu_pct = None;
                row.ram_bytes = None;
                row.executable.clear();
                row.parent = None;
                row.parent_pid = None;
                for key in [
                    MetricKind::PrivateCommit,
                    MetricKind::ResidentShared,
                    MetricKind::Swap,
                    MetricKind::MinorFaults,
                    MetricKind::MajorFaults,
                    MetricKind::PageFaults,
                ] {
                    row.metrics.remove(&key);
                }
                match linux::read(row.pid) {
                    Ok(reading) => {
                        if self
                            .ledger
                            .previous
                            .get(&row.pid)
                            .is_none_or(|(token, _)| *token != reading.stat.started)
                        {
                            // Cached PID-only metrics cannot cross an exact
                            // lifetime transition discovered by this source.
                            row.gpu_pct = None;
                            row.gpu_memory_bytes = None;
                            row.down_bps = None;
                            row.up_bps = None;
                            row.downloaded_bytes = None;
                            row.uploaded_bytes = None;
                            row.metrics.clear();
                        }
                        row.started_at = reading.started_at;
                        row.lifetime = Some(reading.stat.started);
                        row.name = reading.stat.name;
                        row.parent_pid = (reading.stat.parent > 0).then_some(reading.stat.parent);
                        row.executable = reading.executable;
                        row.ram_bytes = reading.memory.resident;
                        if let Some(value) = reading.memory.shared {
                            row.metrics.insert(MetricKind::ResidentShared, value as f64);
                        }
                        if let Some(value) = reading.memory.swap {
                            row.metrics.insert(MetricKind::Swap, value as f64);
                        }
                        self.ledger.apply(
                            row,
                            Counters {
                                cpu: Some(reading.stat.cpu),
                                minor: Some(reading.stat.minor),
                                major: Some(reading.stat.major),
                                total: None,
                            },
                            elapsed,
                            frequency,
                            cores,
                        );
                        if row.ram_bytes.is_none() {
                            unavailable += 1;
                        }
                        true
                    }
                    Err(error) => {
                        row.lifetime = None;
                        unavailable += 1;
                        error.kind() != std::io::ErrorKind::NotFound
                    }
                }
            });
            self.ledger.retain(rows);
            format!(
                "Linux procfs: resident/shared/swap; RAM недоступна у {unavailable}/{} процессов; CPU/faults требуют двух непрерывных замеров",
                rows.len()
            )
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    #[derive(Debug, Default)]
    pub(super) struct Stat {
        pub name: String,
        pub parent: u32,
        pub started: u64,
        pub cpu: u64,
        pub minor: u64,
        pub major: u64,
    }
    #[derive(Debug, Default)]
    pub(super) struct Memory {
        pub resident: Option<u64>,
        pub shared: Option<u64>,
        pub swap: Option<u64>,
    }
    pub(super) fn parse_stat(text: &str) -> Option<Stat> {
        if text.len() > 64 * 1024 {
            return None;
        }
        let begin = text.find('(')?;
        let end = text.rfind(") ")?;
        if begin >= end {
            return None;
        }
        let fields: Vec<_> = text.get(end + 2..)?.split_whitespace().collect();
        let number = |index: usize| fields.get(index)?.parse::<u64>().ok();
        Some(Stat {
            name: text.get(begin + 1..end)?.to_owned(),
            parent: number(1)?.try_into().ok()?,
            started: number(19)?,
            cpu: number(11)?.checked_add(number(12)?)?,
            minor: number(7)?,
            major: number(9)?,
        })
    }
    pub(super) fn parse_memory(text: &str) -> Memory {
        let mut values = HashMap::new();
        for line in text.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if !["VmRSS", "RssFile", "RssShmem", "VmSwap"].contains(&name) {
                continue;
            }
            let mut fields = value.split_whitespace();
            let Some(bytes) = fields
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .and_then(|value| value.checked_mul(1024))
            else {
                continue;
            };
            if fields.next() != Some("kB") || fields.next().is_some() {
                continue;
            }
            values.insert(name, bytes);
        }
        Memory {
            resident: values.get("VmRSS").copied(),
            shared: values
                .get("RssFile")
                .copied()
                .zip(values.get("RssShmem").copied())
                .and_then(|(file, shmem)| file.checked_add(shmem)),
            swap: values.get("VmSwap").copied(),
        }
    }
    unsafe extern "C" {
        fn sysconf(name: i32) -> std::ffi::c_long;
    }
    pub(super) fn clock_ticks() -> Option<u64> {
        static TICKS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        *TICKS.get_or_init(|| {
            u64::try_from(unsafe { sysconf(2) })
                .ok()
                .filter(|value| *value > 0)
        })
    }
    pub(super) struct Reading {
        pub stat: Stat,
        pub memory: Memory,
        pub started_at: u64,
        pub executable: String,
    }
    fn bounded_text(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::other("procfs record exceeds limit"));
        }
        String::from_utf8(bytes).map_err(std::io::Error::other)
    }
    pub(super) fn lifetime(pid: u32) -> Option<u64> {
        parse_stat(&bounded_text(format!("/proc/{pid}/stat")).ok()?).map(|stat| stat.started)
    }
    pub(super) fn read(pid: u32) -> std::io::Result<Reading> {
        let root = std::path::PathBuf::from(format!("/proc/{pid}"));
        let stat = parse_stat(&bounded_text(root.join("stat"))?)
            .ok_or_else(|| std::io::Error::other("Invalid process stat"))?;
        let memory = bounded_text(root.join("status"))
            .map(|text| parse_memory(&text))
            .unwrap_or_default();
        let executable = std::fs::read_link(root.join("exe"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let after = process_lifetime(pid).ok_or_else(|| {
            std::io::Error::other("Process identity unavailable after reading details")
        })?;
        if stat.started != after {
            return Err(std::io::Error::other(
                "Process lifetime changed while reading details",
            ));
        }
        static BOOT_TIME: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let boot = *BOOT_TIME.get_or_init(sysinfo::System::boot_time);
        let started_at = boot
            .checked_add(
                stat.started
                    / clock_ticks()
                        .ok_or_else(|| std::io::Error::other("Clock tick frequency unavailable"))?,
            )
            .ok_or_else(|| std::io::Error::other("Process start time overflow"))?;
        Ok(Reading {
            stat,
            memory,
            started_at,
            executable,
        })
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn stat_parser_handles_parentheses_and_uses_only_process_counters() {
            let stat = parse_stat("42 (a tricky ) name) R 7 0 0 0 0 0 10 999 2 888 100 50 777 666 0 0 1 0 12345 200000 20\n").unwrap();
            assert_eq!(stat.name, "a tricky ) name");
            assert_eq!(stat.parent, 7);
            assert_eq!(stat.started, 12345);
            assert_eq!(stat.cpu, 150);
            assert_eq!((stat.minor, stat.major), (10, 2));
            assert!(parse_stat("42 (bad) R 1").is_none());
        }
        #[test]
        fn memory_fields_preserve_missing_and_use_kib_not_virtual_size() {
            let memory = parse_memory(
                "VmSize:\t999999 kB\nVmRSS:\t100 kB\nRssFile:\t20 kB\nRssShmem:\t3 kB\nVmSwap:\t0 kB\n",
            );
            assert_eq!(memory.resident, Some(102400));
            assert_eq!(memory.shared, Some(23552));
            assert_eq!(memory.swap, Some(0));
            let partial = parse_memory("VmRSS: 2 kB\nRssFile: 1 kB\nVmSwap: nope kB\n");
            assert_eq!(partial.resident, Some(2048));
            assert_eq!(partial.shared, None);
            assert_eq!(partial.swap, None);
            assert_eq!(parse_memory("VmRSS: 100 bytes").resident, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn native_linux_details_measure_own_resident_and_clear_unreadable_values() {
        let pid = std::process::id();
        let read = linux::read(pid).expect("own procfs details unavailable");
        assert!(read.memory.resident.is_some_and(|value| value > 0));
        assert!(read.memory.shared.is_some());
        assert!(read.memory.swap.is_some());
        let mut rows = vec![ProcessRow {
            pid,
            executable: "stale sysinfo path".into(),
            ..Default::default()
        }];
        let mut sampler = ProcessDetails::default();
        sampler.sample(&mut rows, 16, 0.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cpu_pct, None);
        assert!(rows[0].ram_bytes.is_some_and(|value| value > 0));
        assert_ne!(rows[0].executable, "stale sysinfo path");
        assert!(rows[0].lifetime.is_some());
        std::thread::sleep(std::time::Duration::from_millis(30));
        sampler.sample(&mut rows, 16, 1.0);
        assert!(rows[0].cpu_pct.is_some());
        assert!(rows[0].metrics.contains_key(&MetricKind::MinorFaults));
        assert!(!rows[0].metrics.contains_key(&MetricKind::PrivateCommit));
        eprintln!(
            "Native Linux resident={:?}, shared={:?}, swap={:?}, CPU={:?}, lifetime={:?}",
            rows[0].ram_bytes,
            rows[0].metrics.get(&MetricKind::ResidentShared),
            rows[0].metrics.get(&MetricKind::Swap),
            rows[0].cpu_pct,
            rows[0].lifetime
        );
        rows[0].pid = u32::MAX;
        sampler.sample(&mut rows, 16, 1.0);
        assert!(rows.is_empty(), "exited/unreadable PID kept stale metrics");
    }

    #[test]
    fn native_lifetime_lookup_reads_own_exact_token_and_rejects_missing_pid() {
        let token = process_lifetime(std::process::id()).expect("own lifetime unavailable");
        assert!(token > 0);
        assert_eq!(process_lifetime(std::process::id()), Some(token));
        assert_eq!(process_lifetime(u32::MAX), None);
    }

    #[test]
    fn cpu_and_fault_rates_require_contiguous_matching_lifetime_and_reset_independently() {
        let mut ledger = CpuFaultLedger::default();
        let mut row = ProcessRow {
            pid: 42,
            lifetime: Some(100),
            ..Default::default()
        };
        let counters = |cpu, minor, major| Counters {
            cpu: Some(cpu),
            minor: Some(minor),
            major: Some(major),
            total: None,
        };
        ledger.apply(&mut row, counters(100, 10, 2), 0.0, 100.0, 4);
        assert_eq!(row.cpu_pct, None);
        ledger.apply(&mut row, counters(200, 14, 4), 2.0, 100.0, 4);
        assert_eq!(row.cpu_pct, Some(12.5));
        assert_eq!(row.metrics[&MetricKind::MinorFaults], 2.0);
        assert_eq!(row.metrics[&MetricKind::MajorFaults], 1.0);
        ledger.apply(&mut row, counters(1, 16, 5), 1.0, 100.0, 4);
        assert_eq!(row.cpu_pct, None);
        assert_eq!(row.metrics[&MetricKind::MinorFaults], 2.0);
        row.lifetime = Some(101);
        ledger.apply(&mut row, counters(20, 100, 20), 1.0, 100.0, 4);
        assert_eq!(row.cpu_pct, None);
        assert!(!row.metrics.contains_key(&MetricKind::MinorFaults));
        ledger.apply(&mut row, counters(100, 110, 25), 0.0, 100.0, 4);
        assert_eq!(row.cpu_pct, None);
        ledger.apply(&mut row, counters(200, 120, 30), 1.0, 100.0, 4);
        assert_eq!(row.cpu_pct, Some(25.0));
        ledger.retain(&[]);
        assert!(ledger.previous.is_empty());
    }
    #[test]
    fn missing_cpu_is_unknown_while_available_fault_counter_remains_independent() {
        let mut ledger = CpuFaultLedger::default();
        let mut row = ProcessRow {
            pid: 42,
            lifetime: Some(100),
            ..Default::default()
        };
        ledger.apply(
            &mut row,
            Counters {
                total: Some(10),
                ..Default::default()
            },
            1.0,
            10_000_000.0,
            8,
        );
        ledger.apply(
            &mut row,
            Counters {
                total: Some(12),
                ..Default::default()
            },
            1.0,
            10_000_000.0,
            8,
        );
        assert_eq!(row.cpu_pct, None);
        assert_eq!(row.metrics[&MetricKind::PageFaults], 2.0);
        row.lifetime = None;
        ledger.apply(
            &mut row,
            Counters {
                total: Some(14),
                ..Default::default()
            },
            1.0,
            10_000_000.0,
            8,
        );
        assert!(!row.metrics.contains_key(&MetricKind::PageFaults));
    }
}
