//! Windows WDDM performance counters. Collection stays on the collector thread.
use crate::metrics::MetricKind;
use crate::model::ProcessRow;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Adapter {
    luid_high: u32,
    luid_low: u32,
    physical: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Instance {
    pid: u32,
    adapter: Adapter,
    engine: Option<u32>,
}

#[derive(Clone, Debug, Default)]
struct Usage {
    utilization: Option<f32>,
    dedicated: Option<f64>,
    shared: Option<f64>,
}

#[derive(Default)]
struct LifetimeGuard {
    previous: HashMap<u32, u64>,
}

#[cfg(target_os = "windows")]
#[derive(Default)]
pub struct WindowsGpuSampler {
    query: Option<crate::pdh::Query>,
    identities: LifetimeGuard,
}

fn parse_instance(name: &str, engine: bool) -> Option<Instance> {
    if name.len() > 256 {
        return None;
    }
    let name = name.to_ascii_lowercase();
    let (base, engine_id) = if engine {
        let (base, suffix) = name.split_once("_eng_")?;
        let (number, kind) = suffix.split_once("_engtype_")?;
        if kind.is_empty()
            || !kind
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        {
            return None;
        }
        (base, Some(number.parse::<u32>().ok()?))
    } else {
        (name.as_str(), None)
    };
    let fields: Vec<_> = base.split('_').collect();
    if fields.len() != 7 || fields[0] != "pid" || fields[2] != "luid" || fields[5] != "phys" {
        return None;
    }
    let pid = fields[1].parse::<u32>().ok().filter(|pid| *pid > 0)?;
    let hex = |value: &str| {
        let digits = value.strip_prefix("0x")?;
        if digits.is_empty() || digits.len() > 8 {
            return None;
        }
        u32::from_str_radix(digits, 16).ok()
    };
    Some(Instance {
        pid,
        adapter: Adapter {
            luid_high: hex(fields[3])?,
            luid_low: hex(fields[4])?,
            physical: fields[6].parse().ok()?,
        },
        engine: engine_id,
    })
}

fn aggregate(
    engines: &[(String, f64)],
    dedicated: Option<&[(String, f64)]>,
    shared: Option<&[(String, f64)]>,
) -> HashMap<u32, Usage> {
    let mut adapters: HashMap<u32, HashSet<Adapter>> = HashMap::new();
    let mut result: HashMap<u32, Usage> = HashMap::new();
    for (name, value) in engines {
        let Some(instance) = parse_instance(name, true) else {
            continue;
        };
        if !value.is_finite() || !(0.0..=100.0).contains(value) {
            continue;
        }
        adapters
            .entry(instance.pid)
            .or_default()
            .insert(instance.adapter);
        let usage = result.entry(instance.pid).or_default();
        usage.utilization = Some(usage.utilization.unwrap_or(0.0).max(*value as f32));
    }
    let mut memory: [HashMap<(u32, Adapter), f64>; 2] = Default::default();
    for (index, values) in [dedicated, shared].into_iter().enumerate() {
        for (name, value) in values.unwrap_or_default() {
            let Some(instance) = parse_instance(name, false) else {
                continue;
            };
            if !value.is_finite() || !(0.0..=u64::MAX as f64).contains(value) {
                continue;
            }
            adapters
                .entry(instance.pid)
                .or_default()
                .insert(instance.adapter);
            // PDH instance duplicates never represent a second physical allocation.
            memory[index]
                .entry((instance.pid, instance.adapter))
                .and_modify(|current| *current = current.max(*value))
                .or_insert(*value);
        }
    }
    for (pid, adapters) in adapters {
        let total = |values: &HashMap<(u32, Adapter), f64>| {
            adapters.iter().try_fold(0.0, |sum, adapter| {
                values.get(&(pid, *adapter)).map(|v| sum + v)
            })
        };
        let usage = result.entry(pid).or_default();
        usage.dedicated = total(&memory[0]);
        usage.shared = total(&memory[1]);
    }
    result
}
impl LifetimeGuard {
    fn apply(
        &mut self,
        rows: &mut [ProcessRow],
        seconds: f64,
        before: &HashMap<u32, u64>,
        after: &HashMap<u32, u64>,
        usage: &HashMap<u32, Usage>,
    ) {
        let previous = std::mem::take(&mut self.previous);
        let contiguous = seconds.is_finite() && seconds > 0.0 && seconds <= 15.0;
        for row in rows {
            row.gpu_pct = None;
            row.gpu_memory_bytes = None;
            row.metrics.remove(&MetricKind::GpuDedicated);
            row.metrics.remove(&MetricKind::GpuShared);
            let Some(current) = before.get(&row.pid).copied().filter(|current| {
                row.lifetime == Some(*current) && after.get(&row.pid) == Some(current)
            }) else {
                continue;
            };
            self.previous.insert(row.pid, current);
            if !contiguous || previous.get(&row.pid) != Some(&current) {
                continue;
            }
            if let Some(usage) = usage.get(&row.pid) {
                row.gpu_pct = usage.utilization;
                row.gpu_memory_bytes = usage.dedicated.map(|value| value as u64);
                if let Some(value) = usage.dedicated {
                    row.metrics.insert(MetricKind::GpuDedicated, value);
                }
                if let Some(value) = usage.shared {
                    row.metrics.insert(MetricKind::GpuShared, value);
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
const COUNTERS: [&str; 3] = [
    r"\GPU Engine(*)\Utilization Percentage",
    r"\GPU Process Memory(*)\Dedicated Usage",
    r"\GPU Process Memory(*)\Shared Usage",
];

#[cfg(target_os = "windows")]
impl WindowsGpuSampler {
    pub fn sample(&mut self, rows: &mut [ProcessRow], seconds: f64) -> String {
        fn identities(rows: &[ProcessRow]) -> HashMap<u32, u64> {
            rows.iter()
                .filter_map(|row| {
                    let created = crate::process_details::process_lifetime(row.pid)?;
                    let started = (created / 10_000_000).checked_sub(11_644_473_600)?;
                    (row.started_at == started && row.lifetime.is_none_or(|token| token == created))
                        .then_some((row.pid, created))
                })
                .collect()
        }
        // Check exact process identities around counter collection, independently
        // of disk access (some processes allow identity but deny I/O counters).
        let before = identities(rows);
        for row in rows.iter_mut() {
            if let Some(created) = before.get(&row.pid) {
                row.lifetime = Some(*created);
            }
        }
        let initialize = if self.query.is_none() {
            crate::pdh::Query::new(&COUNTERS).map(|query| self.query = Some(query))
        } else {
            Ok(())
        };
        let collect =
            initialize.and_then(|()| self.query.as_mut().expect("initialized query").collect());
        if let Err(error) = collect {
            self.query = None;
            self.identities
                .apply(rows, 0.0, &HashMap::new(), &HashMap::new(), &HashMap::new());
            return format!("Windows GPU: источник PDH недоступен: {error}");
        }
        let query = self.query.as_ref().expect("collected query");
        let engines = query.values(0);
        let dedicated = query.values(1);
        let shared = query.values(2);
        let usage = aggregate(
            engines.as_deref().unwrap_or_default(),
            dedicated.as_deref().ok(),
            shared.as_deref().ok(),
        );
        let after = identities(rows);
        self.identities
            .apply(rows, seconds, &before, &after, &usage);
        let measured = rows
            .iter()
            .filter(|row| {
                row.gpu_pct.is_some()
                    || row.metrics.contains_key(&MetricKind::GpuDedicated)
                    || row.metrics.contains_key(&MetricKind::GpuShared)
            })
            .count();
        let errors = [&engines, &dedicated, &shared]
            .into_iter()
            .filter_map(|value| value.as_ref().err())
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let detail = if errors.is_empty() {
            String::new()
        } else {
            format!("; {errors}")
        };
        format!(
            "Windows WDDM/PDH: метрики GPU у {measured}/{} процессов; GPU% — максимум доступных движков{detail}",
            rows.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn name(pid: u32, luid: u32, engine: Option<u32>) -> String {
        let base = format!("pid_{pid}_luid_0x00000000_0x{luid:08x}_phys_0");
        engine.map_or(base.clone(), |engine| {
            format!("{base}_eng_{engine}_engtype_3D")
        })
    }
    fn reading(pid: u32, luid: u32, engine: Option<u32>, value: f64) -> (String, f64) {
        (name(pid, luid, engine), value)
    }
    fn row(lifetime: u64) -> ProcessRow {
        ProcessRow {
            pid: 42,
            lifetime: Some(lifetime),
            ..Default::default()
        }
    }
    fn usage() -> HashMap<u32, Usage> {
        [(
            42,
            Usage {
                utilization: Some(8.0),
                dedicated: Some(1024.0),
                shared: Some(512.0),
            },
        )]
        .into()
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn native_wddm_counter_smoke() {
        let required = std::env::var_os("PC_ACTIVITY_REQUIRE_WINDOWS_GPU").is_some();
        let mut query = match crate::pdh::Query::new(&COUNTERS) {
            Ok(query) => query,
            Err(error) => {
                assert!(!required, "GPU counters required: {error}");
                eprintln!("GPU source unavailable: {error}");
                return;
            }
        };
        if let Err(error) = query.collect() {
            assert!(!required, "GPU counters required: {error}");
            eprintln!("GPU source unavailable: {error}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        query.collect().unwrap();
        let engines = query.values(0).unwrap_or_default();
        let dedicated = query.values(1).ok();
        let shared = query.values(2).ok();
        let usage = aggregate(&engines, dedicated.as_deref(), shared.as_deref());
        let adapters: HashSet<_> = engines
            .iter()
            .filter_map(|(name, _)| parse_instance(name, true).map(|instance| instance.adapter))
            .collect();
        eprintln!(
            "Native WDDM source: engines={}, dedicated={}, shared={}, adapters={}, PIDs={}",
            engines.len(),
            dedicated.as_ref().map_or(0, Vec::len),
            shared.as_ref().map_or(0, Vec::len),
            adapters.len(),
            usage.len()
        );
        assert!(
            !required || !usage.is_empty(),
            "GPU source required but has no valid values"
        );
        let mut rows: Vec<_> = usage
            .keys()
            .filter_map(|pid| {
                let created = crate::network::process_creation_time(*pid)?;
                Some(ProcessRow {
                    pid: *pid,
                    lifetime: Some(created),
                    started_at: created / 10_000_000 - 11_644_473_600,
                    ..Default::default()
                })
            })
            .take(32)
            .collect();
        let mut sampler = WindowsGpuSampler::default();
        let first = sampler.sample(&mut rows, 0.0);
        assert!(
            rows.iter()
                .all(|row| row.gpu_pct.is_none() && row.gpu_memory_bytes.is_none())
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
        let second = sampler.sample(&mut rows, 1.0);
        let measured = rows
            .iter()
            .filter(|row| row.gpu_pct.is_some() || row.gpu_memory_bytes.is_some())
            .count();
        eprintln!("{first}\n{second}\nNative lifetime-verified GPU rows={measured}");
        assert!(
            !required || measured > 0,
            "No lifetime-verified GPU process received metrics"
        );
        assert!(rows.iter().all(|row| {
            row.gpu_pct
                .is_none_or(|value| (0.0..=100.0).contains(&value))
        }));
    }

    #[test]
    fn parses_only_complete_pid_adapter_and_engine_instances() {
        let a = parse_instance(&name(42, 0xABC, Some(3)), true).unwrap();
        assert_eq!(a.pid, 42);
        assert_eq!(a.adapter.luid_low, 0xABC);
        assert_eq!(a.engine, Some(3));
        assert!(parse_instance(&name(42, 0xABC, None), false).is_some());
        for bad in [
            "_Total",
            "pid_42",
            "pid_42_luid_bad_phys_0",
            "pid_42_luid_0x0_0x1_phys_0_eng_x_engtype_3D",
            "pid_0_luid_0x0_0x1_phys_0",
            "pid_42_luid_0x0_0x1_phys_0_extra",
        ] {
            assert!(parse_instance(bad, false).is_none(), "accepted {bad}");
        }
    }
    #[test]
    fn utilization_is_busiest_engine_across_adapters_and_memory_is_deduplicated() {
        let engines = [
            reading(42, 1, Some(0), 60.0),
            reading(42, 1, Some(1), 70.0),
            reading(42, 2, Some(0), 40.0),
            reading(42, 1, Some(1), 70.0),
        ];
        let dedicated = [
            reading(42, 1, None, 1024.0),
            reading(42, 1, None, 1024.0),
            reading(42, 2, None, 2048.0),
        ];
        let shared = [reading(42, 1, None, 512.0), reading(42, 2, None, 256.0)];
        let result = aggregate(&engines, Some(&dedicated), Some(&shared));
        assert_eq!(result[&42].utilization, Some(70.0));
        assert_eq!(result[&42].dedicated, Some(3072.0));
        assert_eq!(result[&42].shared, Some(768.0));
    }
    #[test]
    fn partial_memory_and_invalid_values_remain_unknown() {
        let engines = [
            reading(42, 1, Some(0), f64::NAN),
            reading(42, 2, Some(0), 105.0),
            reading(99, 1, Some(0), -1.0),
        ];
        let dedicated = [reading(42, 1, None, 1024.0)];
        let shared = [reading(42, 1, None, 0.0), reading(42, 2, None, 256.0)];
        let result = aggregate(&engines, Some(&dedicated), Some(&shared));
        assert_eq!(result[&42].utilization, None);
        assert_eq!(result[&42].dedicated, None);
        assert_eq!(result[&42].shared, Some(256.0));
        assert!(!result.contains_key(&99));
        let failed = aggregate(&[], None, None);
        assert!(failed.is_empty());
    }
    #[test]
    fn lifetime_guard_rejects_first_reuse_exit_pause_and_inflight_changes() {
        let mut guard = LifetimeGuard::default();
        let mut rows = [row(100)];
        let ids = [(42, 100)].into();
        guard.apply(&mut rows, 0.0, &ids, &ids, &usage());
        assert_eq!(rows[0].gpu_pct, None);
        guard.apply(&mut rows, 1.0, &ids, &ids, &usage());
        assert_eq!(rows[0].gpu_pct, Some(8.0));
        assert_eq!(rows[0].metrics[&MetricKind::GpuDedicated], 1024.0);
        rows[0] = row(200);
        let next = [(42, 200)].into();
        guard.apply(&mut rows, 1.0, &next, &next, &usage());
        assert_eq!(rows[0].gpu_pct, None);
        guard.apply(&mut rows, 1.0, &next, &next, &usage());
        assert_eq!(rows[0].gpu_pct, Some(8.0));
        guard.apply(&mut rows, 0.0, &next, &next, &usage());
        assert_eq!(rows[0].gpu_pct, None);
        guard.apply(&mut rows, 1.0, &next, &HashMap::new(), &usage());
        assert_eq!(rows[0].gpu_pct, None);
        guard.apply(&mut rows, 1.0, &next, &next, &usage());
        assert_eq!(rows[0].gpu_pct, None);
        guard.apply(&mut rows, 1.0, &next, &next, &usage());
        assert_eq!(rows[0].gpu_pct, Some(8.0));
        guard.apply(&mut rows, 16.0, &next, &next, &usage());
        assert_eq!(rows[0].gpu_pct, None);
        guard.apply(&mut [], 1.0, &next, &next, &usage());
        assert!(guard.previous.is_empty());
    }
    #[test]
    fn unreadable_or_mismatched_lifetime_clears_stale_gpu_metrics() {
        let mut guard = LifetimeGuard::default();
        let mut rows = [row(100)];
        rows[0].gpu_pct = Some(50.0);
        rows[0].gpu_memory_bytes = Some(100);
        rows[0].metrics.insert(MetricKind::GpuDedicated, 100.0);
        let ids = [(42, 200)].into();
        guard.apply(&mut rows, 1.0, &ids, &ids, &usage());
        assert_eq!(rows[0].gpu_pct, None);
        assert_eq!(rows[0].gpu_memory_bytes, None);
        assert!(!rows[0].metrics.contains_key(&MetricKind::GpuDedicated));
    }
}
