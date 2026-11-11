use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "windows", test))]
const MAX_PENDING_FRAMES: usize = 256;

#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkRate {
    pub upload_bps: f64,
    pub download_bps: f64,
}

#[derive(Clone, Debug)]
pub struct NetworkFrame {
    pub epoch: u64,
    pub sequence: u64,
    pub rates: HashMap<u32, NetworkRate>,
    pub seconds_per_sample: f64,
    pub captured_at: u64,
    pub process_lifetimes: HashMap<u32, u64>,
    pub exact_bytes: HashMap<u32, (u64, u64)>,
}

#[derive(Clone, Debug)]
pub struct NetworkSample {
    pub epoch: u64,
    #[cfg(any(target_os = "linux", target_os = "windows", test))]
    pub sequence: u64,
    pub is_available: bool,
    pub status: String,
    pub rates: HashMap<u32, NetworkRate>,
    #[cfg(target_os = "windows")]
    pub process_lifetimes: HashMap<u32, u64>,
    pub last_frame_at: Option<Instant>,
    pending: VecDeque<NetworkFrame>,
    dropped_frames: u64,
}

impl Default for NetworkSample {
    fn default() -> Self {
        Self {
            epoch: 0,
            #[cfg(any(target_os = "linux", target_os = "windows", test))]
            sequence: 0,
            is_available: false,
            status: "Учёт трафика по процессам отключён".to_string(),
            rates: HashMap::new(),
            #[cfg(target_os = "windows")]
            process_lifetimes: HashMap::new(),
            last_frame_at: None,
            pending: VecDeque::new(),
            dropped_frames: 0,
        }
    }
}

impl NetworkSample {
    fn reset_session(&mut self) {
        let epoch = self.epoch.wrapping_add(1);
        let pending = self.take_frames();
        let dropped_frames = self.dropped_frames;
        *self = Self {
            epoch,
            pending,
            dropped_frames,
            ..Self::default()
        };
    }

    pub fn status(&self) -> String {
        if self.dropped_frames == 0 {
            self.status.clone()
        } else {
            format!(
                "{} · пропущено кадров: {} (длительная пауза)",
                self.status, self.dropped_frames
            )
        }
    }

    pub fn take_frames(&mut self) -> VecDeque<NetworkFrame> {
        std::mem::take(&mut self.pending)
    }

    #[cfg(any(target_os = "linux", target_os = "windows", test))]
    #[cfg(any(target_os = "linux", test))]
    fn publish_frame(&mut self, rates: HashMap<u32, NetworkRate>, captured_at: u64) {
        self.publish_timed_frame(rates, captured_at, 2.0, "NetHogs · получаем данные по PID");
    }

    #[cfg(any(target_os = "linux", target_os = "windows", test))]
    fn publish_timed_frame(
        &mut self,
        rates: HashMap<u32, NetworkRate>,
        captured_at: u64,
        seconds: f64,
        status: &str,
    ) {
        self.sequence = self.sequence.wrapping_add(1);
        if self.pending.len() == MAX_PENDING_FRAMES {
            self.pending.pop_front();
            self.dropped_frames = self.dropped_frames.saturating_add(1);
        }
        self.pending.push_back(NetworkFrame {
            epoch: self.epoch,
            sequence: self.sequence,
            rates: rates.clone(),
            seconds_per_sample: seconds,
            captured_at,
            process_lifetimes: HashMap::new(),
            exact_bytes: HashMap::new(),
        });
        self.rates = rates;
        self.is_available = true;
        self.last_frame_at = Some(Instant::now());
        self.status = status.into();
    }

    /// Suppress stale per-process rates if the backend stopped publishing.
    pub fn expire_if_stale(&mut self, max_age: Duration) {
        if self.is_available
            && self
                .last_frame_at
                .is_none_or(|last| last.elapsed() > max_age)
        {
            self.is_available = false;
            self.rates.clear();
            self.status = "Нет новых данных трафика, последняя скорость устарела".into();
        }
    }
}

pub struct NetworkBackend {
    state: Arc<Mutex<NetworkSample>>,
    #[cfg(target_os = "linux")]
    session: Option<LinuxSession>,
    #[cfg(target_os = "windows")]
    session: Option<windows::WindowsSession>,
}

impl Default for NetworkBackend {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(NetworkSample::default())),
            #[cfg(target_os = "linux")]
            session: None,
            #[cfg(target_os = "windows")]
            session: None,
        }
    }
}

impl NetworkBackend {
    pub fn shared(&self) -> Arc<Mutex<NetworkSample>> {
        Arc::clone(&self.state)
    }

