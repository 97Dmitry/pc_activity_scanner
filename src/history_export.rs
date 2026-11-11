//! Historical CSV reports. Called on a one-shot worker, never on the GUI thread.
use crate::history::types::{HistoryPoint, HistoryQuery, HistoryView};
use chrono::{DateTime, Local};
use directories::UserDirs;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn export_history(query: &HistoryQuery, view: &HistoryView) -> Result<PathBuf, String> {
    if query.request_id != view.request_id {
        return Err("Ответ истории не соответствует выбранному периоду".into());
    }
    let root = UserDirs::new()
        .and_then(|user| user.download_dir().map(ToOwned::to_owned))
        .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?);
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    export_to(&root, query, view, stamp)
}

fn export_to(
    root: &Path,
    query: &HistoryQuery,
    view: &HistoryView,
    stamp: u128,
) -> Result<PathBuf, String> {
    for suffix in 0..100 {
        let dir = root.join(format!("pc-activity-history-{stamp}-{suffix}"));
        match fs::create_dir(&dir) {
            Ok(()) => {
                let result = write_reports(&dir, query, view);
                if let Err(error) = result {
                    let _ = fs::remove_dir_all(&dir);
                    return Err(error);
                }
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Не удалось найти свободное имя экспорта".into())
}

fn safe(value: &str) -> String {
    if value
        .trim_start_matches([' ', '\t', '\r', '\n'])
        .starts_with(['=', '+', '-', '@'])
        || value.starts_with(['\t', '\r', '\n'])
    {
        format!("'{value}")
    } else {
        value.into()
    }
}
fn optional(value: Option<f64>) -> String {
    value.map(|v| format!("{v:.3}")).unwrap_or_default()
}
fn local_time(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_default()
}
fn writer(mut output: impl Write) -> Result<csv::Writer<impl Write>, String> {
    output
        .write_all(b"\xEF\xBB\xBF")
        .map_err(|e| e.to_string())?;
    Ok(csv::WriterBuilder::new()
        .delimiter(b';')
        .from_writer(output))
}
fn file(dir: &Path, name: &str) -> Result<std::fs::File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(name))
        .map_err(|e| e.to_string())
}
fn write_reports(dir: &Path, query: &HistoryQuery, view: &HistoryView) -> Result<(), String> {
    write_summary(file(dir, "application-summary.csv")?, query, view)?;
    write_points(file(dir, "system-series.csv")?, query, view, false)?;
    if !view.data.series.is_empty() {
        write_points(
            file(dir, "selected-application-series.csv")?,
            query,
            view,
            true,
        )?;
    }
    Ok(())
}
fn write_summary(
    output: impl Write,
    query: &HistoryQuery,
    view: &HistoryView,
) -> Result<(), String> {
    let mut csv = writer(output)?;
    csv.write_record([
        "window_from_utc_seconds",
        "window_to_utc_seconds",
        "application_id",
        "name",
        "executable",
        "observed_seconds",
        "window_coverage_percent",
        "cpu_avg_percent",
        "cpu_peak_percent",
        "cpu_seconds",
        "ram_avg_bytes",
        "ram_peak_bytes",
        "gpu_avg_percent",
        "downloaded_bytes",
        "uploaded_bytes",
        "network_observed_seconds",
        "last_seen_utc_seconds",
        "gpu_observed_seconds",
    ])
    .map_err(|e| e.to_string())?;
    for app in &view.data.applications {
        csv.write_record([
            query.from.to_string(),
            query.to.to_string(),
            app.id.to_string(),
            safe(&app.name),
            safe(&app.executable),
            format!("{:.3}", app.observed_secs),
            format!(
                "{:.3}",
                app.observed_secs / (query.to - query.from).max(1) as f64 * 100.0
            ),
            format!("{:.3}", app.cpu_avg),
            format!("{:.3}", app.cpu_peak),
            format!("{:.3}", app.cpu_seconds),
            format!("{:.3}", app.ram_avg),
            app.ram_peak.to_string(),
            optional(app.gpu_avg),
            optional(app.downloaded_bytes),
            optional(app.uploaded_bytes),
            format!("{:.3}", app.network_observed_secs),
            app.last_seen.to_string(),
            format!("{:.3}", app.gpu_observed_secs),
        ])
        .map_err(|e| e.to_string())?;
    }
    csv.flush().map_err(|e| e.to_string())
}
fn point_row(
    query: &HistoryQuery,
    id: &str,
    name: &str,
    executable: &str,
    point: &HistoryPoint,
) -> Vec<String> {
    vec![
        query.from.to_string(),
        query.to.to_string(),
        id.into(),
        safe(name),
        safe(executable),
        point.timestamp.to_string(),
        local_time(point.timestamp),
        format!("{:.3}", point.duration_secs),
        format!("{:.3}", point.cpu_avg),
        format!("{:.3}", point.cpu_peak),
        format!("{:.3}", point.ram_avg),
        point.ram_peak.to_string(),
        optional(point.gpu_avg),
        optional(point.down_bps),
        optional(point.up_bps),
        optional(point.downloaded_bytes),
        optional(point.uploaded_bytes),
        format!("{:.3}", point.gpu_observed_secs),
        format!("{:.3}", point.network_observed_secs),
    ]
}
fn write_points(
    output: impl Write,
    query: &HistoryQuery,
    view: &HistoryView,
    applications: bool,
) -> Result<(), String> {
    let mut csv = writer(output)?;
    csv.write_record([
        "window_from_utc_seconds",
        "window_to_utc_seconds",
        "application_id",
        "name",
        "executable",
        "bucket_utc_seconds",
        "bucket_local_time_with_offset",
        "observed_seconds",
        "cpu_avg_percent",
        "cpu_peak_percent",
        "ram_avg_bytes",
        "ram_peak_bytes",
        "gpu_avg_percent",
        "download_bytes_per_second",
        "upload_bytes_per_second",
        "downloaded_bytes",
        "uploaded_bytes",
        "gpu_observed_seconds",
        "network_observed_seconds",
    ])
    .map_err(|e| e.to_string())?;
    if applications {
        for series in &view.data.series {
            for point in &series.points {
                csv.write_record(point_row(
                    query,
                    &series.application.id.to_string(),
                    &series.application.name,
                    &series.application.executable,
                    point,
                ))
                .map_err(|e| e.to_string())?;
            }
        }
    } else {
        for point in &view.data.systems {
            csv.write_record(point_row(query, "", "System", "", point))
                .map_err(|e| e.to_string())?;
        }
    }
    csv.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::types::*;
    #[test]
    fn historical_reports_preserve_window_identity_unknowns_and_formula_safety() {
        let query = HistoryQuery {
            request_id: 7,
            from: 100,
            to: 200,
            application_ids: vec![3],
        };
        let view = HistoryView {
            request_id: 7,
            data: HistoryData {
                applications: vec![ApplicationSummary {
                    id: 3,
                    name: "  =cmd".into(),
                    executable: "+path;file".into(),
                    observed_secs: 25.0,
                    ..Default::default()
                }],
                systems: vec![HistoryPoint {
                    timestamp: 120,
                    duration_secs: 25.0,
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut summary = Vec::new();
        write_summary(&mut summary, &query, &view).unwrap();
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b';')
            .from_reader(&summary[3..]);
        let row = reader.records().next().unwrap().unwrap();
        assert_eq!(&row[0], "100");
        assert_eq!(&row[1], "200");
        assert_eq!(&row[3], "'  =cmd");
        assert_eq!(&row[4], "'+path;file");
        assert_eq!(&row[6], "25.000");
        assert_eq!(&row[12], "");
        let mut points = Vec::new();
        write_points(&mut points, &query, &view, false).unwrap();
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b';')
            .from_reader(&points[3..]);
        let row = reader.records().next().unwrap().unwrap();
        assert_eq!(&row[5], "120");
        assert_eq!(&row[13], "");
    }
    #[test]
    fn simultaneous_exports_get_distinct_directories_and_only_requested_series() {
        let dir = tempfile::tempdir().unwrap();
        let query = HistoryQuery::default();
        let view = HistoryView::default();
        let first = export_to(dir.path(), &query, &view, 42).unwrap();
        let second = export_to(dir.path(), &query, &view, 42).unwrap();
        assert_ne!(first, second);
        assert!(first.join("application-summary.csv").exists());
        assert!(first.join("system-series.csv").exists());
        assert!(!first.join("selected-application-series.csv").exists());
    }
    #[test]
    fn stale_response_is_rejected_before_creating_files() {
        assert!(
            export_history(
                &HistoryQuery {
                    request_id: 2,
                    ..Default::default()
                },
                &HistoryView {
                    request_id: 1,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
