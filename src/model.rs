use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct ProcessRow {
    pub pid: u32,
    pub count: usize,
    pub name: String,
    pub executable: String,
    /// CPU usage normalized to 0..100% of all logical CPUs.
    pub cpu_pct: f32,
    pub gpu_pct: Option<f32>,
    pub gpu_memory_bytes: Option<u64>,
    pub ram_bytes: u64,
    pub down_bps: Option<f64>,
    pub up_bps: Option<f64>,
    pub downloaded_bytes: Option<u64>,
    pub uploaded_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub process_rows: Vec<ProcessRow>,
    pub cpu_pct: f32,
    pub ram_used: u64,
    pub ram_total: u64,
    pub down_bps: f64,
    pub up_bps: f64,
    pub interface_count: usize,
    pub network_status: String,
    pub gpu_status: String,
    pub updated_at: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Pid,
    Cpu,
    Gpu,
    Ram,
    GpuMemory,
    Down,
    Up,
    TotalDown,
    TotalUp,
}

impl SortColumn {
    pub const ALL: [(SortColumn, &'static str); 10] = [
        (Self::Name, "Процесс"),
        (Self::Pid, "PID"),
        (Self::Cpu, "CPU %"),
        (Self::Gpu, "GPU %"),
        (Self::Ram, "RAM"),
        (Self::GpuMemory, "GPU RAM"),
        (Self::Down, "Приём/с"),
        (Self::Up, "Отдача/с"),
        (Self::TotalDown, "Принято"),
        (Self::TotalUp, "Отдано"),
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortOrder {
    pub column: SortColumn,
    pub descending: bool,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self {
            column: SortColumn::Cpu,
            descending: true,
        }
    }
}

impl SortOrder {
    pub fn select(&mut self, selected: SortColumn) {
        if self.column == selected {
            self.descending = !self.descending;
        } else {
            self.column = selected;
            self.descending = selected != SortColumn::Name && selected != SortColumn::Pid;
        }
    }

    pub fn sort(&self, rows: &mut [ProcessRow]) {
        rows.sort_by(|a, b| {
            // Missing metrics must sort last in BOTH directions; "—" is not 0.
            let missing = match self.column {
                SortColumn::Gpu => (a.gpu_pct.is_none(), b.gpu_pct.is_none()),
                SortColumn::GpuMemory => {
                    (a.gpu_memory_bytes.is_none(), b.gpu_memory_bytes.is_none())
                }
                SortColumn::Down => (a.down_bps.is_none(), b.down_bps.is_none()),
                SortColumn::Up => (a.up_bps.is_none(), b.up_bps.is_none()),
                SortColumn::TotalDown => {
                    (a.downloaded_bytes.is_none(), b.downloaded_bytes.is_none())
                }
                SortColumn::TotalUp => (a.uploaded_bytes.is_none(), b.uploaded_bytes.is_none()),
                _ => (false, false),
            };
            if missing.0 != missing.1 {
                return missing.0.cmp(&missing.1);
            }
            let order = match self.column {
                SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortColumn::Pid => a.pid.cmp(&b.pid),
                SortColumn::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
                SortColumn::Gpu => compare_option(a.gpu_pct, b.gpu_pct),
                SortColumn::Ram => a.ram_bytes.cmp(&b.ram_bytes),
                SortColumn::GpuMemory => compare_option(a.gpu_memory_bytes, b.gpu_memory_bytes),
                SortColumn::Down => compare_option(a.down_bps, b.down_bps),
                SortColumn::Up => compare_option(a.up_bps, b.up_bps),
                SortColumn::TotalDown => compare_option(a.downloaded_bytes, b.downloaded_bytes),
                SortColumn::TotalUp => compare_option(a.uploaded_bytes, b.uploaded_bytes),
            };
            let order = if self.descending {
                order.reverse()
            } else {
                order
            };
            order
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.pid.cmp(&b.pid))
        });
    }
}

fn compare_option<T: PartialOrd>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filters {
    pub query: String,
    pub only_active: bool,
    pub only_network: bool,
    pub minimum_cpu: f32,
    pub minimum_ram_mb: u32,
    pub group_by_name: bool,
}

impl Filters {
    pub fn apply(&self, rows: &[ProcessRow], sort: SortOrder) -> Vec<ProcessRow> {
        // Search individual identities before grouping erases PIDs and paths.
        let query = self.query.trim().to_lowercase();
        let matches = rows
            .iter()
            .filter(|p| {
                query.is_empty()
                    || p.name.to_lowercase().contains(&query)
                    || p.executable.to_lowercase().contains(&query)
                    || p.pid.to_string().contains(&query)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut visible = if self.group_by_name {
            aggregate_by_name(&matches)
        } else {
            matches
        };

        visible.retain(|p| {
            p.cpu_pct >= self.minimum_cpu
                && p.ram_bytes >= self.minimum_ram_mb as u64 * 1024 * 1024
                && (!self.only_network
                    || p.down_bps.unwrap_or(0.0) > 0.0
                    || p.up_bps.unwrap_or(0.0) > 0.0)
                && (!self.only_active
                    || p.cpu_pct >= 1.0
                    || p.gpu_pct.unwrap_or(0.0) >= 1.0
                    || p.down_bps.unwrap_or(0.0) >= 1024.0
                    || p.up_bps.unwrap_or(0.0) >= 1024.0)
        });
        sort.sort(&mut visible);
        visible
    }
}

#[derive(Default)]
pub struct ProcessView {
    updated_at: Option<std::time::Instant>,
    filters: Option<Filters>,
    sort: SortOrder,
    rows: Arc<[ProcessRow]>,
}

impl ProcessView {
    pub fn rows(
        &mut self,
        snapshot: &Snapshot,
        filters: &Filters,
        sort: SortOrder,
    ) -> Arc<[ProcessRow]> {
        if self.updated_at != snapshot.updated_at
            || self.filters.as_ref() != Some(filters)
            || self.sort != sort
        {
            self.rows = filters.apply(&snapshot.process_rows, sort).into();
            self.updated_at = snapshot.updated_at;
            self.filters = Some(filters.clone());
            self.sort = sort;
        }
        Arc::clone(&self.rows)
    }
}

fn add_option<T: std::ops::Add<Output = T> + Copy>(to: &mut Option<T>, from: Option<T>) {
    if let Some(from) = from {
        *to = Some(to.map_or(from, |current| current + from));
    }
}

pub fn aggregate_by_name(rows: &[ProcessRow]) -> Vec<ProcessRow> {
    let mut groups: HashMap<String, ProcessRow> = HashMap::new();
    for process in rows {
        let key = process.name.to_lowercase();
        if let Some(group) = groups.get_mut(&key) {
            group.count += 1;
            group.pid = 0;
            group.cpu_pct += process.cpu_pct;
            group.ram_bytes += process.ram_bytes;
            add_option(&mut group.gpu_pct, process.gpu_pct);
            add_option(&mut group.gpu_memory_bytes, process.gpu_memory_bytes);
            add_option(&mut group.down_bps, process.down_bps);
            add_option(&mut group.up_bps, process.up_bps);
            add_option(&mut group.downloaded_bytes, process.downloaded_bytes);
            add_option(&mut group.uploaded_bytes, process.uploaded_bytes);
            group.executable.clear();
        } else {
            let mut first = process.clone();
            first.count = 1;
            groups.insert(key, first);
        }
    }
    groups.into_values().collect()
}

pub fn format_bytes(bytes: u64) -> String {
    format_size(bytes as f64)
}

pub fn format_size(bytes: f64) -> String {
    let units = ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

pub fn format_rate(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{}/с", format_size(v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu_pct: f32, rx: Option<f64>) -> ProcessRow {
        ProcessRow {
            pid,
            name: name.into(),
            cpu_pct,
            down_bps: rx,
            ram_bytes: 1024 * 1024,
            ..Default::default()
        }
    }

    #[test]
    fn sorting_on_cpu_descending() {
        let mut rows = vec![row(2, "b", 10.0, None), row(1, "a", 80.0, None)];
        SortOrder::default().sort(&mut rows);
        assert_eq!(rows[0].pid, 1);
    }

    #[test]
    fn missing_metrics_are_last_regardless_of_direction() {
        let mut data = vec![row(1, "a", 1.0, None), row(2, "b", 1.0, Some(5.0))];
        let mut sort = SortOrder {
            column: SortColumn::Down,
            descending: true,
        };
        sort.sort(&mut data);
        assert_eq!(data[0].pid, 2);
        sort.descending = false;
        sort.sort(&mut data);
        assert_eq!(data[0].pid, 2);
    }

    #[test]
    fn grouping_preserves_totals_and_process_count() {
        let grouped = aggregate_by_name(&[
            row(1, "Firefox", 10.0, Some(1024.0)),
            row(2, "firefox", 20.0, Some(2048.0)),
        ]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].count, 2);
        assert_eq!(grouped[0].cpu_pct, 30.0);
        assert_eq!(grouped[0].down_bps, Some(3072.0));
    }

    #[test]
    fn filters_match_name_or_pid() {
        let data = [row(41, "Firefox", 2.0, None), row(51, "Code", 0.0, None)];
        let filters = Filters {
            query: "41".into(),
            ..Default::default()
        };
        assert_eq!(filters.apply(&data, SortOrder::default()).len(), 1);
    }

    #[test]
    fn grouped_search_matches_original_pid() {
        let data = [row(41, "Firefox", 2.0, None), row(51, "Firefox", 3.0, None)];
        let filters = Filters {
            query: "41".into(),
            group_by_name: true,
            ..Default::default()
        };
        let visible = filters.apply(&data, SortOrder::default());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].pid, 41);
        assert_eq!(visible[0].cpu_pct, 2.0);
    }

