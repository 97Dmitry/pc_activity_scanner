//! Shared, renderer-independent history contracts. Times are UTC Unix seconds.
use crate::metrics::MetricKind;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Duration-weighted measurements. Integral is value * observed seconds.
#[derive(Clone, Debug, Default)]
pub struct HistoricalMetric {
    pub average: f64,
    pub peak: f64,
    pub observed_secs: f64,
    pub integral: f64,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceHistory {
    pub id: String,
    pub name: String,
    pub metrics: BTreeMap<MetricKind, HistoricalMetric>,
}

pub const BUCKET_SECONDS: i64 = 30;
pub const MAX_COMPARISON_APPS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub retention_days: u32,
}
impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 1,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    pub request_id: u64,
    pub from: i64,
    pub to: i64,
    pub application_ids: Vec<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryPoint {
    pub metrics: BTreeMap<MetricKind, HistoricalMetric>,
    pub timestamp: i64,
    pub duration_secs: f64,
    pub cpu_avg: Option<f64>,
    pub cpu_observed_secs: f64,
    pub cpu_peak: Option<f64>,
    pub ram_avg: Option<f64>,
    pub ram_observed_secs: f64,
    pub ram_peak: Option<u64>,
    pub gpu_avg: Option<f64>,
    pub gpu_observed_secs: f64,
    pub network_observed_secs: f64,
    pub down_bps: Option<f64>,
    pub up_bps: Option<f64>,
    pub downloaded_bytes: Option<f64>,
    pub uploaded_bytes: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationSummary {
    pub metrics: BTreeMap<MetricKind, HistoricalMetric>,
    pub id: i64,
    pub name: String,
    pub executable: String,
    pub observed_secs: f64,
    pub cpu_avg: Option<f64>,
    pub cpu_observed_secs: f64,
    pub cpu_peak: Option<f64>,
    pub cpu_seconds: Option<f64>,
    pub ram_avg: Option<f64>,
    pub ram_observed_secs: f64,
    pub ram_peak: Option<u64>,
    pub gpu_avg: Option<f64>,
    pub gpu_observed_secs: f64,
    pub downloaded_bytes: Option<f64>,
    pub uploaded_bytes: Option<f64>,
    pub network_observed_secs: f64,
    pub last_seen: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ApplicationSeries {
    pub application: ApplicationSummary,
    pub points: Vec<HistoryPoint>,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryData {
    pub devices: Vec<DeviceHistory>,
    pub systems: Vec<HistoryPoint>,
    pub applications: Vec<ApplicationSummary>,
    pub series: Vec<ApplicationSeries>,
    pub earliest_timestamp: Option<i64>,
    pub latest_timestamp: Option<i64>,
    pub recorded_secs: f64,
    pub ram_total_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryStatus {
    pub ready: bool,
    pub enabled: bool,
    pub retention_days: u32,
    pub database_path: Option<PathBuf>,
    pub database_bytes: u64,
    pub dropped_samples: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryView {
    pub request_id: u64,
    /// Query outcome is independent from sticky recording/storage warnings.
    pub query_error: Option<String>,
    pub data: HistoryData,
    pub status: HistoryStatus,
}
