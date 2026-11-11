use crate::model::{ProcessRow, Snapshot};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Default)]
pub(super) struct Metric {
    pub seconds: f64,
    pub cpu_sum: f64,
    pub cpu_peak: f64,
    pub ram_sum: f64,
    pub ram_peak: u64,
    pub gpu_sum: f64,
    pub gpu_seconds: f64,
    pub down: f64,
    pub up: f64,
    pub network_seconds: f64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AppMetric {
    pub name: String,
    pub executable: String,
    pub last_seen: i64,
    pub metric: Metric,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Bucket {
    pub timestamp: i64,
    pub last_seen: i64,
    pub observed_until: f64,
    pub ram_total: u64,
    pub system: Metric,
    pub applications: HashMap<String, AppMetric>,
}

#[derive(Default)]
pub(super) struct Aggregator {
    buckets: BTreeMap<i64, Bucket>,
    baselines: HashMap<(u32, u64), (Option<u64>, Option<u64>)>,
    last_timestamp: Option<i64>,
    timeline_end: Option<f64>,
    high_water: Option<f64>,
}

impl Aggregator {
    pub fn with_high_water(high_water: Option<f64>) -> Self {
        Self {
            high_water,
            ..Self::default()
        }
    }

    /// Returns true when a saved UTC boundary makes this sample ambiguous.
    pub fn record(&mut self, sample: &Snapshot) -> bool {
        let seconds = sample.sample_seconds;
        let contiguous = seconds.is_finite()
            && seconds > 0.0
            && seconds <= 15.0
            && self.last_timestamp.is_some_and(|last| {
                sample.sampled_at >= last
                    && ((sample.sampled_at - last) as f64 - seconds).abs() <= 1.25
            });
        let start = self.timeline_end.unwrap_or(sample.sampled_at as f64);
        let end = start + seconds;
        let contiguous = contiguous && (end - sample.sampled_at as f64).abs() <= 1.25;
        self.timeline_end = Some(if contiguous {
            end
        } else {
            sample.sampled_at as f64
        });
        self.last_timestamp = Some(sample.sampled_at);
        // A wall-clock rollback must not re-cover an interval already committed
        // in this session or a previous one. Reject ambiguous crossing intervals
        // whole, but refresh their network baselines below.
        let overlapping = self.high_water.is_some_and(|watermark| {
            if contiguous {
                start < watermark
            } else {
                (sample.sampled_at as f64) < watermark
            }
        });
        let contiguous = contiguous && !overlapping;
        let previous = std::mem::take(&mut self.baselines);
        let mut groups: HashMap<String, Observation> = HashMap::new();
        for row in &sample.process_rows {
            let identity = (row.pid, row.started_at);
            self.baselines
                .insert(identity, (row.downloaded_bytes, row.uploaded_bytes));
            if !contiguous {
                continue;
            }
            let delta = previous.get(&identity).and_then(|(down, up)| {
                Some((
                    row.downloaded_bytes?.checked_sub((*down)?)? as f64,
                    row.uploaded_bytes?.checked_sub((*up)?)? as f64,
                ))
            });
            let group = groups
                .entry(application_key(row))
                .or_insert_with(|| Observation {
                    name: row.name.clone(),
                    executable: row.executable.clone(),
                    gpu: Some(0.0),
                    network: Some((0.0, 0.0)),
                    ..Default::default()
                });
            group.cpu += finite(row.cpu_pct as f64);
            group.ram = group.ram.saturating_add(row.ram_bytes);
            group.gpu = group
                .gpu
                .zip(row.gpu_pct)
                .map(|(sum, value)| sum + finite(value as f64));
            group.network = group
                .network
                .zip(delta)
                .map(|((d, u), (nd, nu))| (d + nd, u + nu));
        }
        if !contiguous {
            return overlapping;
        }
        self.high_water = Some(end);
        let system = Observation {
            cpu: finite(sample.cpu_pct as f64),
            ram: sample.ram_used,
            network: (sample.interface_count > 0).then_some((
                finite(sample.down_bps) * seconds,
                finite(sample.up_bps) * seconds,
            )),
            ..Default::default()
        };
        // Integer UTC stamps anchor the timeline; monotonic durations advance it.
        // Reconstructing every start as `UTC - duration` overlaps fractional samples.
        let mut start = start;
        while start < end {
            let timestamp = (start.floor() as i64).div_euclid(super::types::BUCKET_SECONDS)
                * super::types::BUCKET_SECONDS;
            let stop = end.min((timestamp + super::types::BUCKET_SECONDS) as f64);
            let duration = stop - start;
            let bucket = self.buckets.entry(timestamp).or_insert_with(|| Bucket {
                timestamp,
                ..Default::default()
            });
            bucket.last_seen = sample.sampled_at;
            bucket.observed_until = stop;
            bucket.ram_total = sample.ram_total;
            bucket.system.observe(&system, duration, seconds);
            for (key, value) in &groups {
                let app = bucket
                    .applications
                    .entry(key.clone())
                    .or_insert_with(|| AppMetric {
                        name: value.name.clone(),
                        executable: value.executable.clone(),
                        last_seen: 0,
                        metric: Metric::default(),
                    });
                app.last_seen = sample.sampled_at;
                app.metric.observe(value, duration, seconds);
            }
            start = stop;
        }
        false
    }
    pub fn take(&mut self) -> Vec<Bucket> {
        std::mem::take(&mut self.buckets).into_values().collect()
    }
    pub fn break_continuity(&mut self) {
        self.baselines.clear();
        self.last_timestamp = None;
        self.timeline_end = None;
    }
    pub fn reset(&mut self) {
        *self = Self::with_high_water(self.high_water);
    }
}

#[derive(Default)]
struct Observation {
    name: String,
    executable: String,
    cpu: f64,
    ram: u64,
    gpu: Option<f64>,
    network: Option<(f64, f64)>,
}
impl Metric {
    fn observe(&mut self, value: &Observation, seconds: f64, sample_seconds: f64) {
        self.seconds += seconds;
        self.cpu_sum += value.cpu * seconds;
        self.cpu_peak = self.cpu_peak.max(value.cpu);
        self.ram_sum += value.ram as f64 * seconds;
        self.ram_peak = self.ram_peak.max(value.ram);
        if let Some(gpu) = value.gpu {
            self.gpu_sum += gpu * seconds;
            self.gpu_seconds += seconds;
        }
        if let Some((down, up)) = value.network {
            self.down += down * seconds / sample_seconds;
            self.up += up * seconds / sample_seconds;
            self.network_seconds += seconds;
        }
    }
}
fn finite(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}
fn application_key(row: &ProcessRow) -> String {
    let (kind, value) = if row.executable.is_empty() {
        ("name", &row.name)
    } else {
        ("path", &row.executable)
    };
    #[cfg(target_os = "windows")]
    let value = value.to_lowercase();
    format!("{kind}:{value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(
        pid: u32,
        started_at: u64,
        path: &str,
        cpu: f32,
        ram: u64,
        total: Option<u64>,
    ) -> ProcessRow {
        ProcessRow {
            pid,
            started_at,
            name: "worker".into(),
            executable: path.into(),
            cpu_pct: cpu,
            ram_bytes: ram,
            downloaded_bytes: total,
            uploaded_bytes: total,
            ..Default::default()
        }
    }
    fn sample(at: i64, seconds: f64, rows: Vec<ProcessRow>) -> Snapshot {
        Snapshot {
            sampled_at: at,
            sample_seconds: seconds,
            process_rows: rows,
            cpu_pct: 50.0,
            ram_used: 1000,
            ram_total: 2000,
            interface_count: 1,
            ..Default::default()
        }
    }

    #[test]
    fn contiguous_samples_produce_duration_weighted_buckets() {
        let mut a = Aggregator::default();
        a.record(&sample(1, 0.0, vec![]));
        a.record(&sample(
            3,
            2.0,
            vec![
                process(1, 1, "/a", 20.0, 100, None),
                process(2, 1, "/a", 10.0, 50, None),
            ],
        ));
        a.record(&sample(9, 6.0, vec![process(1, 1, "/a", 10.0, 50, None)]));
        let buckets = a.take();
        assert_eq!(buckets.len(), 1);
        let m = &buckets[0].applications.values().next().unwrap().metric;
        assert_eq!(m.seconds, 8.0);
        assert_eq!(m.cpu_sum / m.seconds, 15.0);
        assert_eq!(m.cpu_peak, 30.0);
        assert_eq!(m.ram_sum / m.seconds, 75.0);
        assert_eq!(m.ram_peak, 150);
        assert_eq!(m.gpu_seconds, 0.0);
        assert_eq!(m.network_seconds, 0.0);
    }

    #[test]
    fn counters_are_deduplicated_and_pid_reuse_cannot_inherit_bytes() {
        let mut a = Aggregator::default();
        for (at, started, bytes) in [
            (1, 1, 100),
            (2, 1, 300),
            (3, 1, 300),
            (4, 1, 500),
            (5, 2, 900),
            (6, 2, 920),
        ] {
            a.record(&sample(
                at,
                if at == 1 { 0.0 } else { 1.0 },
                vec![process(1, started, "/a", 1.0, 1, Some(bytes))],
            ));
        }
        let buckets = a.take();
        let m = &buckets[0].applications.values().next().unwrap().metric;
        assert_eq!(m.down, 420.0);
        assert_eq!(m.network_seconds, 4.0);
    }

    #[test]
    fn unknown_reset_and_pause_counters_never_bridge_unobserved_intervals() {
        let mut a = Aggregator::default();
        for (at, seconds, bytes) in [
            (1, 0.0, Some(10)),
            (2, 1.0, None),
            (3, 1.0, Some(100)),
            (4, 1.0, Some(120)),
            (20, 0.0, Some(900)),
            (21, 1.0, Some(910)),
            (22, 1.0, Some(1)),
            (23, 1.0, Some(5)),
            (28, 1.0, Some(1000)),
            (29, 1.0, Some(1010)),
        ] {
            a.record(&sample(
                at,
                seconds,
                vec![process(1, 1, "/a", 1.0, 1, bytes)],
            ));
        }
        let buckets = a.take();
        let m = &buckets[0].applications.values().next().unwrap().metric;
        assert_eq!(m.down, 44.0);
        assert_eq!(m.network_seconds, 4.0);
        assert_eq!(m.seconds, 7.0);
    }

    #[test]
    fn split_bucket_weights_and_executable_identity_are_preserved() {
        let mut a = Aggregator::default();
        a.record(&sample(28, 0.0, vec![]));
        a.record(&sample(
            32,
            4.0,
            vec![
                process(1, 1, "/a", 10.0, 10, None),
                process(2, 1, "/b", 20.0, 20, None),
            ],
        ));
        let buckets = a.take();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].system.seconds, 2.0);
        assert_eq!(buckets[1].system.seconds, 2.0);
        assert_eq!(buckets[0].applications.len(), 2);
    }
    #[test]
    fn fractional_intervals_never_overlap_when_utc_timestamps_are_whole_seconds() {
        let mut a = Aggregator::default();
        a.record(&sample(0, 0.0, vec![]));
        for tick in 1..=100 {
            a.record(&sample((tick as f64 * 1.1).floor() as i64, 1.1, vec![]));
        }
        let buckets = a.take();
        assert!(
            buckets.iter().all(|b| b.system.seconds <= 30.00001),
            "overlapping intervals: {:?}",
            buckets.iter().map(|b| b.system.seconds).collect::<Vec<_>>()
        );
        assert!((buckets.iter().map(|b| b.system.seconds).sum::<f64>() - 110.0).abs() < 0.00001);
    }
    #[test]
    fn partially_unknown_instance_metrics_are_not_reported_as_complete_application_usage() {
        let mut a = Aggregator::default();
        let mut known = process(1, 1, "/a", 10.0, 10, Some(10));
        known.gpu_pct = Some(20.0);
        let unknown = process(2, 1, "/a", 10.0, 10, None);
        a.record(&sample(1, 0.0, vec![known.clone(), unknown.clone()]));
        known.downloaded_bytes = Some(20);
        known.uploaded_bytes = Some(20);
        a.record(&sample(2, 1.0, vec![known, unknown]));
        let buckets = a.take();
        let m = &buckets[0].applications.values().next().unwrap().metric;
        assert_eq!(m.gpu_seconds, 0.0);
        assert_eq!(m.network_seconds, 0.0);
        assert_eq!(m.cpu_sum, 20.0);
    }

    #[test]
    fn exited_application_keeps_its_own_last_seen_inside_a_later_system_bucket() {
        let mut a = Aggregator::default();
        a.record(&sample(1, 0.0, vec![]));
        a.record(&sample(2, 1.0, vec![process(1, 1, "/a", 10.0, 10, None)]));
        a.record(&sample(3, 1.0, vec![]));
        let buckets = a.take();
        assert_eq!(
            buckets[0].applications.values().next().unwrap().last_seen,
            2
        );
        assert_eq!(buckets[0].last_seen, 3);
    }

    #[test]
    fn capture_jitter_can_share_a_whole_second_without_discarding_real_duration() {
        let mut a = Aggregator::default();
        a.record(&sample(0, 0.0, vec![]));
        a.record(&sample(1, 1.01, vec![]));
        a.record(&sample(1, 0.98, vec![]));
        assert!((a.take()[0].system.seconds - 1.99).abs() < 0.00001);
    }
}
