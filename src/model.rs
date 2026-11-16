use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// One observed process lifetime. Native tokens preserve sub-second identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: u64,
    pub lifetime: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessRow {
    pub pid: u32,
    /// UTC Unix seconds identifying this process lifetime together with its PID.
    pub started_at: u64,
    pub count: usize,
    /// Native exact process creation token (FILETIME or Linux start ticks).
    pub lifetime: Option<u64>,
    pub metrics: crate::metrics::Measurements,
    /// Raw OS parent PID; it is not sufficient to establish a current parent.
    pub parent_pid: Option<u32>,
    /// Current parent validated against both process creation identities.
    pub parent: Option<ProcessIdentity>,
    /// Presentation only; the collector always leaves this at zero.
    pub view_depth: usize,
    /// A summary of multiple same-name processes, never a real PID.
    pub is_group: bool,
    pub name: String,
    pub executable: String,
    /// CPU usage normalized to 0..100% of all logical CPUs.
    pub cpu_pct: Option<f32>,
    pub gpu_pct: Option<f32>,
    pub gpu_memory_bytes: Option<u64>,
    pub ram_bytes: Option<u64>,
    pub down_bps: Option<f64>,
    pub up_bps: Option<f64>,
    pub downloaded_bytes: Option<u64>,
    pub uploaded_bytes: Option<u64>,
}

impl ProcessRow {
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            started_at: self.started_at,
            lifetime: self.lifetime,
        }
    }
}

/// Resolve against the complete sample, before search hides any parent rows.
/// A newer process reusing an exited parent's PID cannot become that parent.
pub fn resolve_parents(rows: &mut [ProcessRow]) {
    let mut identities: HashMap<u32, Option<(ProcessIdentity, usize)>> =
        HashMap::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        identities
            .entry(row.pid)
            .and_modify(|value| *value = None)
            .or_insert(Some((row.identity(), index)));
    }
    let mut parents = vec![None; rows.len()];
    for (index, row) in rows.iter_mut().enumerate() {
        row.parent = None;
        let parent = row
            .parent_pid
            .and_then(|pid| identities.get(&pid))
            .and_then(|entry| *entry);
        if let Some((parent, parent_index)) = parent
            && parent.pid != row.pid
            && parent.started_at <= row.started_at
            && parent
                .lifetime
                .zip(row.lifetime)
                .is_some_and(|(parent, child)| parent <= child)
        {
            row.parent = Some(parent);
            parents[index] = Some(parent_index);
        }
    }
    // An OS should not return cycles. Equal-tick process creation and malformed
    // sources can nevertheless produce one; discard one edge deterministically.
    // Each node is traversed once; no recursion or quadratic ancestor walks.
    let mut state = vec![0_u8; rows.len()];
    for index in 0..rows.len() {
        if state[index] != 0 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(index);
        while let Some(node) = current {
            if state[node] == 1 {
                parents[node] = None;
                rows[node].parent = None;
                break;
            }
            if state[node] == 2 {
                break;
            }
            state[node] = 1;
            path.push(node);
            current = parents[node];
        }
        for node in path {
            state[node] = 2;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// UTC Unix seconds at the resource sample capture, independent of UI repainting.
    pub sampled_at: i64,
    /// Actual contiguous sampling interval; zero at startup and after a pause.
    pub sample_seconds: f64,
    pub process_rows: Vec<ProcessRow>,
    /// Incomplete or unavailable native process metrics, separate from network status.
    pub process_status: String,
    pub devices: Vec<crate::metrics::DeviceRow>,
    pub disk_status: String,
    pub cpu_pct: f32,
    pub logical_cpus: Vec<Option<f32>>,
    pub logical_cpu_count: usize,
    pub thermal: crate::thermal::ThermalSnapshot,
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
    Metric(crate::metrics::MetricKind),
}

impl SortColumn {
    pub const ALL: [(SortColumn, &'static str); 20] = [
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
        (
            Self::Metric(crate::metrics::MetricKind::IoRead),
            "I/O чтение/с",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::IoWrite),
            "I/O запись/с",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::GpuDedicated),
            "GPU dedicated",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::GpuShared),
            "GPU shared",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::PrivateCommit),
            "Private commit",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::ResidentShared),
            "Shared resident",
        ),
        (Self::Metric(crate::metrics::MetricKind::Swap), "Swap"),
        (
            Self::Metric(crate::metrics::MetricKind::MinorFaults),
            "Minor faults/с",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::MajorFaults),
            "Major faults/с",
        ),
        (
            Self::Metric(crate::metrics::MetricKind::PageFaults),
            "Page faults/с",
        ),
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
                SortColumn::Cpu => (a.cpu_pct.is_none(), b.cpu_pct.is_none()),
                SortColumn::Ram => (a.ram_bytes.is_none(), b.ram_bytes.is_none()),
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
                SortColumn::Metric(k) => (!a.metrics.contains_key(&k), !b.metrics.contains_key(&k)),
                _ => (false, false),
            };
            if missing.0 != missing.1 {
                return missing.0.cmp(&missing.1);
            }
            let order = match self.column {
                SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortColumn::Pid => a.pid.cmp(&b.pid),
                SortColumn::Cpu => compare_option(a.cpu_pct, b.cpu_pct),
                SortColumn::Gpu => compare_option(a.gpu_pct, b.gpu_pct),
                SortColumn::Ram => a.ram_bytes.cmp(&b.ram_bytes),
                SortColumn::GpuMemory => compare_option(a.gpu_memory_bytes, b.gpu_memory_bytes),
                SortColumn::Down => compare_option(a.down_bps, b.down_bps),
                SortColumn::Up => compare_option(a.up_bps, b.up_bps),
                SortColumn::TotalDown => compare_option(a.downloaded_bytes, b.downloaded_bytes),
                SortColumn::TotalUp => compare_option(a.uploaded_bytes, b.uploaded_bytes),
                SortColumn::Metric(k) => compare_option(a.metrics.get(&k), b.metrics.get(&k)),
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

#[derive(Clone, Debug, PartialEq)]
pub struct Filters {
    pub query: String,
    pub only_active: bool,
    pub only_network: bool,
    pub minimum_cpu: f32,
    pub minimum_ram_mb: u32,
    pub group_by_name: bool,
    /// Tree is a separate view and takes precedence over name grouping.
    pub tree: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            query: String::new(),
            only_active: false,
            only_network: false,
            minimum_cpu: 0.0,
            minimum_ram_mb: 0,
            group_by_name: true,
            tree: false,
        }
    }
}

