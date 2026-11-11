use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default)]
pub struct GpuUsage {
    pub utilization_pct: Option<f32>,
    pub memory_bytes: Option<u64>,
}

impl GpuUsage {
    pub fn combine(&mut self, other: Self) {
        // NVIDIA and DRM can report the same device. Never sum twice.
        self.utilization_pct = match (self.utilization_pct, other.utilization_pct) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, value) | (value, None) => value,
        };
        self.memory_bytes = match (self.memory_bytes, other.memory_bytes) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, value) | (value, None) => value,
        };
    }
}

/// NVIDIA pmon output is a Linux-only fallback for the proprietary driver.
pub fn parse_nvidia_pmon(output: &str) -> HashMap<u32, GpuUsage> {
    let mut result: HashMap<u32, GpuUsage> = HashMap::new();
    let mut fb_index = None;
    for line in output.lines() {
        let cols = line.split_whitespace().collect::<Vec<_>>();
        // NVIDIA pmon varies its columns between driver versions. Read the
        // framebuffer column by header name instead of assuming an offset.
        if cols.first() == Some(&"#") && cols.get(1) == Some(&"gpu") {
            fb_index = cols[1..]
                .iter()
                .position(|value| value.eq_ignore_ascii_case("fb"));
            continue;
        }
        if cols.len() < 4 || cols[0].starts_with('#') {
            continue;
        }
        let Ok(pid) = cols[1].parse::<u32>() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let gpu_pct = cols[3]
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 100.0));
        let gpu_memory_bytes = fb_index
            .and_then(|index| cols.get(index))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|mib| mib.saturating_mul(1024 * 1024));
        let sample = GpuUsage {
            utilization_pct: gpu_pct,
            memory_bytes: gpu_memory_bytes,
        };
        result.entry(pid).or_default().combine(sample);
    }
    result
}

#[cfg(target_os = "linux")]
pub mod drm {
    use super::GpuUsage;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::time::Instant;

    #[derive(Debug, Clone, Default)]
    pub struct DrmClient {
        pub identifier: Option<String>,
        pub gpu_busy_ns: u64,
        pub memory_bytes: Option<u64>,
    }

    /// Parse Linux's documented DRM fdinfo keys (not process-wide estimates).
    pub fn parse_fdinfo(text: &str) -> Option<DrmClient> {
        let mut client_id = None;
        let mut pdev = String::new();
        let mut busy_ns = 0u64;
        let mut memory = None;
        let mut found_engine = false;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if key == "drm-client-id" {
                client_id = Some(value.to_string());
            } else if key == "drm-pdev" {
                pdev = value.to_string();
            } else if key.starts_with("drm-engine-") {
                if let Some(nanos) = value
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    busy_ns = busy_ns.saturating_add(nanos);
                    found_engine = true;
                }
            } else if key == "drm-memory-vram" {
                memory = value
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|kib| kib.saturating_mul(1024));
            }
        }
        if !found_engine && memory.is_none() {
            return None;
        }
        Some(DrmClient {
            identifier: client_id.map(|id| format!("{pdev}/{id}")),
            gpu_busy_ns: busy_ns,
            memory_bytes: memory,
        })
    }

    pub struct DrmSampler {
        previous: HashMap<u32, u64>,
        last: Instant,
    }

    impl DrmSampler {
        pub fn new() -> Self {
            Self {
                previous: HashMap::new(),
                last: Instant::now(),
            }
        }

        pub fn sample(&mut self, pids: &[u32]) -> HashMap<u32, GpuUsage> {
            let elapsed_ns = self.last.elapsed().as_nanos().max(1) as f64;
            let mut next = HashMap::new();
            let mut result = HashMap::new();
            for &pid in pids {
                let dir = format!("/proc/{pid}/fdinfo");
                let Ok(entries) = fs::read_dir(dir) else {
                    continue;
                };
                let mut unique = HashSet::new();
                let mut sum_busy_ns = 0u64;
                let mut sum_memory_bytes = 0u64;
                let mut have_busy = false;
                let mut have_memory = false;
                for entry in entries.flatten() {
                    let Ok(text) = fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    let Some(client) = parse_fdinfo(&text) else {
                        continue;
                    };
                    if let Some(identifier) = client.identifier {
                        if !unique.insert(identifier) {
                            continue;
                        }
                    } else {
                        // Missing client id: ignore to avoid double counting duplicated FDs.
                        continue;
                    }
                    sum_busy_ns = sum_busy_ns.saturating_add(client.gpu_busy_ns);
                    have_busy = true;
                    if let Some(mem) = client.memory_bytes {
                        sum_memory_bytes = sum_memory_bytes.saturating_add(mem);
                        have_memory = true;
                    }
                }
                if have_busy || have_memory {
                    next.insert(pid, sum_busy_ns);
                    let percent = if have_busy {
                        self.previous.get(&pid).and_then(|prev| {
                            sum_busy_ns.checked_sub(*prev).map(|delta| {
                                (delta as f64 / elapsed_ns * 100.0).clamp(0.0, 100.0) as f32
                            })
                        })
                    } else {
                        None
                    };
                    result.insert(
                        pid,
                        GpuUsage {
                            utilization_pct: percent,
                            memory_bytes: have_memory.then_some(sum_memory_bytes),
                        },
                    );
                }
            }
            self.previous = next;
            self.last = Instant::now();
            result
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_drm_client_fields() {
            let source = "drm-client-id:\t9\ndrm-pdev:\t0000:01:00.0\n\
                          drm-engine-gfx:\t1200000 ns\ndrm-engine-compute:\t250000 ns\n\
                          drm-memory-vram:\t1048576 KiB\n";
            let sample = parse_fdinfo(source).unwrap();
            assert_eq!(sample.identifier.as_deref(), Some("0000:01:00.0/9"));
            assert_eq!(sample.gpu_busy_ns, 1_450_000);
            assert_eq!(sample.memory_bytes, Some(1_073_741_824));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_nvidia_processes() {
        let output = "# gpu pid type sm mem enc dec fb command\n 0 1431 G 24 10 0 0 532 firefox\n";
        let sample = parse_nvidia_pmon(output);
        assert_eq!(sample[&1431].utilization_pct, Some(24.0));
        assert_eq!(sample[&1431].memory_bytes, Some(532 * 1024 * 1024));
    }

    #[test]
    fn parses_extended_nvidia_pmon_header_without_misreading_jpeg_as_vram() {
        let output = "# gpu pid type sm mem enc dec jpg ofa fb command\n 0 1431 G 24 10 0 0 1 0 532 firefox\n";
        assert_eq!(
            parse_nvidia_pmon(output)[&1431].memory_bytes,
            Some(532 * 1024 * 1024)
        );
    }

    #[test]
    fn unknown_is_not_zero() {
        let mut usage = GpuUsage::default();
        usage.combine(GpuUsage::default());
        assert_eq!(usage.utilization_pct, None);
    }
}
