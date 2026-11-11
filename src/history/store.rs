use super::aggregation::{Bucket, Metric};
use super::types::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::time::Duration;

const APPLICATION_ID: i64 = 0x50434148;
const MAX_RANGE: i64 = 7 * 86400;
const MAX_POINTS: usize = (MAX_RANGE / BUCKET_SECONDS + 1) as usize;

struct WriterLock(std::fs::File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        // Closing our descriptor alone does not release an open-file-description
        // lock while a forked child or try_clone() still owns a duplicate.
        let _ = self.0.unlock();
    }
}

pub(super) struct Store {
    // Fields drop in declaration order: finish SQLite before releasing ownership.
    connection: Connection,
    _writer_lock: WriterLock,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(&lock_path)
            .map_err(|e| format!("History writer lock: {e}"))?;
        lock.try_lock()
            .map_err(|e| format!("History is already open in another application instance: {e}"))?;
        Self::open_inner(path, WriterLock(lock)).map_err(|e| format!("History database: {e}"))
    }
    fn open_inner(path: &Path, writer_lock: WriterLock) -> Result<Self, rusqlite::Error> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(250))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let id: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if !((version == 0 && id == 0 && tables == 0)
            || ([1, 2].contains(&version) && id == APPLICATION_ID))
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        if version == 0 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE applications(id INTEGER PRIMARY KEY AUTOINCREMENT,identity TEXT NOT NULL UNIQUE,name TEXT NOT NULL,executable TEXT NOT NULL);
                CREATE TABLE buckets(timestamp INTEGER NOT NULL,application_id INTEGER NOT NULL,
                    last_seen INTEGER NOT NULL,ram_total INTEGER NOT NULL,
                    seconds REAL NOT NULL,cpu_sum REAL NOT NULL,cpu_peak REAL NOT NULL,
                    ram_sum REAL NOT NULL,ram_peak INTEGER NOT NULL,gpu_sum REAL NOT NULL,gpu_seconds REAL NOT NULL,
                    down REAL NOT NULL,up REAL NOT NULL,network_seconds REAL NOT NULL,
                    PRIMARY KEY(timestamp,application_id)) WITHOUT ROWID;
                CREATE INDEX buckets_application ON buckets(application_id,timestamp);
                CREATE TABLE history_state(id INTEGER PRIMARY KEY CHECK(id=1),high_water REAL);
                INSERT INTO history_state VALUES(1,NULL);
                PRAGMA application_id=1346584904; PRAGMA user_version=2; COMMIT;")?;
        }
        // Validate the complete owned schema before changing journal settings.
        connection.prepare("SELECT id,identity,name,executable FROM applications LIMIT 0")?;
        connection.prepare("SELECT timestamp,application_id,last_seen,ram_total,seconds,cpu_sum,cpu_peak,ram_sum,ram_peak,gpu_sum,gpu_seconds,down,up,network_seconds FROM buckets LIMIT 0")?;
        if version == 1 {
            // V1 lost exact fractional endpoints. Seal its last occupied bucket
            // once rather than guess an endpoint and risk overlapping old time.
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE history_state(id INTEGER PRIMARY KEY CHECK(id=1),high_water REAL);
                INSERT INTO history_state SELECT 1,max(timestamp)+30.0 FROM buckets WHERE application_id=0;
                PRAGMA user_version=2; COMMIT;")?;
        }
        connection.prepare("SELECT high_water FROM history_state WHERE id=1")?;
        let page_size: i64 = connection.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let max_pages = 512_i64 * 1024 * 1024 / page_size;
        connection.pragma_update(None, "max_page_count", max_pages)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=16777216;")?;
        Ok(Self {
            connection,
            _writer_lock: writer_lock,
        })
    }
    pub fn high_water(&self) -> Result<Option<f64>, String> {
        let value: Option<f64> = self
            .connection
            .query_row("SELECT high_water FROM history_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .map_err(|e| format!("History watermark: {e}"))?;
        if value.is_some_and(|v| !v.is_finite()) {
            return Err("History watermark is invalid".into());
        }
        Ok(value)
    }
    pub fn save(&mut self, buckets: Vec<Bucket>) -> Result<(), String> {
        self.save_inner(buckets)
            .map_err(|e| format!("History write: {e}"))
    }
    fn save_inner(&mut self, buckets: Vec<Bucket>) -> Result<(), rusqlite::Error> {
        if buckets.is_empty() {
            return Ok(());
        }
        let tx = self.connection.transaction()?;
        for bucket in buckets {
            tx.execute(
                "UPDATE history_state SET high_water=max(coalesce(high_water,?1),?1) WHERE id=1",
                [bucket.observed_until],
            )?;
            save_metric(&tx, &bucket, 0, &bucket.system, bucket.last_seen)?;
            for (key, app) in &bucket.applications {
                tx.execute("INSERT INTO applications(identity,name,executable) VALUES (?1,?2,?3) ON CONFLICT(identity) DO UPDATE SET name=excluded.name,executable=excluded.executable",params![key,app.name,app.executable])?;
                let id: i64 = tx.query_row(
                    "SELECT id FROM applications WHERE identity=?1",
                    [key],
                    |r| r.get(0),
                )?;
                save_metric(&tx, &bucket, id, &app.metric, app.last_seen)?;
            }
        }
        tx.commit()
    }
    pub fn query(&self, query: &HistoryQuery) -> Result<HistoryData, String> {
        self.query_inner(query)
            .map_err(|e| format!("History query: {e}"))
    }
    fn query_inner(&self, query: &HistoryQuery) -> Result<HistoryData, rusqlite::Error> {
        if query.to <= query.from
            || query.to.saturating_sub(query.from) > MAX_RANGE
            || query.application_ids.len() > MAX_COMPARISON_APPS
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let mut data = HistoryData::default();
        (data.earliest_timestamp, data.latest_timestamp) = self.connection.query_row(
            "SELECT min(timestamp),max(last_seen) FROM buckets WHERE application_id=0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        data.ram_total_bytes=self.connection.query_row("SELECT ram_total FROM buckets WHERE application_id=0 ORDER BY timestamp DESC LIMIT 1",[],|r|r.get::<_,i64>(0)).optional()?.unwrap_or(0).max(0) as u64;
        data.systems = self.points(0, query)?;
        data.recorded_secs = data.systems.iter().map(|p| p.duration_secs).sum();
        let mut statement=self.connection.prepare_cached("SELECT a.id,a.name,a.executable,sum(b.seconds),sum(b.cpu_sum),max(b.cpu_peak),sum(b.ram_sum),max(b.ram_peak),sum(b.gpu_sum),sum(b.gpu_seconds),sum(b.down),sum(b.up),sum(b.network_seconds),max(b.last_seen)
            FROM applications a JOIN buckets b ON b.application_id=a.id WHERE b.timestamp>=?1 AND b.timestamp<?2 GROUP BY a.id ORDER BY sum(b.cpu_sum) DESC,a.id")?;
        let rows = statement.query_map(params![query.from, query.to], |r| {
            let seconds: f64 = r.get(3)?;
            let cpu: f64 = r.get(4)?;
            let gpu_seconds: f64 = r.get(9)?;
            let network_seconds: f64 = r.get(12)?;
            Ok(ApplicationSummary {
                id: r.get(0)?,
                name: r.get(1)?,
                executable: r.get(2)?,
                observed_secs: seconds,
                cpu_avg: cpu / seconds,
                cpu_peak: r.get(5)?,
                cpu_seconds: cpu / 100.0,
                ram_avg: r.get::<_, f64>(6)? / seconds,
                ram_peak: r.get::<_, i64>(7)?.max(0) as u64,
                gpu_avg: (gpu_seconds > 0.0)
                    .then(|| r.get::<_, f64>(8).map(|v| v / gpu_seconds))
                    .transpose()?,
                gpu_observed_secs: gpu_seconds,
                downloaded_bytes: (network_seconds > 0.0).then(|| r.get(10)).transpose()?,
                uploaded_bytes: (network_seconds > 0.0).then(|| r.get(11)).transpose()?,
                network_observed_secs: network_seconds,
                last_seen: r.get(13)?,
            })
        })?;
        data.applications = rows.collect::<Result<_, _>>()?;
        for id in &query.application_ids {
            if data.series.iter().any(|s| s.application.id == *id) {
                continue;
            }
            if let Some(application) = data.applications.iter().find(|a| a.id == *id) {
                data.series.push(ApplicationSeries {
                    application: application.clone(),
                    points: self.points(*id, query)?,
                });
            }
        }
        Ok(data)
    }
    fn points(&self, id: i64, query: &HistoryQuery) -> Result<Vec<HistoryPoint>, rusqlite::Error> {
        let mut statement=self.connection.prepare_cached("SELECT timestamp,seconds,cpu_sum,cpu_peak,ram_sum,ram_peak,gpu_sum,gpu_seconds,down,up,network_seconds FROM buckets WHERE application_id=?1 AND timestamp>=?2 AND timestamp<?3 ORDER BY timestamp LIMIT ?4")?;
        statement
            .query_map(params![id, query.from, query.to, MAX_POINTS as i64], |r| {
                let seconds: f64 = r.get(1)?;
                let gpu_seconds: f64 = r.get(7)?;
                let network_seconds: f64 = r.get(10)?;
                let down: f64 = r.get(8)?;
                let up: f64 = r.get(9)?;
                Ok(HistoryPoint {
                    timestamp: r.get(0)?,
                    duration_secs: seconds,
                    cpu_avg: r.get::<_, f64>(2)? / seconds,
                    cpu_peak: r.get(3)?,
                    ram_avg: r.get::<_, f64>(4)? / seconds,
                    ram_peak: r.get::<_, i64>(5)?.max(0) as u64,
                    gpu_avg: (gpu_seconds > 0.0)
                        .then(|| r.get::<_, f64>(6).map(|v| v / gpu_seconds))
                        .transpose()?,
                    gpu_observed_secs: gpu_seconds,
                    network_observed_secs: network_seconds,
                    down_bps: (network_seconds > 0.0).then_some(down / network_seconds),
                    up_bps: (network_seconds > 0.0).then_some(up / network_seconds),
                    downloaded_bytes: (network_seconds > 0.0).then_some(down),
                    uploaded_bytes: (network_seconds > 0.0).then_some(up),
                })
            })?
            .collect()
    }
    pub fn retain(&mut self, now: i64, days: u32) -> Result<(), String> {
        let cutoff = now.saturating_sub(i64::from(days) * 86400);
        self.connection
            .execute("DELETE FROM buckets WHERE timestamp<?1", [cutoff])
            .map_err(|e| e.to_string())?;
        self.connection.execute("DELETE FROM applications WHERE NOT EXISTS (SELECT 1 FROM buckets WHERE application_id=applications.id)",[]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn clear(&mut self) -> Result<(), String> {
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM buckets", [])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM applications", [])
            .map_err(|e| e.to_string())?;
        tx.execute("UPDATE history_state SET high_water=NULL WHERE id=1", [])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|e| e.to_string())
    }
}
fn save_metric(
    connection: &Connection,
    bucket: &Bucket,
    id: i64,
    m: &Metric,
    last_seen: i64,
) -> Result<(), rusqlite::Error> {
    connection.execute("INSERT INTO buckets VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
        ON CONFLICT(timestamp,application_id) DO UPDATE SET last_seen=max(last_seen,excluded.last_seen),ram_total=excluded.ram_total,
        seconds=seconds+excluded.seconds,cpu_sum=cpu_sum+excluded.cpu_sum,cpu_peak=max(cpu_peak,excluded.cpu_peak),
        ram_sum=ram_sum+excluded.ram_sum,ram_peak=max(ram_peak,excluded.ram_peak),gpu_sum=gpu_sum+excluded.gpu_sum,
        gpu_seconds=gpu_seconds+excluded.gpu_seconds,down=down+excluded.down,up=up+excluded.up,network_seconds=network_seconds+excluded.network_seconds",
        params![bucket.timestamp,id,last_seen,bucket.ram_total.min(i64::MAX as u64) as i64,m.seconds,m.cpu_sum,m.cpu_peak,m.ram_sum,m.ram_peak.min(i64::MAX as u64) as i64,m.gpu_sum,m.gpu_seconds,m.down,m.up,m.network_seconds])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::aggregation::AppMetric;
    use super::*;
    fn bucket(at: i64) -> Bucket {
        let metric = Metric {
            seconds: 10.0,
            cpu_sum: 200.0,
            cpu_peak: 30.0,
            ram_sum: 1000.0,
            ram_peak: 150,
            gpu_sum: 40.0,
            gpu_seconds: 2.0,
            down: 100.0,
            up: 50.0,
            network_seconds: 5.0,
        };
        Bucket {
            timestamp: at,
            last_seen: at + 10,
            observed_until: (at + 10) as f64,
            ram_total: 2000,
            system: metric.clone(),
            applications: [(
                "path:/a".into(),
                AppMetric {
                    name: "a".into(),
                    executable: "/a".into(),
                    last_seen: at + 10,
                    metric,
                },
            )]
            .into(),
        }
    }
    fn query() -> HistoryQuery {
        HistoryQuery {
            request_id: 42,
            from: 0,
            to: 1000,
            application_ids: vec![],
        }
    }

    #[test]
    fn real_database_reopens_with_weighted_summaries_and_selected_series() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite3");
        {
            let mut s = Store::open(&path).unwrap();
            s.save(vec![bucket(30), bucket(60)]).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let data = s.query(&query()).unwrap();
        assert_eq!(data.systems.len(), 2);
        assert_eq!(data.recorded_secs, 20.0);
        assert_eq!(data.ram_total_bytes, 2000);
        let app = &data.applications[0];
        assert_eq!(app.cpu_avg, 20.0);
        assert_eq!(app.cpu_peak, 30.0);
        assert_eq!(app.cpu_seconds, 4.0);
        assert_eq!(app.gpu_avg, Some(20.0));
        assert_eq!(app.downloaded_bytes, Some(200.0));
        assert_eq!(app.network_observed_secs, 10.0);
        assert!(data.series.is_empty());
        let mut q = query();
        q.application_ids = vec![app.id];
        let selected = s.query(&q).unwrap();
        assert_eq!(selected.series[0].points.len(), 2);
        assert_eq!(selected.earliest_timestamp, Some(30));
        assert_eq!(selected.latest_timestamp, Some(70));
    }
    #[test]
    fn retention_prunes_orphans_and_clear_removes_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("h")).unwrap();
        s.save(vec![bucket(30), bucket(90_000)]).unwrap();
        s.retain(90_100, 1).unwrap();
        let mut q = query();
        q.to = 100_000;
        let data = s.query(&q).unwrap();
        assert_eq!(data.systems.len(), 1);
        s.clear().unwrap();
        assert!(s.query(&q).unwrap().applications.is_empty());
        let n: i64 = s
            .connection
            .query_row("SELECT count(*) FROM applications", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
    #[test]
    fn unrelated_and_future_databases_are_rejected_without_adopting_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE unrelated(x); PRAGMA user_version=99;")
            .unwrap();
        drop(conn);
        assert!(Store::open(&path).is_err());
    }
    #[test]
    fn partial_flushes_merge_without_losing_peaks_or_known_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("h")).unwrap();
        s.save(vec![bucket(30)]).unwrap();
        let mut b = bucket(30);
        b.applications
            .get_mut("path:/a")
            .unwrap()
            .metric
            .gpu_seconds = 0.0;
        b.applications.get_mut("path:/a").unwrap().metric.gpu_sum = 0.0;
        s.save(vec![b]).unwrap();
        let d = s.query(&query()).unwrap();
        let a = &d.applications[0];
        assert_eq!(a.observed_secs, 20.0);
        assert_eq!(a.gpu_avg, Some(20.0));
        assert_eq!(a.gpu_observed_secs, 2.0);
    }
    #[test]
    fn failed_writes_roll_back_the_whole_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("h")).unwrap();
        s.connection.execute_batch("PRAGMA query_only=ON;").unwrap();
        assert!(s.save(vec![bucket(30)]).is_err());
        assert!(s.query(&query()).unwrap().systems.is_empty());
    }
    #[test]
    fn failed_clear_rolls_back_instead_of_leaving_a_partially_cleared_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("h")).unwrap();
        s.save(vec![bucket(30)]).unwrap();
        s.connection.execute_batch("CREATE TRIGGER reject_application_delete BEFORE DELETE ON applications BEGIN SELECT RAISE(ABORT,'blocked'); END;").unwrap();
        assert!(s.clear().is_err());
        assert_eq!(s.query(&query()).unwrap().systems.len(), 1);
        assert!(s.connection.is_autocommit());
    }
    #[test]
    fn database_full_preserves_existing_committed_history_and_returns_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(&dir.path().join("h")).unwrap();
        s.save(vec![bucket(30)]).unwrap();
        let pages: i64 = s
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        s.connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        let mut b = bucket(60);
        b.applications.get_mut("path:/a").unwrap().name = "x".repeat(1024 * 1024);
        assert!(s.save(vec![b]).is_err());
        assert_eq!(s.query(&query()).unwrap().systems.len(), 1);
    }
    #[test]
    fn a_second_history_writer_is_rejected_until_the_first_store_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let first = Store::open(&path).unwrap();
        assert!(Store::open(&path).is_err());
        drop(first);
        assert!(Store::open(&path).is_ok());
    }
    fn assert_rollback_never_recovers_committed_time(reopen: bool) {
        use crate::history::aggregation::Aggregator;
        use crate::model::{ProcessRow, Snapshot};
        fn snapshot(at: i64, seconds: f64, total: u64) -> Snapshot {
            Snapshot {
                sampled_at: at,
                sample_seconds: seconds,
                cpu_pct: 20.0,
                ram_used: 100,
                process_rows: vec![ProcessRow {
                    pid: 1,
                    started_at: 1,
                    name: "worker".into(),
                    executable: "/a".into(),
                    cpu_pct: 10.0,
                    ram_bytes: 50,
                    downloaded_bytes: Some(total),
                    uploaded_bytes: Some(total),
                    ..Default::default()
                }],
                ..Default::default()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollback");
        let mut store = Store::open(&path).unwrap();
        let mut aggregator = Aggregator::with_high_water(store.high_water().unwrap());
        for at in 0..=30 {
            aggregator.record(&snapshot(
                at,
                if at == 0 { 0.0 } else { 1.0 },
                at as u64 * 100,
            ));
            if at == 15 {
                store.save(aggregator.take()).unwrap();
            }
        }
        store.save(aggregator.take()).unwrap();
        if reopen {
            drop(store);
            store = Store::open(&path).unwrap();
            aggregator = Aggregator::with_high_water(store.high_water().unwrap());
        }
        aggregator.record(&snapshot(20, 1.0, 4000));
        for at in 21..=30 {
            aggregator.record(&snapshot(at, 1.0, 4000 + (at - 20) as u64 * 100));
        }
        store.save(aggregator.take()).unwrap();
        let data = store.query(&query()).unwrap();
        assert_eq!(data.recorded_secs, 30.0);
        assert_eq!(data.systems[0].duration_secs, 30.0);
        assert_eq!(data.applications[0].downloaded_bytes, Some(3000.0));
        aggregator.record(&snapshot(31, 1.0, 5100));
        store.save(aggregator.take()).unwrap();
        let resumed = store.query(&query()).unwrap();
        assert_eq!(resumed.recorded_secs, 31.0);
        assert_eq!(resumed.applications[0].downloaded_bytes, Some(3100.0));
    }
    #[test]
    fn utc_rollback_never_recounts_already_flushed_seconds_or_traffic() {
        assert_rollback_never_recovers_committed_time(false);
    }
    #[test]
    fn persisted_watermark_rejects_rollback_overlap_after_reopen() {
        assert_rollback_never_recovers_committed_time(true);
    }

    #[test]
    fn fractional_watermark_survives_reopen_without_rounding_into_covered_time() {
        use crate::history::aggregation::Aggregator;
        use crate::model::{ProcessRow, Snapshot};
        fn sample(at: i64, seconds: f64, total: u64) -> Snapshot {
            Snapshot {
                sampled_at: at,
                sample_seconds: seconds,
                process_rows: vec![ProcessRow {
                    pid: 1,
                    started_at: 1,
                    name: "a".into(),
                    executable: "/a".into(),
                    downloaded_bytes: Some(total),
                    uploaded_bytes: Some(total),
                    ..Default::default()
                }],
                ..Default::default()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fractional");
        {
            let mut store = Store::open(&path).unwrap();
            let mut a = Aggregator::default();
            a.record(&sample(0, 0.0, 0));
            for tick in 1..=3 {
                a.record(&sample(tick, 1.1, tick as u64 * 100));
            }
            store.save(a.take()).unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let watermark = store.high_water().unwrap().unwrap();
        assert!((watermark - 3.3).abs() < 1e-10);
        let mut a = Aggregator::with_high_water(Some(watermark));
        a.record(&sample(3, 0.0, 1000));
        a.record(&sample(4, 1.0, 1100));
        a.record(&sample(5, 1.0, 1200));
        store.save(a.take()).unwrap();
        let data = store.query(&query()).unwrap();
        assert!((data.recorded_secs - 4.3).abs() < 1e-10);
        assert!((data.applications[0].downloaded_bytes.unwrap() - 400.0).abs() < 1e-10);
    }
    #[test]
    fn schema_one_migrates_with_a_conservative_boundary_and_preserves_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migration");
        {
            let mut store = Store::open(&path).unwrap();
            store.save(vec![bucket(30)]).unwrap();
            store
                .connection
                .execute_batch("DROP TABLE history_state; PRAGMA user_version=1;")
                .unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.high_water().unwrap(), Some(60.0));
        assert_eq!(store.query(&query()).unwrap().recorded_secs, 10.0);
        store.clear().unwrap();
        assert_eq!(store.high_water().unwrap(), None);
    }
    #[test]
    fn dropping_store_unlocks_even_when_a_duplicate_descriptor_remains_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("duplicated-lock");
        let first = Store::open(&path).unwrap();
        let inherited_descriptor = first._writer_lock.0.try_clone().unwrap();
        drop(first);
        let reopened = Store::open(&path);
        assert!(
            reopened.is_ok(),
            "writer lock survived store ownership: {:?}",
            reopened.err()
        );
        drop(inherited_descriptor);
    }
}

#[cfg(test)]
fn seed_twelve_hours(path: &Path, end: i64, with_gap: bool) -> Result<(), String> {
    use super::aggregation::Aggregator;
    use crate::model::{ProcessRow, Snapshot};
    let mut store = Store::open(path)?;
    let mut aggregator = Aggregator::default();
    let start = end - 12 * 3600;
    for tick in 0..=8640 {
        let elapsed = tick * 5;
        if with_gap && (4 * 3600..4 * 3600 + 1800).contains(&elapsed) {
            continue;
        }
        let mut rows = Vec::new();
        for (pid, name, cpu, ram) in [
            (1, "browser", 20.0, 512_u64 * 1024 * 1024),
            (2, "editor", 10.0, 256 * 1024 * 1024),
            (3, "build-worker", 40.0, 128 * 1024 * 1024),
        ] {
            if pid == 3 && elapsed % (2 * 3600) > 1800 {
                continue;
            }
            rows.push(ProcessRow {
                pid,
                started_at: start as u64,
                name: name.into(),
                executable: format!("/opt/history-fixture/{name}"),
                cpu_pct: cpu,
                ram_bytes: ram,
                gpu_pct: (pid == 1).then_some(15.0),
                downloaded_bytes: (pid == 1).then_some(tick as u64 * 10000),
                uploaded_bytes: (pid == 1).then_some(tick as u64 * 1000),
                ..Default::default()
            });
        }
        aggregator.record(&Snapshot {
            sampled_at: start + elapsed,
            sample_seconds: if tick == 0 { 0.0 } else { 5.0 },
            process_rows: rows,
            cpu_pct: if elapsed % 3600 < 1800 { 35.0 } else { 25.0 },
            ram_used: 2 * 1024 * 1024 * 1024,
            ram_total: 8 * 1024 * 1024 * 1024,
            down_bps: 2000.0,
            up_bps: 200.0,
            interface_count: 1,
            ..Default::default()
        });
        if tick % 60 == 0 {
            store.save(aggregator.take())?;
        }
    }
    store.save(aggregator.take())
}

#[cfg(test)]
mod long_range_tests {
    use super::*;
    #[test]
    fn twelve_hours_survive_restart_and_six_hour_query_has_independent_weights() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("12h.sqlite3");
        let end = 1_800_000_000;
        seed_twelve_hours(&path, end, false).unwrap();
        let store = Store::open(&path).unwrap();
        let mut query = HistoryQuery {
            from: end - 43200,
            to: end,
            application_ids: vec![],
            request_id: 99,
        };
        let data = store.query(&query).unwrap();
        assert_eq!(data.systems.len(), 1440);
        assert_eq!(data.recorded_secs, 43200.0);
        assert_eq!(data.applications.len(), 3);
        let browser = data
            .applications
            .iter()
            .find(|a| a.name == "browser")
            .unwrap();
        assert_eq!(browser.cpu_avg, 20.0);
        assert_eq!(browser.cpu_seconds, 8640.0);
        assert_eq!(browser.downloaded_bytes, Some(86_400_000.0));
        assert_eq!(browser.gpu_avg, Some(15.0));
        query.from = end - 21600;
        query.application_ids = vec![browser.id];
        let half = store.query(&query).unwrap();
        assert_eq!(half.systems.len(), 720);
        assert_eq!(half.recorded_secs, 21600.0);
        assert_eq!(half.series[0].points.len(), 720);
        assert_eq!(
            half.series[0].application.downloaded_bytes,
            Some(43_200_000.0)
        );
    }
    /// Test-only application-code fixture for manual GUI acceptance, never normal startup.
    #[test]
    #[ignore = "writes a controlled 12-hour fixture to PC_ACTIVITY_HISTORY_FIXTURE"]
    fn seed_history_fixture() {
        let path = std::env::var_os("PC_ACTIVITY_HISTORY_FIXTURE")
            .expect("PC_ACTIVITY_HISTORY_FIXTURE is required");
        let path = std::path::PathBuf::from(path);
        super::super::prepare_path(&path).unwrap();
        let end = chrono::Utc::now().timestamp().div_euclid(30) * 30;
        seed_twelve_hours(&path, end, true).unwrap();
    }
}