impl Filters {
    pub fn apply(&self, rows: &[ProcessRow], sort: SortOrder) -> Vec<ProcessRow> {
        self.build(rows, sort, &HashSet::new())
    }

    fn includes(&self, p: &ProcessRow) -> bool {
        (self.minimum_cpu <= 0.0 || p.cpu_pct.is_some_and(|value| value >= self.minimum_cpu))
            && (self.minimum_ram_mb == 0
                || p.ram_bytes
                    .is_some_and(|value| value >= self.minimum_ram_mb as u64 * 1024 * 1024))
            && (!self.only_network
                || p.down_bps.unwrap_or(0.0) > 0.0
                || p.up_bps.unwrap_or(0.0) > 0.0)
            && (!self.only_active
                || p.cpu_pct.is_some_and(|value| value >= 1.0)
                || p.gpu_pct.unwrap_or(0.0) >= 1.0
                || p.down_bps.unwrap_or(0.0) >= 1024.0
                || p.up_bps.unwrap_or(0.0) >= 1024.0
                || p.metrics
                    .get(&crate::metrics::MetricKind::IoRead)
                    .copied()
                    .unwrap_or(0.0)
                    >= 1024.0
                || p.metrics
                    .get(&crate::metrics::MetricKind::IoWrite)
                    .copied()
                    .unwrap_or(0.0)
                    >= 1024.0)
    }