    pub fn enable(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            if self.session.is_some() {
                return Ok(());
            }
            {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                guard.reset_session();
            }
            let session = LinuxSession::start(Arc::clone(&self.state))?;
            self.session = Some(session);
            Ok(())
        }
        #[cfg(target_os = "windows")]
        {
            if self.session.is_some() {
                return Ok(());
            }
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.reset_session();
                state.status = "Windows ETW: ожидание подтверждения UAC".into();
            }
            self.session = Some(windows::WindowsSession::start(Arc::clone(&self.state))?);
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.status = "Для этой ОС пока нет счётчика трафика по PID".into();
            Err(state.status.clone())
        }
    }

    pub fn disable(&mut self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.reset_session();
        }
        #[cfg(target_os = "windows")]
        if let Some(mut session) = self.session.take() {
            session.stop();
        }
        #[cfg(target_os = "linux")]
        if let Some(mut session) = self.session.take() {
            std::thread::spawn(move || session.stop());
        }
    }
}

impl Drop for NetworkBackend {
    fn drop(&mut self) {
        self.disable();
    }
}

/// Parse one process line from NetHogs' trace mode. Traffic values are KiB/s.
/// A program name can include slash characters; PID and UID are parsed from the right.
#[cfg(target_os = "linux")]
pub fn parse_nethogs_line(line: &str) -> Option<(u32, NetworkRate)> {
    let columns = line.split_whitespace().collect::<Vec<_>>();
    if columns.len() < 3 {
        return None;
    }
    // Names may contain spaces; numeric rate columns are always at the end.
    let program_pid_uid = columns[..columns.len() - 2].join(" ");
    let mut suffixes = program_pid_uid.rsplitn(3, '/');
    let _uid = suffixes.next()?.parse::<u32>().ok()?;
    let pid = suffixes.next()?.parse::<u32>().ok()?;
    if pid == 0 || suffixes.next().is_none() {
        return None;
    }
    let upload_kib = columns[columns.len() - 2].parse::<f64>().ok()?;
    let download_kib = columns[columns.len() - 1].parse::<f64>().ok()?;
    if !upload_kib.is_finite()
        || !download_kib.is_finite()
        || upload_kib < 0.0
        || download_kib < 0.0
    {
        return None;
    }
    Some((
        pid,
        NetworkRate {
            upload_bps: upload_kib.max(0.0) * 1024.0,
            download_bps: download_kib.max(0.0) * 1024.0,
        },
    ))
}

/// An elevated helper must be root-owned and have no writable ancestors.
#[cfg(target_os = "linux")]
fn trusted_helper_binary(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return false;
    };
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return false;
    }
    for parent in path.ancestors().skip(1) {
        let Ok(metadata) = std::fs::metadata(parent) else {
            return false;
        };
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return false;
        }
    }
    true
}

/// Run only NetHogs while elevated, and stop it when the UI closes stdin.
#[cfg(target_os = "linux")]
pub fn run_nethogs_helper() -> i32 {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let is_root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0");
    if !is_root {
        eprintln!("The NetHogs helper requires root authorization.");
        return 77;
    }
    if !std::env::current_exe()
        .ok()
        .is_some_and(|path| trusted_helper_binary(&path))
    {
        eprintln!("Unsafe helper path: install a root-owned binary first.");
        return 78;
    }
    let Some(binary) = ["/usr/sbin/nethogs", "/usr/bin/nethogs"]
        .into_iter()
        .find(|path| std::path::Path::new(path).is_file())
    else {
        eprintln!("NetHogs is not installed.");
        return 127;
    };
    let mut child = match Command::new(binary)
        .args(["-t", "-d", "2"])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("Unable to start NetHogs: {err}");
            return 1;
        }
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let stopped = Arc::new(AtomicBool::new(false));
    let stdin_closed = Arc::clone(&stopped);
    // A separate thread ensures the helper also exits when NetHogs crashes,
    // even if the unprivileged UI keeps stdin open.
    let _stdin_watcher = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut bytes = [0u8; 64];
        loop {
            match stdin.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        stdin_closed.store(true, Ordering::Relaxed);
    });
    while !stopped.load(Ordering::Relaxed) {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) => {}
            Err(err) => {
                eprintln!("NetHogs state error: {err}");
                let _ = child.kill();
                let _ = child.wait();
                return 1;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    0
}

#[cfg(target_os = "linux")]
struct LinuxSession {
    process: Arc<Mutex<std::process::Child>>,
    reader: Option<std::thread::JoinHandle<()>>,
    elevated: bool,
}

