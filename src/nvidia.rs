use crate::gpu::{GpuUsage, parse_nvidia_pmon};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[derive(Clone, Default)]
pub struct NvidiaState {
    pub data: HashMap<u32, GpuUsage>,
    pub status: String,
    pub updated_at: Option<Instant>,
}

pub fn start_nvidia_sampler(
    stopped: Arc<AtomicBool>,
    state: Arc<RwLock<NvidiaState>>,
) {
    #[cfg(target_os = "linux")]
    std::thread::spawn(move || {
        use std::process::Command;
        if Command::new("nvidia-smi").arg("-L").output().is_err() {
            return;
        }
        while !stopped.load(Ordering::Relaxed) {
            let sample = Command::new("nvidia-smi")
                .args(["pmon", "-c", "1", "-s", "um"])
                .output();
            let mut guard = state.write().unwrap_or_else(|e| e.into_inner());
            match sample {
                Ok(output) if output.status.success() => {
                    guard.data = parse_nvidia_pmon(&String::from_utf8_lossy(&output.stdout));
                    guard.status = "NVIDIA pmon".into();
                    guard.updated_at = Some(Instant::now());
                }
                Ok(output) => {
                    guard.status = format!("NVIDIA pmon недоступен (код {:?})", output.status.code());
                }
                Err(error) => guard.status = format!("NVIDIA pmon: {error}"),
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