    fn build(
        &self,
        rows: &[ProcessRow],
        sort: SortOrder,
        expanded: &HashSet<String>,
    ) -> Vec<ProcessRow> {
        // Search individual identities before grouping erases PIDs and paths.
        let query = self.query.trim().to_lowercase();
        let mut matches = rows.to_vec();
        resolve_parents(&mut matches);
        matches.retain(|p| {
            query.is_empty()
                || p.name.to_lowercase().contains(&query)
                || p.executable.to_lowercase().contains(&query)
                || p.pid.to_string().contains(&query)
        });
        for row in &mut matches {
            row.view_depth = 0;
            row.is_group = false;
        }
        if self.tree || !self.group_by_name {
            matches.retain(|p| self.includes(p));
            sort.sort(&mut matches);
            return if self.tree {
                tree_rows(matches)
            } else {
                matches
            };
        }
        let mut groups = aggregate_by_name(&matches);
        groups.retain(|p| self.includes(p));
        sort.sort(&mut groups);
        if expanded.is_empty() {
            return groups;
        }
        let eligible: HashSet<_> = groups
            .iter()
            .filter(|p| p.is_group && expanded.contains(&p.name.to_lowercase()))
            .map(|p| p.name.to_lowercase())
            .collect();
        let mut members: HashMap<String, Vec<ProcessRow>> = HashMap::new();
        for mut row in matches {
            let key = row.name.to_lowercase();
            if eligible.contains(&key) {
                row.view_depth = 1;
                members.entry(key).or_default().push(row);
            }
        }
        for group in members.values_mut() {
            sort.sort(group);
        }
        let mut visible =
            Vec::with_capacity(groups.len() + members.values().map(Vec::len).sum::<usize>());
        for group in groups {
            let key = group.name.to_lowercase();
            visible.push(group);
            if let Some(children) = members.remove(&key) {
                visible.extend(children);
            }
        }
        visible
    }
}

/// `rows` are globally sorted first. Their order then sorts roots and siblings;
/// CPU/RAM remain the individual node's metrics, without subtree summation.
fn tree_rows(rows: Vec<ProcessRow>) -> Vec<ProcessRow> {
    let identities: HashMap<_, _> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.identity(), i))
        .collect();
    let mut children = vec![Vec::new(); rows.len()];
    let mut roots = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(parent) = row.parent.and_then(|id| identities.get(&id).copied()) {
            children[parent].push(index);
        } else {
            roots.push(index);
        }
    }
    let mut stack: Vec<_> = roots
        .into_iter()
        .rev()
        .map(|index| (index, 0_usize))
        .collect();
    let mut result = Vec::with_capacity(rows.len());
    let mut rows: Vec<_> = rows.into_iter().map(Some).collect();
    // resolve_parents removed cycles, but the visited Option also guards future
    // source changes. No recursion, including tens of thousands of generations.
    while let Some((index, depth)) = stack.pop() {
        let Some(mut row) = rows[index].take() else {
            continue;
        };
        row.view_depth = depth.min(64);
        result.push(row);
        stack.extend(
            children[index]
                .iter()
                .rev()
                .map(|child| (*child, (depth + 1).min(64))),
        );
    }
    // Fail closed if malformed duplicate identities prevented a rooted graph.
    result.extend(rows.into_iter().flatten());
    result
}

const MAX_EXPANDED_GROUPS: usize = 1024;

#[derive(Default)]
pub struct ProcessView {
    updated_at: Option<std::time::Instant>,
    filters: Option<Filters>,
    sort: SortOrder,
    rows: Arc<[ProcessRow]>,
    expanded: HashSet<String>,
    expansion_changed: bool,
}

impl ProcessView {
    pub fn toggle_group(&mut self, name: &str) {
        if name.len() > 4096 {
            return;
        }
        let name = name.to_lowercase();
        if !self.expanded.remove(&name) {
            if self.expanded.len() >= MAX_EXPANDED_GROUPS {
                return;
            }
            self.expanded.insert(name);
        }
        self.expansion_changed = true;
    }

    pub fn is_group_expanded(&self, name: &str) -> bool {
        self.expanded.contains(&name.to_lowercase())
    }

