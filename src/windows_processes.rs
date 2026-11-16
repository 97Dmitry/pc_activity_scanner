//! Native per-process accounting, including minimal VMs and protected processes.
use crate::model::ProcessRow;
use crate::process_details::{Counters, CpuFaultLedger};
use std::time::Instant;

#[derive(Clone, Debug)]
struct ProcessReading {
    pid: u32,
    created_100ns: u64,
    cpu_100ns: Option<u64>,
    ram_bytes: u64,
    private_commit: u64,
    faults: u64,
    parent_pid: Option<u32>,
    name: String,
    executable: String,
}
#[derive(Default)]
pub struct ProcessSampler {
    ledger: CpuFaultLedger,
    sampled_at: Option<Instant>,
}
impl ProcessSampler {
    fn apply(
        &mut self,
        rows: &mut Vec<ProcessRow>,
        readings: Vec<ProcessReading>,
        now: Instant,
        cores: usize,
        seconds: f64,
    ) -> String {
        let elapsed = self
            .sampled_at
            .map_or(0.0, |at| now.duration_since(at).as_secs_f64());
        self.sampled_at = Some(now);
        let elapsed = if seconds.is_finite() && seconds > 0.0 && seconds <= 15.0 {
            elapsed
        } else {
            0.0
        };
        let mut previous: std::collections::HashMap<_, _> = std::mem::take(rows)
            .into_iter()
            .map(|row| (row.pid, row))
            .collect();
        for reading in readings {
            let started_at = reading.created_100ns / 10_000_000 - 11_644_473_600;
            // Only keep independently sampled metrics when metadata names this
            // same lifetime. Refresh the image path even if sysinfo cached one.
            let mut row = previous
                .remove(&reading.pid)
                .filter(|row| {
                    row.started_at == started_at
                        && row
                            .lifetime
                            .is_none_or(|token| token == reading.created_100ns)
                })
                .unwrap_or_default();
            row.pid = reading.pid;
            row.started_at = started_at;
            row.lifetime = Some(reading.created_100ns);
            row.parent_pid = reading.parent_pid;
            row.parent = None;
            row.count = 1;
            row.name = if reading.name.eq_ignore_ascii_case("vmmemWSL") {
                "WSL2 (vmmemWSL)".into()
            } else {
                reading.name
            };
            row.executable = reading.executable;
            row.ram_bytes = Some(reading.ram_bytes);
            for key in [
                crate::metrics::MetricKind::ResidentShared,
                crate::metrics::MetricKind::Swap,
            ] {
                row.metrics.remove(&key);
            }
            row.metrics.insert(
                crate::metrics::MetricKind::PrivateCommit,
                reading.private_commit as f64,
            );
            self.ledger.apply(
                &mut row,
                Counters {
                    cpu: reading.cpu_100ns,
                    total: Some(reading.faults),
                    ..Default::default()
                },
                elapsed,
                10_000_000.0,
                cores,
            );
            rows.push(row);
        }
        self.ledger.retain(rows);
        let warming = rows.iter().filter(|row| row.cpu_pct.is_none()).count();
        format!(
            "Windows native snapshot: working set/private commit; CPU недоступен или ожидает второй замер у {warming}/{} процессов",
            rows.len()
        )
    }
    #[cfg(target_os = "windows")]
    pub fn sample(&mut self, rows: &mut Vec<ProcessRow>, cores: usize, seconds: f64) -> String {
        match native::read() {
            Ok(readings) => self.apply(rows, readings, Instant::now(), cores, seconds),
            Err(error) => {
                *self = Self::default();
                for row in rows {
                    row.cpu_pct = None;
                    row.ram_bytes = None;
                    row.lifetime = None;
                    row.parent = None;
                    row.parent_pid = None;
                    row.executable.clear();
                    for key in [
                        crate::metrics::MetricKind::PrivateCommit,
                        crate::metrics::MetricKind::ResidentShared,
                        crate::metrics::MetricKind::Swap,
                        crate::metrics::MetricKind::MinorFaults,
                        crate::metrics::MetricKind::MajorFaults,
                        crate::metrics::MetricKind::PageFaults,
                    ] {
                        row.metrics.remove(&key);
                    }
                }
                format!("Windows native process metrics unavailable: {error}")
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricKind;
    const CREATED: u64 = 133_000_000_000_000_000;
    fn reading(pid: u32, created: u64, cpu: u64, name: &str) -> ProcessReading {
        ProcessReading {
            pid,
            created_100ns: created,
            cpu_100ns: Some(cpu),
            ram_bytes: 10 * 1024 * 1024 * 1024,
            private_commit: 20 * 1024 * 1024 * 1024,
            faults: 10,
            parent_pid: Some(1),
            name: name.into(),
            executable: String::new(),
        }
    }
    #[test]
    fn authoritative_rows_keep_vm_ram_during_cpu_warmup_and_replace_sysinfo_once() {
        let mut sampler = ProcessSampler::default();
        let now = Instant::now();
        let mut rows = vec![ProcessRow {
            pid: 7,
            name: "old sysinfo VM".into(),
            executable: "stale.exe".into(),
            ..Default::default()
        }];
        sampler.apply(
            &mut rows,
            vec![
                reading(7, CREATED, 10_000_000, "vmmemWSL"),
                reading(8, CREATED, 1, "editor.exe"),
            ],
            now,
            8,
            0.0,
        );
        assert_eq!(rows.len(), 2);
        let vm = rows.iter().find(|r| r.pid == 7).unwrap();
        assert_eq!(vm.name, "WSL2 (vmmemWSL)");
        assert_eq!(vm.cpu_pct, None);
        assert_eq!(vm.ram_bytes, Some(10 * 1024 * 1024 * 1024));
        assert_eq!(
            vm.metrics[&MetricKind::PrivateCommit],
            (20u64 * 1024 * 1024 * 1024) as f64
        );
        assert_eq!(vm.lifetime, Some(CREATED));
        assert_eq!(vm.parent_pid, Some(1));
        assert!(vm.executable.is_empty());
        sampler.apply(
            &mut rows,
            vec![reading(7, CREATED, 30_000_000, "vmmemWSL")],
            now + std::time::Duration::from_secs(2),
            8,
            2.0,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cpu_pct, Some(12.5));
    }
    #[test]
    fn reuse_counter_reset_pause_and_exit_never_inherit_cpu() {
        let mut sampler = ProcessSampler::default();
        let now = Instant::now();
        let mut rows = vec![];
        sampler.apply(
            &mut rows,
            vec![reading(7, CREATED, 100, "vmmemWSL")],
            now,
            8,
            1.0,
        );
        sampler.apply(
            &mut rows,
            vec![reading(7, CREATED + 1, 200, "vmmemWSL")],
            now + std::time::Duration::from_secs(1),
            8,
            1.0,
        );
        assert_eq!(rows[0].cpu_pct, None);
        sampler.apply(
            &mut rows,
            vec![reading(7, CREATED + 1, 1, "vmmemWSL")],
            now + std::time::Duration::from_secs(2),
            8,
            1.0,
        );
        assert_eq!(rows[0].cpu_pct, None);
        sampler.apply(
            &mut rows,
            vec![reading(7, CREATED + 1, 100, "vmmemWSL")],
            now + std::time::Duration::from_secs(3),
            8,
            0.0,
        );
        assert_eq!(rows[0].cpu_pct, None);
        sampler.apply(
            &mut rows,
            vec![],
            now + std::time::Duration::from_secs(4),
            8,
            1.0,
        );
        assert!(rows.is_empty());
    }
    #[test]
    fn other_vms_are_not_mislabelled_as_wsl_and_unknown_native_cpu_is_not_zero() {
        let mut sampler = ProcessSampler::default();
        let mut rows = vec![];
        let mut value = reading(9, CREATED, 100, "vmmemCmZygote");
        value.cpu_100ns = None;
        sampler.apply(&mut rows, vec![value], Instant::now(), 8, 1.0);
        assert_eq!(rows[0].name, "vmmemCmZygote");
        assert_eq!(rows[0].cpu_pct, None);
        assert!(rows[0].ram_bytes.is_some());
    }
}
#[cfg(target_os = "windows")]
mod native {
    use super::*;
    use ntapi::ntexapi::{
        NtQuerySystemInformation, SYSTEM_PROCESS_INFORMATION, SystemProcessInformation,
    };
    const LIMIT: usize = 64 * 1024 * 1024;
    const MISMATCH: i32 = 0xC0000004_u32 as i32;

    pub(super) fn read() -> Result<Vec<ProcessReading>, String> {
        let mut size: usize = 1024 * 1024;
        for _ in 0..8 {
            // u64 alignment also satisfies SYSTEM_PROCESS_INFORMATION on x64.
            let mut allocation = vec![0u64; size.div_ceil(8)];
            let mut returned = 0u32;
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    allocation.as_mut_ptr().cast(),
                    (allocation.len() * 8) as u32,
                    &mut returned,
                )
            };
            if status == MISMATCH {
                size = (returned as usize).max(size * 2);
                if size > LIMIT {
                    return Err("Windows process snapshot exceeds size limit".into());
                }
                continue;
            }
            if status < 0 {
                return Err(format!("NtQuerySystemInformation: 0x{:08X}", status as u32));
            }
            if returned as usize > allocation.len() * 8 || returned == 0 {
                return Err("Invalid Windows process snapshot length".into());
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(allocation.as_ptr().cast::<u8>(), returned as usize)
            };
            let mut rows = parse(bytes)?;
            for row in &mut rows {
                row.executable = image_path(row.pid, row.created_100ns).unwrap_or_default();
            }
            return Ok(rows);
        }
        Err("Windows process snapshot changed too often".into())
    }

    fn image_path(pid: u32, expected: u64) -> Option<String> {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let result = (|| {
            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
                == 0
            {
                return None;
            }
            let token =
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            if token != expected {
                return None;
            }
            let mut path = vec![0u16; 32768];
            let mut len = path.len() as u32;
            if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) } == 0
                || len as usize > path.len()
            {
                return None;
            }
            Some(String::from_utf16_lossy(&path[..len as usize]))
        })();
        unsafe {
            CloseHandle(handle);
        }
        result
    }
    fn parse(data: &[u8]) -> Result<Vec<ProcessReading>, String> {
        let header_size = std::mem::offset_of!(SYSTEM_PROCESS_INFORMATION, Threads);
        let mut offset = 0usize;
        let mut result = Vec::new();
        let mut pids = std::collections::HashSet::new();
        loop {
            if data.len().saturating_sub(offset) < header_size {
                return Err("Truncated process entry".into());
            }
            let p = unsafe {
                data.as_ptr()
                    .add(offset)
                    .cast::<SYSTEM_PROCESS_INFORMATION>()
            };
            // Read only fixed fields: VM entries can have no trailing threads.
            let (next, image, pid, parent, created, user, kernel, ram, private, faults) = unsafe {
                (
                    std::ptr::addr_of!((*p).NextEntryOffset).read_unaligned() as usize,
                    std::ptr::addr_of!((*p).ImageName).read_unaligned(),
                    std::ptr::addr_of!((*p).UniqueProcessId).read_unaligned() as usize,
                    std::ptr::addr_of!((*p).InheritedFromUniqueProcessId).read_unaligned() as usize,
                    *std::ptr::addr_of!((*p).CreateTime)
                        .read_unaligned()
                        .QuadPart(),
                    *std::ptr::addr_of!((*p).UserTime)
                        .read_unaligned()
                        .QuadPart(),
                    *std::ptr::addr_of!((*p).KernelTime)
                        .read_unaligned()
                        .QuadPart(),
                    std::ptr::addr_of!((*p).WorkingSetSize).read_unaligned() as u64,
                    std::ptr::addr_of!((*p).PrivatePageCount).read_unaligned() as u64,
                    std::ptr::addr_of!((*p).PageFaultCount).read_unaligned() as u64,
                )
            };
            if next != 0 && (next < header_size || next > data.len().saturating_sub(offset)) {
                return Err("Invalid process entry offset".into());
            }
            if pid > u32::MAX as usize || parent > u32::MAX as usize {
                return Err("Invalid process ID".into());
            }
            let length = image.Length as usize;
            if !length.is_multiple_of(2) {
                return Err("Invalid Windows process name length".into());
            }
            let name = if length == 0 {
                String::new()
            } else {
                let start = (image.Buffer as usize)
                    .checked_sub(data.as_ptr() as usize)
                    .ok_or("Invalid Windows process name pointer")?;
                let end = start
                    .checked_add(length)
                    .filter(|end| *end <= data.len())
                    .ok_or("Windows process name outside snapshot")?;
                let utf16 = data[start..end]
                    .chunks_exact(2)
                    .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
                    .collect::<Vec<_>>();
                String::from_utf16_lossy(&utf16)
            };
            // Idle is not a workload process. Special records without a valid
            // creation identity cannot participate in per-process accounting.
            if pid != 0 && created >= 116_444_736_000_000_000 {
                if !pids.insert(pid) {
                    return Err("Duplicate PID in Windows process snapshot".into());
                }
                result.push(ProcessReading {
                    pid: pid as u32,
                    created_100ns: created as u64,
                    cpu_100ns: u64::try_from(user)
                        .ok()
                        .zip(u64::try_from(kernel).ok())
                        .and_then(|(user, kernel)| user.checked_add(kernel)),
                    ram_bytes: ram,
                    private_commit: private,
                    faults,
                    parent_pid: (parent > 0).then_some(parent as u32),
                    name: if name.is_empty() {
                        format!("PID {pid}")
                    } else {
                        name
                    },
                    executable: String::new(),
                });
            }
            if next == 0 {
                return Ok(result);
            }
            offset += next;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn malformed_native_records_are_rejected() {
            assert!(parse(&[]).is_err());
            let mut data = vec![0u8; std::mem::offset_of!(SYSTEM_PROCESS_INFORMATION, Threads)];
            data[..4].copy_from_slice(&1u32.to_ne_bytes());
            assert!(parse(&data).is_err());
            data[..4].fill(0);
            let name = std::mem::offset_of!(SYSTEM_PROCESS_INFORMATION, ImageName);
            data[name..name + 2].copy_from_slice(&2u16.to_ne_bytes());
            assert!(parse(&data).is_err());
        }
        #[test]
        fn native_snapshot_matches_own_psapi_memory_and_keeps_vm_ram_on_warmup() {
            use windows_sys::Win32::System::ProcessStatus::{
                K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
            };
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let mut psapi = PROCESS_MEMORY_COUNTERS_EX::default();
            psapi.cb = std::mem::size_of_val(&psapi) as u32;
            assert_ne!(
                unsafe {
                    K32GetProcessMemoryInfo(
                        GetCurrentProcess(),
                        (&mut psapi as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                        psapi.cb,
                    )
                },
                0
            );
            let rows = read().expect("native process snapshot unavailable");
            let own = rows
                .iter()
                .find(|row| row.pid == std::process::id())
                .expect("own PID missing");
            assert!(own.ram_bytes > 0);
            assert!(own.private_commit > 0);
            // The snapshot allocates a bounded buffer while sampling. A 4-MiB
            // allowance covers that temporary buffer plus normal test machinery,
            // while detecting page-vs-byte errors by orders of magnitude.
            assert!(own.ram_bytes.abs_diff(psapi.WorkingSetSize as u64) < 4 * 1024 * 1024);
            assert!(own.private_commit.abs_diff(psapi.PrivateUsage as u64) < 4 * 1024 * 1024);
            assert!(!own.executable.is_empty());
            eprintln!(
                "Native own memory: working_set={} PSAPI={}, private_commit={} PSAPI={}, faults={}, rows={}",
                own.ram_bytes,
                psapi.WorkingSetSize,
                own.private_commit,
                psapi.PrivateUsage,
                own.faults,
                rows.len()
            );
            let mut sampler = ProcessSampler::default();
            let mut output = Vec::new();
            sampler.sample(&mut output, 16, 0.0);
            assert!(
                output
                    .iter()
                    .all(|row| row.cpu_pct.is_none() && row.ram_bytes.is_some())
            );
            let vm: Vec<_> = output
                .iter()
                .filter(|row| row.name.contains("vmmem"))
                .collect();
            eprintln!(
                "Native VM rows with warm CPU: {:?}",
                vm.iter()
                    .map(|row| (&row.name, row.ram_bytes, row.cpu_pct))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(std::time::Duration::from_millis(30));
            sampler.sample(&mut output, 16, 1.0);
            let own = output
                .iter()
                .find(|row| row.pid == std::process::id())
                .unwrap();
            assert!(own.cpu_pct.is_some());
            assert!(
                own.metrics
                    .contains_key(&crate::metrics::MetricKind::PageFaults)
            );
            assert!(!own.metrics.contains_key(&crate::metrics::MetricKind::Swap));
        }
        #[test]
        fn native_query_runs_without_opening_vm_handles() {
            let rows = read().expect("SystemProcessInformation failed");
            for row in rows {
                assert!(!row.name.is_empty());
                assert!(row.created_100ns > 0);
            }
        }
    }
}