    #[test]
    fn grouped_search_matches_original_executable_path() {
        let mut a = row(41, "worker", 2.0, None);
        a.executable = "/opt/project-a/worker".into();
        let mut b = row(51, "worker", 3.0, None);
        b.executable = "/opt/project-b/worker".into();
        let filters = Filters {
            query: "project-b".into(),
            group_by_name: true,
            ..Default::default()
        };
        let visible = filters.apply(&[a, b], SortOrder::default());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].pid, 51);
    }

    #[test]
    fn view_reuses_rows_until_snapshot_filters_or_sort_change() {
        let mut view = ProcessView::default();
        let mut snapshot = Snapshot {
            updated_at: Some(std::time::Instant::now()),
            process_rows: vec![row(41, "Firefox", 2.0, None), row(51, "Code", 3.0, None)],
            ..Default::default()
        };
        let mut filters = Filters::default();
        let sort = SortOrder::default();
        let first = view.rows(&snapshot, &filters, sort);
        assert!(Arc::ptr_eq(&first, &view.rows(&snapshot, &filters, sort)));
        filters.query = "Firefox".into();
        assert_eq!(view.rows(&snapshot, &filters, sort)[0].pid, 41);
        filters.query.clear();
        let ascending = SortOrder {
            descending: false,
            ..sort
        };
        assert_eq!(view.rows(&snapshot, &filters, ascending)[0].pid, 41);
        snapshot.updated_at = snapshot
            .updated_at
            .map(|at| at + std::time::Duration::from_secs(1));
        snapshot.process_rows[0].cpu_pct = 90.0;
        assert_eq!(view.rows(&snapshot, &filters, ascending)[0].pid, 51);
    }

    #[test]
    fn unset_metrics_are_not_falsely_zero() {
        let a = row(1, "A", 1.0, None);
        let b = row(2, "A", 1.0, Some(100.0));
        assert_eq!(aggregate_by_name(&[a, b])[0].down_bps, Some(100.0));
    }
}