    pub fn rows(
        &mut self,
        snapshot: &Snapshot,
        filters: &Filters,
        sort: SortOrder,
    ) -> Arc<[ProcessRow]> {
        if self.updated_at != snapshot.updated_at
            || self.filters.as_ref() != Some(filters)
            || self.sort != sort
            || self.expansion_changed
        {
            if self.updated_at != snapshot.updated_at {
                let alive: HashSet<_> = snapshot
                    .process_rows
                    .iter()
                    .map(|row| row.name.to_lowercase())
                    .collect();
                self.expanded.retain(|name| alive.contains(name));
            }
            self.rows = if self.expanded.is_empty() {
                filters.apply(&snapshot.process_rows, sort)
            } else {
                filters.build(&snapshot.process_rows, sort, &self.expanded)
            }
            .into();
            self.updated_at = snapshot.updated_at;
            self.filters = Some(filters.clone());
            self.sort = sort;
            self.expansion_changed = false;
        }
        Arc::clone(&self.rows)
    }
}

fn add_option<T: std::ops::Add<Output = T> + Copy>(to: &mut Option<T>, from: Option<T>) {
    *to = to.zip(from).map(|(current, from)| current + from);
}

pub fn aggregate_by_name(rows: &[ProcessRow]) -> Vec<ProcessRow> {
    let mut groups: HashMap<String, ProcessRow> = HashMap::new();
    for process in rows {
        let key = process.name.to_lowercase();
        if let Some(group) = groups.get_mut(&key) {
            group.count += 1;
            group.pid = 0;
            group.started_at = 0;
            group.lifetime = None;
            group.parent_pid = None;
            group.parent = None;
            group.is_group = true;
            add_option(&mut group.cpu_pct, process.cpu_pct);
            group.ram_bytes = group
                .ram_bytes
                .zip(process.ram_bytes)
                .map(|(a, b)| a.saturating_add(b));
            group.metrics.retain(|key, value| {
                if let Some(other) = process.metrics.get(key) {
                    *value += other;
                    true
                } else {
                    false
                }
            });
            group.gpu_pct = group.gpu_pct.zip(process.gpu_pct).map(|(a, b)| a.max(b));
            add_option(&mut group.gpu_memory_bytes, process.gpu_memory_bytes);
            add_option(&mut group.down_bps, process.down_bps);
            add_option(&mut group.up_bps, process.up_bps);
            add_option(&mut group.downloaded_bytes, process.downloaded_bytes);
            add_option(&mut group.uploaded_bytes, process.uploaded_bytes);
            group.executable.clear();
        } else {
            let mut first = process.clone();
            first.count = 1;
            first.view_depth = 0;
            first.is_group = false;
            groups.insert(key, first);
        }
    }
    groups.into_values().collect()
}

