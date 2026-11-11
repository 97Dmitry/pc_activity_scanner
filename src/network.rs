use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkRate {
    pub upload_bps: f64,
    pub download_bps: f64,
}

#[derive(Clone, Debug)]
pub struct NetworkSample {
    pub epoch: u64,
    pub sequence: u64,
    pub is_available: bool,
    pub status: String,
    pub rates: HashMap<u32, NetworkRate>,
    /// The requested NetHogs sampling period, used for session traffic estimates.
    pub seconds_per_sample: f64,
}

impl Default for NetworkSample {
    fn default() -> Self {
        Self {
            epoch: 0,
            sequence: 0,
            is_available: false,
            status: "Учёт трафика по процессам отключён".to_string(),
            rates: HashMap::new(),
            seconds_per_sample: 2.0,
        }
    }
}

pub struct NetworkBackend {
    state: Arc<Mutex<NetworkSample>>,
    epoch: u64,
    #[cfg(target_os = "linux")]
    session: Option<LinuxSession>,
}

impl Default for NetworkBackend {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(NetworkSample::default())),
            epoch: 0,
            #[cfg(target_os = "linux")]
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
            self.epoch = self.epoch.wrapping_add(1);
            {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                *guard = NetworkSample { epoch: self.epoch, ..NetworkSample::default() };
            }
            let session = LinuxSession::start(Arc::clone(&self.state))?;
            self.session = Some(session);
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.status = "Для этой ОС пока нет счётчика трафика по PID".into();
            Err(state.status.clone())
        }
    }

    pub fn disable(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut session) = self.session.take() {
            session.stop();
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = NetworkSample { epoch: self.epoch, ..NetworkSample::default() };
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
    if !upload_kib.is_finite() || !download_kib.is_finite()
        || upload_kib < 0.0 || download_kib < 0.0 {
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

#[cfg(target_os = "linux")]
struct LinuxSession {
    process: Arc<Mutex<std::process::Child>>,
    reader: Option<std::thread::JoinHandle<()>>,
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
        let mut command = if is_root {
            Command::new(binary)
        } else {
            let mut command = Command::new("pkexec");
            command.arg(binary);
            command
        };
        let mut child = command
            .args(["-t", "-d", "2"])
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
                    snapshot.status = format!("NetHogs: {message}");
                }
            }
        });
        let reader = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut frame = HashMap::new();
            let mut saw_first_frame = false;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let normalized = line.trim();
                if normalized.contains("Refreshing:") {
                    if saw_first_frame {
                        let mut snapshot = state.lock().unwrap_or_else(|e| e.into_inner());
                        snapshot.sequence = snapshot.sequence.wrapping_add(1);
                        snapshot.rates = std::mem::take(&mut frame);
                        snapshot.is_available = true;
                        snapshot.status = "NetHogs · получаем данные по PID".into();
                    }
                    saw_first_frame = true;
                } else if let Some((pid, rate)) = parse_nethogs_line(normalized) {
                    // NetHogs may report one PID more than once (connections);
                    // add the rates rather than losing them.
                    let current: &mut NetworkRate = frame.entry(pid).or_default();
                    current.upload_bps += rate.upload_bps;
                    current.download_bps += rate.download_bps;
                }
            }
            let mut snapshot = state.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.is_available = false;
            snapshot.rates.clear();
            if !snapshot.status.starts_with("NetHogs:") {
                snapshot.status = "NetHogs завершился. Выключите и включите сбор трафика.".into();
            }
        });
        Ok(Self {
            process: Arc::new(Mutex::new(child)),
            reader: Some(reader),
        })
    }

    fn stop(&mut self) {
        if let Ok(mut child) = self.process.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(super::parse_nethogs_line("/home/a/My App/8421/1000 1.5 2.5").unwrap().0, 8421);
        assert!(super::parse_nethogs_line("app/5/1000 -3 1").is_none());
        assert!(super::parse_nethogs_line("app/5/1000 NaN 1").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ignores_unknown_process() {
        assert!(super::parse_nethogs_line("unknown TCP/0/0 1.0 1.0").is_none());
    }
}
