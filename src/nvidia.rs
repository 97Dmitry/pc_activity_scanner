use crate::gpu::{GpuUsage, parse_nvidia_pmon};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::Instant;

#[derive(Clone, Default)]
pub struct NvidiaState {
    pub data: HashMap<u32, GpuUsage>,
    pub status: String,
    pub updated_at: Option<Instant>,
}

#[cfg(any(target_os = "linux", test))]
impl NvidiaState {
    fn failed(&mut self, status: String) {
        self.data.clear();
        self.updated_at = None;
        self.status = status;
    }
}

#[cfg(any(target_os = "linux", test))]
fn bind_lifetimes(
    data: HashMap<u32, GpuUsage>,
    before: &HashMap<u32, u64>,
    mut after: impl FnMut(u32) -> Option<u64>,
) -> HashMap<u32, GpuUsage> {
    data.into_iter()
        .take(65_536)
        .filter_map(|(pid, mut value)| {
            let lifetime = *before.get(&pid)?;
            if after(pid) != Some(lifetime) {
                return None;
            }
            value.lifetime = Some(lifetime);
            Some((pid, value))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn current_lifetimes() -> HashMap<u32, u64> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .take(65_536)
        .filter_map(|entry| {
            let pid = entry.ok()?.file_name().to_str()?.parse().ok()?;
            Some((pid, crate::process_details::process_lifetime(pid)?))
        })
        .collect()
}

pub fn start_nvidia_sampler(stopped: Arc<AtomicBool>, state: Arc<RwLock<NvidiaState>>) {
    #[cfg(target_os = "linux")]
    std::thread::spawn(move || {
        let program = std::ffi::OsStr::new("nvidia-smi");
        if let Err(error) = crate::command::run(program, &["-L"], &stopped, Duration::from_secs(3))
        {
            state
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .failed(format!("NVIDIA: {error}"));
            return;
        }
        while !stopped.load(Ordering::Relaxed) {
            let before = current_lifetimes();
            let sample = crate::command::run(
                program,
                &["pmon", "-c", "1", "-s", "um"],
                &stopped,
                Duration::from_secs(3),
            );
            let mut guard = state.write().unwrap_or_else(|e| e.into_inner());
            match sample {
                Ok(output) => {
                    guard.data = bind_lifetimes(
                        parse_nvidia_pmon(&output),
                        &before,
                        crate::process_details::process_lifetime,
                    );
                    guard.status = "NVIDIA pmon".into();
                    guard.updated_at = Some(Instant::now());
                }
                Err(error) => guard.failed(format!("NVIDIA pmon: {error}")),
            }
            drop(guard);
            for _ in 0..25 {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stopped;
        let _ = state;
        let _ = parse_nvidia_pmon("");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pmon_output_is_attributed_only_to_lifetimes_observed_before_and_after() {
        let parsed = parse_nvidia_pmon(
            "# gpu pid type sm mem enc dec fb command\n0 42 G 10 0 0 0 4 app\n0 43 G 20 0 0 0 8 other\n0 44 G 30 0 0 0 16 new\n",
        );
        let before = [(42, 100), (43, 200)].into();
        let bound = bind_lifetimes(parsed, &before, |pid| match pid {
            42 => Some(100),
            43 => Some(201),
            _ => Some(300),
        });
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[&42].lifetime, Some(100));
        assert_eq!(bound[&42].utilization_pct, Some(10.0));
    }
    #[test]
    fn command_failure_invalidates_cached_measurements_immediately() {
        let mut state = NvidiaState {
            data: [(
                42,
                GpuUsage {
                    lifetime: Some(100),
                    utilization_pct: Some(90.0),
                    memory_bytes: Some(1024),
                },
            )]
            .into(),
            status: "old".into(),
            updated_at: Some(Instant::now()),
        };
        state.failed("command failed".into());
        assert!(state.data.is_empty());
        assert_eq!(state.updated_at, None);
        assert_eq!(state.status, "command failed");
    }
}