#[cfg(target_os = "linux")]
impl LinuxSession {
    fn start(state: Arc<Mutex<NetworkSample>>) -> Result<Self, String> {
        use std::process::{Command, Stdio};
        let binary = ["/usr/sbin/nethogs", "/usr/bin/nethogs"]
            .iter()
            .find(|path| std::path::Path::new(path).is_file())
            .ok_or("NetHogs не установлен. Установите пакет nethogs.")?;

        let is_root = Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false);
        let elevated = !is_root;
        let mut command = if is_root {
            let mut direct = Command::new(binary);
            direct.args(["-t", "-d", "2"]);
            direct
        } else {
            let exe = std::env::current_exe()
                .map_err(|err| format!("Не удалось найти исполняемый файл: {err}"))?;
            if !trusted_helper_binary(&exe) {
                return Err("Для NetHogs установите программу через scripts/install-linux.sh и запустите /usr/local/bin/pc-activity-scanner".into());
            }
            let mut helper = Command::new("pkexec");
            helper.arg(exe).arg("--nethogs-helper");
            helper
        };
        let mut child = command
            .stdin(if elevated {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                format!("Не удалось запустить NetHogs/pkexec: {err}. Проверьте polkit.")
            })?;
        let stdout = child.stdout.take().ok_or("NetHogs: нет stdout")?;
        let stderr = child.stderr.take().ok_or("NetHogs: нет stderr")?;
        {
            let mut snapshot = state.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.status = "Ожидание авторизации / первых данных NetHogs…".into();
            snapshot.is_available = false;
        }
        let epoch = state.lock().unwrap_or_else(|e| e.into_inner()).epoch;
        let state_for_errors = Arc::clone(&state);
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for message in BufReader::new(stderr).lines().map_while(Result::ok) {
                let lowered = message.to_lowercase();
                if lowered.contains("error")
                    || lowered.contains("denied")
                    || lowered.contains("authorization")
                    || lowered.contains("permission")
                    || lowered.contains("not found")
                {
                    let mut snapshot = state_for_errors.lock().unwrap_or_else(|e| e.into_inner());
                    if snapshot.epoch != epoch {
                        break;
                    }
                    snapshot.status = format!("NetHogs: {message}");
                }
            }
        });
        let reader =
            std::thread::spawn(move || read_trace(std::io::BufReader::new(stdout), state, epoch));
        Ok(Self {
            process: Arc::new(Mutex::new(child)),
            reader: Some(reader),
            elevated,
        })
    }

    fn stop(&mut self) {
        use std::time::Duration;

        let mut finished = false;
        if let Ok(mut child) = self.process.lock() {
            if self.elevated {
                drop(child.stdin.take());
                // No indefinite wait if Polkit authorization is still pending.
                for _ in 0..30 {
                    if child.try_wait().ok().flatten().is_some() {
                        finished = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                // Cancel a still-pending authorization process when possible.
                if !finished && child.kill().is_ok() {
                    finished = child.wait().is_ok();
                }
            } else {
                let _ = child.kill();
                finished = child.wait().is_ok();
            }
        }
        if finished && let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn read_trace(output: impl std::io::BufRead, state: Arc<Mutex<NetworkSample>>, epoch: u64) {
    let mut frame = HashMap::new();
    let mut saw_first_frame = false;
    let mut captured_at = 0;
    for line in output.lines() {
        let Ok(line) = line else { break };
        let normalized = line.trim();
        if normalized.contains("Refreshing:") {
            if saw_first_frame {
                let mut snapshot = state.lock().unwrap_or_else(|e| e.into_inner());
                if snapshot.epoch != epoch {
                    return;
                }
                snapshot.publish_frame(std::mem::take(&mut frame), captured_at);
            }
            saw_first_frame = true;
            captured_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        } else if let Some((pid, rate)) = parse_nethogs_line(normalized) {
            // NetHogs may report one PID more than once (connections);
            // add the rates rather than losing them.
            let current: &mut NetworkRate = frame.entry(pid).or_default();
            current.upload_bps += rate.upload_bps;
            current.download_bps += rate.download_bps;
        }
    }
    let mut snapshot = state.lock().unwrap_or_else(|e| e.into_inner());
    if snapshot.epoch != epoch {
        return;
    }
    snapshot.is_available = false;
    snapshot.rates.clear();
    if !snapshot.status.starts_with("NetHogs:") {
        snapshot.status = "NetHogs завершился. Выключите и включите сбор трафика.".into();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn old_reader_cannot_publish_or_clear_a_new_session() {
        use super::*;
        let sample = NetworkSample {
            epoch: 2,
            is_available: true,
            status: "new session".into(),
            ..NetworkSample::default()
        };
        let state = Arc::new(Mutex::new(sample));
        let output = std::io::Cursor::new(b"Refreshing:\napp/42/1000 1 2\nRefreshing:\n");
        read_trace(output, Arc::clone(&state), 1);
        let sample = state.lock().unwrap();
        assert_eq!(sample.status, "new session");
        assert!(sample.is_available);
        assert!(sample.pending.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disabling_capture_does_not_wait_on_the_ui_thread() {
        use super::*;
        use std::process::{Command, Stdio};
        let child = Command::new("sh")
            .args(["-c", "sleep 1; cat >/dev/null"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let process = Arc::new(Mutex::new(child));
        let mut backend = NetworkBackend::default();
        backend.session = Some(LinuxSession {
            process: Arc::clone(&process),
            reader: None,
            elevated: true,
        });
        let start = Instant::now();
        backend.disable();
        assert!(start.elapsed() < Duration::from_millis(500));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if process.lock().unwrap().try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "capture child was not reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn slower_collectors_receive_every_completed_frame_once() {
        use super::*;
        let mut sample = NetworkSample::default();
        for value in [10.0, 20.0, 30.0] {
            sample.publish_frame(
                HashMap::from([(
                    42,
                    NetworkRate {
                        upload_bps: value,
                        download_bps: 0.0,
                    },
                )]),
                1000,
            );
        }
        let frames = sample.take_frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames
                .iter()
                .map(|f| f.rates[&42].upload_bps * f.seconds_per_sample)
                .sum::<f64>(),
            120.0
        );
        assert!(sample.take_frames().is_empty());
    }

    #[test]
    fn prolonged_pause_bounds_the_queue_and_reports_lost_frames() {
        use super::*;
        let mut sample = NetworkSample::default();
        for _ in 0..MAX_PENDING_FRAMES + 2 {
            sample.publish_frame(HashMap::new(), 1000);
        }
        let frames = sample.take_frames();
        assert_eq!(frames.len(), MAX_PENDING_FRAMES);
        assert_eq!(frames.front().unwrap().sequence, 3);
        assert_eq!(sample.dropped_frames, 2);
        assert!(sample.status().contains("2"));
    }

    #[test]
    fn stale_network_frames_are_not_shown_as_live() {
        use super::{NetworkRate, NetworkSample};
        use std::time::{Duration, Instant};

        let mut sample = NetworkSample {
            is_available: true,
            last_frame_at: Some(Instant::now() - Duration::from_secs(20)),
            ..NetworkSample::default()
        };
        sample.rates.insert(
            42,
            NetworkRate {
                upload_bps: 100.0,
                download_bps: 200.0,
            },
        );
        sample.expire_if_stale(Duration::from_secs(6));
        assert!(!sample.is_available);
        assert!(sample.rates.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_nethogs_trace_with_path() {
        let line = "/usr/lib/firefox/firefox/8421/1000 12.25 3.5";
        let (pid, sample) = super::parse_nethogs_line(line).unwrap();
        assert_eq!(pid, 8421);
        assert_eq!(sample.upload_bps, 12.25 * 1024.0);
        assert_eq!(sample.download_bps, 3.5 * 1024.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_command_with_spaces_and_rejects_invalid_rates() {
        assert_eq!(
            super::parse_nethogs_line("/home/a/My App/8421/1000 1.5 2.5")
                .unwrap()
                .0,
            8421
        );
        assert!(super::parse_nethogs_line("app/5/1000 -3 1").is_none());
        assert!(super::parse_nethogs_line("app/5/1000 NaN 1").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_binary_path_is_rejected_for_privileged_capture() {
        let path = std::env::temp_dir().join(format!("scanner-untrusted-{}", std::process::id()));
        std::fs::write(&path, b"unsafe").unwrap();
        assert!(!super::trusted_helper_binary(&path));
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn closing_capture_pipe_reaps_child() {
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut session = super::LinuxSession {
            process: Arc::new(Mutex::new(child)),
            reader: None,
            elevated: true,
        };
        session.stop();
        let mut process = session.process.lock().unwrap();
        assert!(process.try_wait().unwrap().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ignores_unknown_process() {
        assert!(super::parse_nethogs_line("unknown TCP/0/0 1.0 1.0").is_none());
    }
}

#[cfg(any(target_os = "windows", test))]
mod windows_protocol;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::process_creation_time;
#[cfg(target_os = "windows")]
pub use windows::run_helper_if_requested as run_windows_helper_if_requested;
#[cfg(target_os = "windows")]
pub use windows::run_smoke_test as run_windows_network_smoke_test;
