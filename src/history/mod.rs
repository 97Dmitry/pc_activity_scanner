//! Persistent resource history. The worker exclusively owns SQLite and aggregation.
mod aggregation;
mod store;
pub mod types;

use crate::model::Snapshot;
use aggregation::Aggregator;
use directories::ProjectDirs;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, SyncSender},
};
use std::time::{Duration, Instant};
use store::Store;
use types::*;

const SAMPLE_CAPACITY: usize = 64;
const COMMAND_CAPACITY: usize = 16;

struct Sample {
    generation: u64,
    sequence: u64,
    snapshot: Arc<Snapshot>,
}
#[derive(Clone)]
pub struct HistoryRecorder {
    samples: SyncSender<Sample>,
    enabled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    sequence: Arc<AtomicU64>,
}
impl HistoryRecorder {
    /// Never waits on disk, the worker, or a full queue.
    pub fn record(&self, snapshot: Arc<Snapshot>) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let sample = Sample {
            generation: self.generation.load(Ordering::Acquire),
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            snapshot,
        };
        if self.samples.try_send(sample).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
enum Command {
    Query(HistoryQuery),
    Configure(HistoryConfig, u64),
    Clear(u64),
}

pub struct HistoryService {
    recorder: HistoryRecorder,
    commands: SyncSender<Command>,
    view: Arc<Mutex<Arc<HistoryView>>>,
    stop: Arc<AtomicBool>,
    finished: Receiver<()>,
}
impl HistoryService {
    pub fn start(config: HistoryConfig) -> Self {
        let path = ProjectDirs::from("dev", "97Dmitry", "PCActivityScanner")
            .map(|d| d.data_local_dir().join("history.sqlite3"));
        match path {
            Some(path) => Self::start_at(path, config),
            None => Self::launch(None, config),
        }
    }
    pub fn start_at(path: PathBuf, config: HistoryConfig) -> Self {
        Self::launch(Some(path), config)
    }
    fn launch(path: Option<PathBuf>, config: HistoryConfig) -> Self {
        let config = normalize(config);
        let (samples_tx, samples_rx) = mpsc::sync_channel(SAMPLE_CAPACITY);
        let (commands, commands_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let recorder = HistoryRecorder {
            samples: samples_tx,
            enabled: Arc::new(AtomicBool::new(config.enabled)),
            generation: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        let view = Arc::new(Mutex::new(Arc::new(HistoryView {
            status: HistoryStatus {
                enabled: config.enabled,
                retention_days: config.retention_days,
                database_path: path.clone(),
                ..Default::default()
            },
            ..Default::default()
        })));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_view = Arc::clone(&view);
        let worker_stop = Arc::clone(&stop);
        let dropped = Arc::clone(&recorder.dropped);
        let spawn = std::thread::Builder::new()
            .name("activity-history".into())
            .spawn(move || {
                Worker::run(
                    path,
                    config,
                    samples_rx,
                    commands_rx,
                    worker_view,
                    worker_stop,
                    dropped,
                );
                let _ = finished_tx.try_send(());
            });
        if let Err(error) = spawn {
            update(&view, |v| {
                v.status.error = Some(format!("History worker: {error}"))
            });
        }
        Self {
            recorder,
            commands,
            view,
            stop,
            finished,
        }
    }
    pub fn recorder(&self) -> HistoryRecorder {
        self.recorder.clone()
    }
    pub fn request(&self, query: HistoryQuery) -> bool {
        query.to > query.from
            && query.to.saturating_sub(query.from) <= 7 * 86400
            && query.application_ids.len() <= MAX_COMPARISON_APPS
            && self.commands.try_send(Command::Query(query)).is_ok()
    }
    pub fn configure(&self, config: HistoryConfig) -> bool {
        let config = normalize(config);
        let generation = self
            .recorder
            .generation
            .load(Ordering::Acquire)
            .wrapping_add(1);
        if self
            .commands
            .try_send(Command::Configure(config, generation))
            .is_err()
        {
            return false;
        }
        self.recorder
            .generation
            .store(generation, Ordering::Release);
        self.recorder
            .enabled
            .store(config.enabled, Ordering::Release);
        true
    }
    /// Caller is responsible for presenting confirmation before this operation.
    pub fn clear(&self) -> bool {
        let generation = self
            .recorder
            .generation
            .load(Ordering::Acquire)
            .wrapping_add(1);
        if self.commands.try_send(Command::Clear(generation)).is_err() {
            return false;
        }
        self.recorder
            .generation
            .store(generation, Ordering::Release);
        true
    }
    pub fn view(&self) -> Arc<HistoryView> {
        let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = self.recorder.dropped.load(Ordering::Relaxed);
        if view.status.dropped_samples != dropped {
            Arc::make_mut(&mut view).status.dropped_samples = dropped;
        }
        Arc::clone(&view)
    }
}
impl Drop for HistoryService {
    fn drop(&mut self) {
        self.recorder.enabled.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        let _ = self.finished.recv_timeout(Duration::from_millis(750));
    }
}
fn normalize(mut config: HistoryConfig) -> HistoryConfig {
    if ![1, 3, 7].contains(&config.retention_days) {
        config.retention_days = 1;
    }
    config
}
fn update(view: &Mutex<Arc<HistoryView>>, action: impl FnOnce(&mut HistoryView)) {
    let mut guard = view.lock().unwrap_or_else(|e| e.into_inner());
    action(Arc::make_mut(&mut guard));
}
struct Worker {
    store: Store,
    aggregator: Aggregator,
    generation: u64,
    last_sequence: Option<u64>,
    pending: VecDeque<Sample>,
    config: HistoryConfig,
    view: Arc<Mutex<Arc<HistoryView>>>,
    path: PathBuf,
    dropped: Arc<AtomicU64>,
}
impl Worker {
    fn run(
        path: Option<PathBuf>,
        config: HistoryConfig,
        samples: Receiver<Sample>,
        commands: Receiver<Command>,
        view: Arc<Mutex<Arc<HistoryView>>>,
        stop: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
    ) {
        let opened = path
            .as_ref()
            .ok_or_else(|| "Local application data directory is unavailable".to_string())
            .and_then(|p| prepare_path(p).and_then(|()| Store::open(p)));
        let store = match opened {
            Ok(store) => store,
            Err(error) => {
                update(&view, |v| v.status.error = Some(error));
                return;
            }
        };
        let high_water = match store.high_water() {
            Ok(value) => value,
            Err(error) => {
                update(&view, |v| v.status.error = Some(error));
                return;
            }
        };
        let mut worker = Self {
            store,
            aggregator: Aggregator::with_high_water(high_water),
            generation: 0,
            last_sequence: None,
            pending: VecDeque::new(),
            config,
            view,
            path: path.unwrap(),
            dropped,
        };
        worker.maintenance();
        update(&worker.view, |v| v.status.ready = true);
        let mut flushed = Instant::now();
        let mut maintained = Instant::now();
        while !stop.load(Ordering::Acquire) {
            for _ in 0..COMMAND_CAPACITY {
                let Ok(command) = commands.try_recv() else {
                    break;
                };
                // A query includes all samples already queued before its request.
                worker.drain(&samples);
                worker.command(command);
            }
            match samples.recv_timeout(Duration::from_millis(50)) {
                Ok(sample) => worker.sample(sample),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if flushed.elapsed() >= Duration::from_secs(30) {
                worker.flush();
                flushed = Instant::now();
            }
            if maintained.elapsed() >= Duration::from_secs(60) {
                worker.maintenance();
                maintained = Instant::now();
            }
        }
        // Apply accepted state changes at shutdown as well; a queued clear must
        // not be undone by the final partial-bucket flush.
        for _ in 0..COMMAND_CAPACITY {
            match commands.try_recv() {
                Ok(Command::Query(_)) => {}
                Ok(command) => {
                    worker.drain(&samples);
                    worker.command(command);
                }
                Err(_) => break,
            }
        }
        worker.drain(&samples);
        worker.flush();
    }
    fn drain(&mut self, samples: &Receiver<Sample>) {
        for _ in 0..SAMPLE_CAPACITY {
            match samples.try_recv() {
                Ok(sample) => self.sample(sample),
                Err(_) => break,
            }
        }
    }
    fn sample(&mut self, sample: Sample) {
        // Samples and commands have separate bounded queues. A producer may
        // publish the new generation before its command is serviced.
        if sample.generation > self.generation {
            if self.pending.len() < SAMPLE_CAPACITY {
                self.pending.push_back(sample);
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        if self.config.enabled && sample.generation == self.generation {
            if self
                .last_sequence
                .is_some_and(|last| sample.sequence != last.wrapping_add(1))
            {
                self.aggregator.break_continuity();
            }
            self.last_sequence = Some(sample.sequence);
            let overlapping = self.aggregator.record(&sample.snapshot);
            const CLOCK_WARNING: &str = "History UTC boundary overlap: recording resumes after the previously recorded time";
            let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
            if overlapping && view.status.error.is_none() {
                Arc::make_mut(&mut view).status.error = Some(CLOCK_WARNING.into());
            } else if !overlapping && view.status.error.as_deref() == Some(CLOCK_WARNING) {
                Arc::make_mut(&mut view).status.error = None;
            }
        }
    }
    fn command(&mut self, command: Command) {
        match command {
            Command::Query(query) => {
                self.flush();
                match self.store.query(&query) {
                    Ok(data) => update(&self.view, |v| {
                        v.request_id = query.request_id;
                        v.data = data;
                    }),
                    Err(error) => update(&self.view, |v| {
                        v.request_id = query.request_id;
                        v.data = HistoryData::default();
                        v.status.error = Some(error);
                    }),
                }
            }
            Command::Configure(config, generation) => {
                self.flush();
                self.aggregator.reset();
                self.config = config;
                self.generation = generation;
                self.last_sequence = None;
                self.replay_pending();
                self.maintenance();
                update(&self.view, |v| {
                    v.status.enabled = config.enabled;
                    v.status.retention_days = config.retention_days;
                });
            }
            Command::Clear(generation) => {
                self.aggregator.reset();
                self.generation = generation;
                self.last_sequence = None;
                match self.store.clear() {
                    Ok(()) => {
                        self.aggregator = Aggregator::default();
                        update(&self.view, |v| {
                            v.data = HistoryData::default();
                            v.status.error = None;
                        });
                    }
                    Err(error) => self.error(error),
                }
                self.replay_pending();
            }
        }
    }
    fn replay_pending(&mut self) {
        for sample in std::mem::take(&mut self.pending) {
            self.sample(sample);
        }
    }
    fn flush(&mut self) {
        if let Err(error) = self.store.save(self.aggregator.take()) {
            self.error(error);
        }
        let bytes = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        let dropped = self.dropped.load(Ordering::Relaxed);
        update(&self.view, |v| {
            v.status.database_bytes = bytes;
            v.status.dropped_samples = dropped;
        });
    }
    fn maintenance(&mut self) {
        if let Err(error) = self
            .store
            .retain(chrono::Utc::now().timestamp(), self.config.retention_days)
        {
            self.error(error);
        }
    }
    fn error(&self, error: String) {
        update(&self.view, |v| v.status.error = Some(error));
    }
}
fn prepare_path(path: &std::path::Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "History database requires a parent directory".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent).map_err(|e| e.to_string())?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcessRow;
    use std::time::{Duration, Instant};

    fn wait(service: &HistoryService, id: u64) -> Arc<HistoryView> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let view = service.view();
            if view.status.ready && view.request_id == id {
                return view;
            }
            assert!(
                Instant::now() < until,
                "worker response timed out: {:?}",
                view.status
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn sample(at: i64, seconds: f64) -> Arc<Snapshot> {
        Arc::new(Snapshot {
            sampled_at: at,
            sample_seconds: seconds,
            cpu_pct: 20.0,
            ram_used: 100,
            ram_total: 1000,
            process_rows: vec![ProcessRow {
                pid: 1,
                started_at: 1,
                name: "a".into(),
                executable: "/a".into(),
                cpu_pct: 10.0,
                ram_bytes: 50,
                ..Default::default()
            }],
            ..Default::default()
        })
    }
    fn request(service: &HistoryService, id: u64, now: i64) -> Arc<HistoryView> {
        assert!(service.request(HistoryQuery {
            request_id: id,
            from: now - 60,
            to: now + 60,
            application_ids: vec![]
        }));
        wait(service, id)
    }
    #[test]
    fn service_queries_restarts_disabled_recording_and_confirmed_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        let now = chrono::Utc::now().timestamp();
        {
            let service = HistoryService::start_at(path.clone(), HistoryConfig::default());
            let recorder = service.recorder();
            recorder.record(sample(now - 2, 0.0));
            recorder.record(sample(now, 2.0));
            let first = request(&service, 42, now);
            assert_eq!(first.data.recorded_secs, 2.0);
            assert!(service.configure(HistoryConfig {
                enabled: false,
                retention_days: 3
            }));
            recorder.record(sample(now + 2, 2.0));
            let disabled = request(&service, 43, now);
            assert!(!disabled.status.enabled);
            assert_eq!(disabled.data.recorded_secs, 2.0);
        }
        let service = HistoryService::start_at(
            path,
            HistoryConfig {
                enabled: false,
                retention_days: 1,
            },
        );
        let reopened = request(&service, 44, now);
        assert_eq!(reopened.data.recorded_secs, 2.0);
        assert!(service.clear());
        let cleared = request(&service, 45, now);
        assert!(cleared.data.systems.is_empty());
        assert!(cleared.data.applications.is_empty());
    }
    #[test]
    fn invalid_queries_are_rejected_before_queueing() {
        let dir = tempfile::tempdir().unwrap();
        let s = HistoryService::start_at(dir.path().join("h"), HistoryConfig::default());
        assert!(!s.request(HistoryQuery {
            from: 10,
            to: 0,
            ..Default::default()
        }));
        assert!(!s.request(HistoryQuery {
            from: 0,
            to: 8 * 86400,
            ..Default::default()
        }));
        assert!(!s.request(HistoryQuery {
            from: 0,
            to: 10,
            application_ids: vec![1, 2, 3, 4, 5],
            ..Default::default()
        }));
    }
    #[test]
    fn bounded_recorder_reports_overflow_without_waiting() {
        let (tx, _rx) = mpsc::sync_channel(2);
        let recorder = HistoryRecorder {
            samples: tx,
            enabled: Arc::new(AtomicBool::new(true)),
            generation: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        for _ in 0..100 {
            recorder.record(sample(1, 1.0));
        }
        assert_eq!(recorder.dropped.load(Ordering::Relaxed), 98);
    }
    #[test]
    fn a_dropped_sample_breaks_counter_and_duration_continuity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let mut worker = Worker {
            store: Store::open(&path).unwrap(),
            aggregator: Aggregator::default(),
            generation: 0,
            last_sequence: None,
            pending: VecDeque::new(),
            config: HistoryConfig::default(),
            view: Arc::new(Mutex::new(Arc::new(HistoryView::default()))),
            path,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        worker.sample(Sample {
            generation: 0,
            sequence: 0,
            snapshot: sample(1, 0.0),
        });
        worker.sample(Sample {
            generation: 0,
            sequence: 2,
            snapshot: sample(3, 1.0),
        });
        worker.sample(Sample {
            generation: 0,
            sequence: 3,
            snapshot: sample(4, 1.0),
        });
        let buckets = worker.aggregator.take();
        assert_eq!(buckets[0].system.seconds, 1.0);
    }
    #[cfg(unix)]
    #[test]
    fn newly_created_history_files_are_private_without_chmod_of_existing_parents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new-private/history.sqlite3");
        prepare_path(&path).unwrap();
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn normal_service_shutdown_flushes_the_partial_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let now = chrono::Utc::now().timestamp();
        {
            let service = HistoryService::start_at(path.clone(), HistoryConfig::default());
            let recorder = service.recorder();
            recorder.record(sample(now - 2, 0.0));
            recorder.record(sample(now, 2.0));
        }
        let store = Store::open(&path).unwrap();
        let d = store
            .query(&HistoryQuery {
                from: now - 60,
                to: now + 60,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(d.recorded_secs, 2.0);
    }
    #[test]
    fn samples_from_after_clear_are_deferred_until_the_command_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let mut worker = Worker {
            store: Store::open(&path).unwrap(),
            aggregator: Aggregator::default(),
            generation: 0,
            last_sequence: None,
            pending: VecDeque::new(),
            config: HistoryConfig::default(),
            view: Arc::new(Mutex::new(Arc::new(HistoryView::default()))),
            path,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        worker.sample(Sample {
            generation: 1,
            sequence: 0,
            snapshot: sample(10, 0.0),
        });
        worker.sample(Sample {
            generation: 1,
            sequence: 1,
            snapshot: sample(11, 1.0),
        });
        worker.command(Command::Clear(1));
        let buckets = worker.aggregator.take();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].system.seconds, 1.0);
    }
    #[test]
    fn clear_queued_immediately_before_shutdown_is_applied_before_final_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let now = chrono::Utc::now().timestamp();
        {
            let service = HistoryService::start_at(path.clone(), HistoryConfig::default());
            let recorder = service.recorder();
            recorder.record(sample(now - 2, 0.0));
            recorder.record(sample(now, 2.0));
            assert_eq!(request(&service, 1, now).data.recorded_secs, 2.0);
            assert!(service.clear());
        }
        let store = Store::open(&path).unwrap();
        assert!(
            store
                .query(&HistoryQuery {
                    from: now - 60,
                    to: now + 60,
                    ..Default::default()
                })
                .unwrap()
                .systems
                .is_empty()
        );
    }
    #[test]
    fn shutdown_drains_old_generation_samples_before_disabling_or_reconfiguring() {
        for enabled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("shutdown");
            let now = chrono::Utc::now().timestamp();
            let (samples_tx, samples) = mpsc::sync_channel(SAMPLE_CAPACITY);
            let (commands_tx, commands) = mpsc::sync_channel(COMMAND_CAPACITY);
            for (sequence, at, seconds) in [(0, now - 3, 0.0), (1, now - 2, 1.0), (2, now - 1, 1.0)]
            {
                samples_tx
                    .try_send(Sample {
                        generation: 0,
                        sequence,
                        snapshot: sample(at, seconds),
                    })
                    .unwrap();
            }
            commands_tx
                .try_send(Command::Configure(
                    HistoryConfig {
                        enabled,
                        retention_days: 3,
                    },
                    1,
                ))
                .unwrap();
            Worker::run(
                Some(path.clone()),
                HistoryConfig::default(),
                samples,
                commands,
                Arc::new(Mutex::new(Arc::new(HistoryView::default()))),
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicU64::new(0)),
            );
            let store = Store::open(&path).unwrap();
            let data = store
                .query(&HistoryQuery {
                    from: now - 60,
                    to: now + 60,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                data.recorded_secs, 2.0,
                "queued samples lost with enabled={enabled}"
            );
            assert_eq!(data.applications[0].cpu_seconds, 0.2);
        }
    }
}