/// Logical processors each report 0..100%; their mean relates to host CPU%.
pub fn logical_cpu_average(values: &[Option<f32>]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    let total = values.iter().try_fold(0.0_f64, |sum, value| {
        let value = (*value)?;
        (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(sum + value as f64)
    })?;
    Some((total / values.len() as f64) as f32)
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
            cpu_pct: Some(cpu_pct),
            down_bps: rx,
            ram_bytes: Some(1024 * 1024),
            ..Default::default()
        }
    }

    #[test]
    fn logical_cpu_average_requires_all_known_values_and_preserves_host_percentages() {
        assert_eq!(logical_cpu_average(&[Some(0.0), Some(100.0)]), Some(50.0));
        assert_eq!(logical_cpu_average(&vec![Some(25.0); 1024]), Some(25.0));
        assert_eq!(logical_cpu_average(&[Some(0.0), None]), None);
        assert_eq!(logical_cpu_average(&[]), None);
        assert_eq!(logical_cpu_average(&[Some(f32::NAN)]), None);
        assert_eq!(logical_cpu_average(&[Some(101.0)]), None);
    }

    #[test]
    fn grouped_gpu_is_busiest_process_and_unknown_member_is_not_hidden() {
        let mut a = row(1, "same", 0.0, None);
        let mut b = row(2, "same", 0.0, None);
        a.gpu_pct = Some(80.0);
        b.gpu_pct = Some(70.0);
        assert_eq!(
            aggregate_by_name(&[a.clone(), b.clone()])[0].gpu_pct,
            Some(80.0)
        );
        b.gpu_pct = None;
        assert_eq!(aggregate_by_name(&[a, b])[0].gpu_pct, None);
    }

    #[test]
    fn supplementary_rates_sort_unknown_last_and_groups_require_all_members() {
        use crate::metrics::MetricKind::IoRead;
        let mut a = row(1, "same", 0.0, None);
        a.metrics.insert(IoRead, 100.0);
        let b = row(2, "same", 0.0, None);
        assert!(
            !aggregate_by_name(&[a.clone(), b.clone()])[0]
                .metrics
                .contains_key(&IoRead)
        );
        for descending in [true, false] {
            let mut rows = vec![b.clone(), a.clone()];
            SortOrder {
                column: SortColumn::Metric(IoRead),
                descending,
            }
            .sort(&mut rows);
            assert_eq!(rows[0].pid, 1);
        }
    }

    #[test]
    fn grouping_is_enabled_by_default_and_can_be_disabled() {
        let rows = [row(1, "Editor", 1.0, None), row(2, "Editor", 2.0, None)];
        let mut filters = Filters::default();
        assert!(filters.group_by_name);
        assert_eq!(filters.apply(&rows, SortOrder::default()).len(), 1);
        filters.group_by_name = false;
        assert_eq!(filters.apply(&rows, SortOrder::default()).len(), 2);
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
        assert_eq!(grouped[0].cpu_pct, Some(30.0));
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
        assert_eq!(visible[0].cpu_pct, Some(2.0));
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
        snapshot.process_rows[0].cpu_pct = Some(90.0);
        assert_eq!(view.rows(&snapshot, &filters, ascending)[0].pid, 51);
    }

    #[test]
    fn unset_metrics_are_not_falsely_zero() {
        let a = row(1, "A", 1.0, None);
        let b = row(2, "A", 1.0, Some(100.0));
        assert_eq!(aggregate_by_name(&[a, b])[0].down_bps, None);
    }
    fn native_row(pid: u32, parent_pid: Option<u32>, token: u64, cpu: f32) -> ProcessRow {
        ProcessRow {
            pid,
            started_at: token / 100,
            lifetime: Some(token),
            parent_pid,
            name: format!("p{pid}"),
            cpu_pct: Some(cpu),
            count: 1,
            ..Default::default()
        }
    }

    #[test]
    fn expanded_groups_keep_summary_and_matching_sorted_members_in_export_order() {
        let mut view = ProcessView::default();
        let mut a = row(1, "Editor", 10.0, Some(1.0));
        a.started_at = 10;
        a.lifetime = Some(1000);
        let mut b = row(2, "editor", 20.0, Some(2.0));
        b.started_at = 20;
        b.lifetime = Some(2000);
        let snapshot = Snapshot {
            updated_at: Some(std::time::Instant::now()),
            process_rows: vec![a, b, row(3, "Browser", 40.0, None)],
            ..Default::default()
        };
        let filters = Filters::default();
        assert_eq!(
            view.rows(&snapshot, &filters, SortOrder::default()).len(),
            2
        );
        view.toggle_group("EDITOR");
        let visible = view.rows(&snapshot, &filters, SortOrder::default());
        assert_eq!(
            visible.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![3, 0, 2, 1]
        );
        assert!(visible[1].is_group);
        assert_eq!(visible[1].started_at, 0);
        assert_eq!(visible[1].lifetime, None);
        assert_eq!(visible[1].cpu_pct, Some(30.0));
        assert_eq!(visible[2].view_depth, 1);
        assert!(!visible[2].is_group);
        assert!(Arc::ptr_eq(
            &visible,
            &view.rows(&snapshot, &filters, SortOrder::default())
        ));
        view.toggle_group("editor");
        assert_eq!(
            view.rows(&snapshot, &filters, SortOrder::default()).len(),
            2
        );
        let selected = Filters {
            query: "2".into(),
            ..filters
        };
        let filtered = view.rows(&snapshot, &selected, SortOrder::default());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pid, 2);
        assert_eq!(filtered[0].cpu_pct, Some(20.0));
    }

    #[test]
    fn group_thresholds_do_not_hide_low_load_members_of_visible_group() {
        let mut view = ProcessView::default();
        let snapshot = Snapshot {
            updated_at: Some(std::time::Instant::now()),
            process_rows: vec![row(1, "app", 6.0, None), row(2, "app", 5.0, None)],
            ..Default::default()
        };
        view.toggle_group("app");
        let filters = Filters {
            minimum_cpu: 10.0,
            ..Default::default()
        };
        let visible = view.rows(&snapshot, &filters, SortOrder::default());
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].cpu_pct, Some(11.0));
    }

    #[test]
    fn tree_is_distinct_from_name_groups_and_keeps_parent_then_sorted_siblings() {
        let mut parent = native_row(1, None, 100, 1.0);
        parent.name = "same".into();
        let mut child = native_row(2, Some(1), 200, 90.0);
        child.name = "same".into();
        let sibling = native_row(3, Some(1), 300, 50.0);
        let other = native_row(4, None, 400, 2.0);
        let filters = Filters {
            tree: true,
            ..Default::default()
        };
        let visible = filters.apply(&[child, sibling, parent, other], SortOrder::default());
        assert_eq!(
            visible.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![4, 1, 2, 3]
        );
        assert!(visible.iter().all(|p| !p.is_group));
        assert_eq!(visible[2].parent, Some(visible[1].identity()));
        assert_eq!(visible[2].view_depth, 1);
    }

    #[test]
    fn tree_never_attaches_to_reused_missing_or_unknown_parent_identity() {
        let old_child = native_row(2, Some(1), 200, 1.0);
        let replacement_parent = native_row(1, None, 300, 1.0);
        let mut unknown_parent = native_row(3, None, 50, 1.0);
        unknown_parent.lifetime = None;
        let child_of_unknown = native_row(4, Some(3), 400, 1.0);
        let orphan = native_row(5, Some(99), 500, 1.0);
        let filters = Filters {
            tree: true,
            ..Default::default()
        };
        let visible = filters.apply(
            &[
                old_child,
                replacement_parent,
                unknown_parent,
                child_of_unknown,
                orphan,
            ],
            SortOrder::default(),
        );
        assert!(
            visible
                .iter()
                .all(|p| p.view_depth == 0 && p.parent.is_none())
        );
    }

    #[test]
    fn tree_filter_makes_hidden_parent_child_a_root_without_injecting_ancestors() {
        let parent = native_row(1, None, 100, 1.0);
        let child = native_row(2, Some(1), 200, 90.0);
        let filters = Filters {
            tree: true,
            minimum_cpu: 10.0,
            ..Default::default()
        };
        let visible = filters.apply(&[parent.clone(), child.clone()], SortOrder::default());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].pid, 2);
        assert_eq!(visible[0].view_depth, 0);
        assert_eq!(visible[0].parent, Some(parent.identity()));
        let search = Filters {
            query: "p2".into(),
            ..filters
        };
        assert_eq!(
            search.apply(&[parent, child], SortOrder::default()).len(),
            1
        );
    }

    #[test]
    fn tree_cycles_are_broken_and_large_chains_do_not_recurse_or_duplicate_rows() {
        let cyclic = [
            native_row(1, Some(2), 100, 2.0),
            native_row(2, Some(1), 100, 1.0),
        ];
        let filters = Filters {
            tree: true,
            ..Default::default()
        };
        let visible = filters.apply(&cyclic, SortOrder::default());
        assert_eq!(visible.len(), 2);
        assert!(
            visible
                .iter()
                .any(|p| p.view_depth == 0 && p.parent.is_none())
        );
        let chain = (1..=20_000)
            .map(|pid| native_row(pid, (pid > 1).then_some(pid - 1), pid as u64, 0.0))
            .collect::<Vec<_>>();
        let visible = filters.apply(&chain, SortOrder::default());
        assert_eq!(visible.len(), chain.len());
        assert_eq!(visible.first().unwrap().pid, 1);
        assert_eq!(visible.last().unwrap().pid, 20_000);
        assert!(visible.iter().all(|p| p.view_depth <= 64));
    }

    #[test]
    fn expansion_state_is_pruned_when_names_exit_and_cache_rebuilds_once() {
        let mut view = ProcessView::default();
        let mut snapshot = Snapshot {
            updated_at: Some(std::time::Instant::now()),
            process_rows: vec![row(1, "app", 1.0, None), row(2, "app", 2.0, None)],
            ..Default::default()
        };
        let filters = Filters::default();
        view.rows(&snapshot, &filters, SortOrder::default());
        view.toggle_group("app");
        let expanded = view.rows(&snapshot, &filters, SortOrder::default());
        assert!(view.is_group_expanded("APP"));
        assert!(Arc::ptr_eq(
            &expanded,
            &view.rows(&snapshot, &filters, SortOrder::default())
        ));
        snapshot.updated_at = snapshot
            .updated_at
            .map(|at| at + std::time::Duration::from_secs(1));
        snapshot.process_rows.clear();
        assert!(
            view.rows(&snapshot, &filters, SortOrder::default())
                .is_empty()
        );
        assert!(!view.is_group_expanded("app"));
    }
    #[test]
    fn unknown_cpu_and_resident_memory_sort_last_in_both_directions() {
        let unknown = ProcessRow {
            pid: 1,
            name: "unknown".into(),
            cpu_pct: None,
            ram_bytes: None,
            ..Default::default()
        };
        let known_zero = ProcessRow {
            pid: 2,
            name: "zero".into(),
            cpu_pct: Some(0.0),
            ram_bytes: Some(0),
            ..Default::default()
        };
        let known_busy = ProcessRow {
            pid: 3,
            name: "busy".into(),
            cpu_pct: Some(10.0),
            ram_bytes: Some(1024),
            ..Default::default()
        };
        for column in [SortColumn::Cpu, SortColumn::Ram] {
            for descending in [false, true] {
                let mut rows = vec![unknown.clone(), known_zero.clone(), known_busy.clone()];
                SortOrder { column, descending }.sort(&mut rows);
                assert_eq!(rows.last().unwrap().pid, 1);
                assert_eq!(rows[0].pid, if descending { 3 } else { 2 });
            }
        }
    }

    #[test]
    fn cpu_and_resident_groups_need_every_member_and_preserve_known_zero() {
        let a = row(1, "same", 0.0, None);
        let mut b = row(2, "same", 10.0, None);
        b.cpu_pct = None;
        b.ram_bytes = None;
        for members in [vec![a.clone(), b.clone()], vec![b, a.clone()]] {
            let summary = aggregate_by_name(&members);
            assert_eq!(summary[0].cpu_pct, None);
            assert_eq!(summary[0].ram_bytes, None);
        }
        let summary = aggregate_by_name(&[a.clone(), a]);
        assert_eq!(summary[0].cpu_pct, Some(0.0));
        assert_eq!(summary[0].ram_bytes, Some(2 * 1024 * 1024));
    }

    #[test]
    fn filters_keep_unknown_without_threshold_and_exclude_it_for_positive_threshold() {
        let unknown = ProcessRow {
            pid: 1,
            name: "unknown".into(),
            ..Default::default()
        };
        let filters = Filters {
            group_by_name: false,
            ..Default::default()
        };
        assert_eq!(
            filters
                .apply(std::slice::from_ref(&unknown), SortOrder::default())
                .len(),
            1
        );
        for thresholds in [
            Filters {
                minimum_cpu: 0.1,
                ..filters.clone()
            },
            Filters {
                minimum_ram_mb: 1,
                ..filters.clone()
            },
            Filters {
                only_active: true,
                ..filters.clone()
            },
        ] {
            assert!(
                thresholds
                    .apply(std::slice::from_ref(&unknown), SortOrder::default())
                    .is_empty()
            );
        }
        let mut active_io = unknown;
        active_io
            .metrics
            .insert(crate::metrics::MetricKind::IoRead, 2048.0);
        assert_eq!(
            Filters {
                only_active: true,
                ..filters
            }
            .apply(&[active_io], SortOrder::default())
            .len(),
            1
        );
    }

    #[test]
    fn resident_group_sum_saturates_instead_of_overflowing() {
        let mut a = row(1, "same", 1.0, None);
        a.ram_bytes = Some(u64::MAX);
        let mut b = row(2, "same", 1.0, None);
        b.ram_bytes = Some(1);
        assert_eq!(aggregate_by_name(&[a, b])[0].ram_bytes, Some(u64::MAX));
    }
}
