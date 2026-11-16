//! Read-only temperature sources and explicit thermal-throttling evidence.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const MAX_READINGS: usize = 256;

#[derive(Clone, Debug, Default)]
pub struct ThermalReading {
    pub id: String,
    pub label: String,
    pub celsius: Option<f64>,
    pub throttled: Option<bool>,
    pub thermal_events: Option<u64>,
    pub source: String,
}
#[derive(Clone, Debug, Default)]
pub struct ThermalSnapshot {
    pub readings: Vec<ThermalReading>,
    pub status: String,
    pub updated_at: Option<Instant>,
}
pub struct ThermalSampler {
    state: Arc<RwLock<ThermalSnapshot>>,
}
impl ThermalSampler {
    pub fn start(stop: Arc<AtomicBool>) -> Self {
        let state = Arc::new(RwLock::new(ThermalSnapshot::default()));
        let output = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut source = Source::default();
            while !stop.load(Ordering::Relaxed) {
                let next = Instant::now() + Duration::from_secs(5);
                let snapshot = source.sample(&stop);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                *output.write().unwrap_or_else(|error| error.into_inner()) = snapshot;
                while Instant::now() < next && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(
                        Duration::from_millis(100)
                            .min(next.saturating_duration_since(Instant::now())),
                    );
                }
            }
        });
        Self { state }
    }
    pub fn snapshot(&self) -> ThermalSnapshot {
        self.state
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}
fn celsius(value: f64) -> Option<f64> {
    (value.is_finite() && value > -273.15 && value <= 1000.0).then_some(value)
}
#[cfg(any(target_os = "windows", test))]
fn kelvin(value: f64) -> Option<f64> {
    celsius(value - 273.15)
}
fn nvidia_rows(text: &str, flags: bool) -> Result<Vec<ThermalReading>, String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for record in reader.records().take(MAX_READINGS + 1) {
        let record = record.map_err(|_| "Invalid NVIDIA sensor CSV")?;
        if record.len() != if flags { 5 } else { 3 } {
            return Err("Unexpected NVIDIA sensor columns".into());
        }
        let uuid = &record[0];
        if uuid.len() > 128
            || !uuid.starts_with("GPU-")
            || !uuid
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || value == b'-')
        {
            return Err("NVIDIA sensor UUID unavailable".into());
        }
        if !seen.insert(uuid.to_owned()) {
            continue;
        }
        if rows.len() == MAX_READINGS {
            break;
        }
        let flag = |text: &str| match text.to_ascii_lowercase().as_str() {
            "active" => Some(true),
            "not active" => Some(false),
            _ => None,
        };
        let throttled = if flags {
            match (flag(&record[3]), flag(&record[4])) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        } else {
            None
        };
        rows.push(ThermalReading {
            id: format!("nvidia:{uuid}"),
            label: format!("GPU {}", record[1].chars().take(256).collect::<String>()),
            celsius: record[2].parse::<f64>().ok().and_then(celsius),
            throttled,
            thermal_events: None,
            source: if flags {
                "NVIDIA temperature.gpu + explicit SW/HW thermal slowdown"
            } else {
                "NVIDIA temperature.gpu; thermal flags unavailable"
            }
            .into(),
        });
    }
    Ok(rows)
}
#[derive(Default)]
struct Batch {
    readings: Vec<ThermalReading>,
    notes: Vec<String>,
}
#[derive(Default)]
struct NvidiaSource;
impl NvidiaSource {
    fn program() -> Option<std::path::PathBuf> {
        #[cfg(target_os = "linux")]
        let candidates = [
            std::path::PathBuf::from("/usr/bin/nvidia-smi"),
            std::path::PathBuf::from("/usr/lib/wsl/lib/nvidia-smi"),
        ];
        #[cfg(target_os = "windows")]
        let candidates = [std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("nvidia-smi.exe")];
        candidates.into_iter().find(|path| path.is_file())
    }
    fn sample(&self, stop: &AtomicBool) -> Batch {
        let Some(program) = Self::program() else {
            return Batch {notes:vec!["NVIDIA: штатный источник не установлен; этот источник не поддерживает GPU других производителей".into()],..Default::default()};
        };
        const FULL: &str = "--query-gpu=uuid,name,temperature.gpu,clocks_event_reasons.sw_thermal_slowdown,clocks_event_reasons.hw_thermal_slowdown";
        const TEMP: &str = "--query-gpu=uuid,name,temperature.gpu";
        let query = |flags| {
            crate::command::run(
                program.as_os_str(),
                &[
                    if flags { FULL } else { TEMP },
                    "--format=csv,noheader,nounits",
                ],
                stop,
                Duration::from_secs(3),
            )
            .and_then(|text| nvidia_rows(&text, flags))
        };
        match query(true) {
            Ok(readings) => Batch {readings,notes:Vec::new()},
            Err(error) if !stop.load(Ordering::Relaxed) => match query(false) {
                Ok(readings) => Batch {readings,notes:vec!["NVIDIA: thermal flags недоступны; источник предоставляет только температуру".into()]},
                Err(fallback) => Batch {notes:vec![format!("NVIDIA источник недоступен: {error}; {fallback}")],..Default::default()},
            },
            Err(error) => Batch {notes:vec![format!("NVIDIA источник недоступен: {error}")],..Default::default()},
        }
    }
}
#[derive(Default)]
struct Source {
    platform: platform::Platform,
    nvidia: NvidiaSource,
}
impl Source {
    fn sample(&mut self, stop: &AtomicBool) -> ThermalSnapshot {
        let mut result = self.platform.sample();
        let mut nvidia = self.nvidia.sample(stop);
        result.readings.append(&mut nvidia.readings);
        result.notes.append(&mut nvidia.notes);
        if result.readings.len() >= MAX_READINGS {
            result
                .notes
                .push("Достигнут лимит 256 датчиков; список может быть неполным".into());
        }
        result.readings.truncate(MAX_READINGS);
        let measured = result
            .readings
            .iter()
            .filter(|reading| {
                reading.celsius.is_some()
                    || reading.throttled.is_some()
                    || reading.thermal_events.is_some()
            })
            .count();
        if measured == 0 {
            let message = if result.readings.is_empty() {
                "Источники не предоставили датчиков: источник недоступен или оборудование не поддерживается"
            } else {
                "Датчики/счётчики обнаружены; показания недоступны либо ожидают следующий непрерывный замер"
            };
            result.notes.insert(0, message.into());
        }
        let status = format!(
            "Доступных датчиков/счётчиков: {measured}/{}. {}",
            result.readings.len(),
            result
                .notes
                .into_iter()
                .take(16)
                .map(|note| note.chars().take(256).collect::<String>())
                .collect::<Vec<_>>()
                .join("; ")
        );
        ThermalSnapshot {
            readings: result.readings,
            status,
            updated_at: Some(Instant::now()),
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct EventCounter {
    id: String,
    label: String,
    path: String,
    value: u64,
}
#[cfg(target_os = "linux")]
#[derive(Default)]
struct EventLedger {
    previous: std::collections::HashMap<String, (String, u64)>,
    at: Option<Instant>,
}
#[cfg(target_os = "linux")]
impl EventLedger {
    fn record(&mut self, counters: Vec<EventCounter>, now: Instant) -> Vec<ThermalReading> {
        let previous = std::mem::take(&mut self.previous);
        let continuous = self.at.is_some_and(|at| {
            let elapsed = now.saturating_duration_since(at).as_secs_f64();
            elapsed > 0.0 && elapsed <= 15.0
        });
        self.at = Some(now);
        counters
            .into_iter()
            .take(MAX_READINGS)
            .map(|counter| {
                let events = previous
                    .get(&counter.id)
                    .filter(|(path, _)| continuous && path == &counter.path)
                    .and_then(|(_, old)| counter.value.checked_sub(*old));
                self.previous
                    .insert(counter.id.clone(), (counter.path.clone(), counter.value));
                ThermalReading {
                    id: counter.id,
                    label: counter.label,
                    celsius: None,
                    throttled: None,
                    thermal_events: events,
                    source: counter.path,
                }
            })
            .collect()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    #[derive(Default)]
    pub(super) struct Platform {
        query: Option<crate::pdh::Query>,
    }
    impl Platform {
        pub(super) fn sample(&mut self) -> Batch {
            let value = (|| {
                if self.query.is_none() {
                    self.query = Some(crate::pdh::Query::new(&[
                        r"\Thermal Zone Information(*)\Temperature",
                    ])?);
                }
                let query = self.query.as_mut().expect("initialized thermal query");
                query.collect()?;
                query.values(0)
            })();
            match value {
                Ok(values)=>Batch {readings:values.into_iter().take(MAX_READINGS).map(|(name,value)|ThermalReading {
                    id:format!("acpi:{name}"),label:format!("ACPI zone {name}"),celsius:kelvin(value),throttled:None,thermal_events:None,
                    source:"Windows Thermal Zone Information Temperature (Kelvin); это не идентифицированный CPU-датчик".into(),
                }).collect(),notes:vec!["Температура CPU Windows не имеет универсального источника; ACPI-зона не означает CPU. CPU throttling этим источником не определяется".into()]},
                Err(error)=>{self.query=None;Batch {notes:vec![format!("Windows ACPI/PDH источник недоступен: {error}; универсальный CPU-датчик не предоставляется")],..Default::default()}},
            }
        }
    }
}
#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::path::{Path, PathBuf};
    const MAX_SCAN: usize = 4096;
    #[derive(Default)]
    pub(super) struct Platform {
        events: EventLedger,
    }
    fn text(path: impl AsRef<Path>) -> Result<String, String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path.as_ref())
            .map_err(|error| error.to_string())?
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 4096 {
            return Err("sysfs field exceeds limit".into());
        }
        String::from_utf8(bytes)
            .map(|text| text.trim().into())
            .map_err(|_| "Invalid sysfs text".into())
    }
    fn entries(path: &Path, notes: &mut Vec<String>) -> Vec<PathBuf> {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut output = Vec::new();
                for (index, entry) in entries.enumerate() {
                    if index == MAX_SCAN {
                        notes.push(format!("sysfs scan limit: {}", path.display()));
                        break;
                    }
                    if let Ok(entry) = entry {
                        output.push(entry.path());
                    }
                }
                output.sort();
                output
            }
            Err(error) => {
                notes.push(format!("{}: {error}", path.display()));
                Vec::new()
            }
        }
    }
    fn hwmon(root: &Path, batch: &mut Batch) -> Vec<PathBuf> {
        let mut valid_devices = Vec::new();
        for device in entries(root, &mut batch.notes) {
            let name = text(device.join("name")).unwrap_or_else(|_| {
                device
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            let canonical = std::fs::canonicalize(&device).unwrap_or_else(|_| device.clone());
            for input in entries(&device, &mut batch.notes) {
                let Some(stem) = input
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix("_input"))
                    .filter(|name| {
                        name.strip_prefix("temp").is_some_and(|number| {
                            !number.is_empty() && number.bytes().all(|value| value.is_ascii_digit())
                        })
                    })
                else {
                    continue;
                };
                if batch.readings.len() == MAX_READINGS {
                    batch.notes.push("Достигнут лимит hwmon датчиков".into());
                    return valid_devices;
                }
                let label =
                    text(device.join(format!("{stem}_label"))).unwrap_or_else(|_| stem.into());
                let analog =
                    text(device.join(format!("{stem}_type"))).is_ok_and(|value| value == "4");
                let value = if analog {
                    batch.notes.push(format!(
                        "{name}/{stem}: analog thermistor units require calibration; unavailable"
                    ));
                    None
                } else {
                    text(&input)
                        .ok()
                        .and_then(|value| value.parse::<i64>().ok())
                        .and_then(|value| celsius(value as f64 / 1000.0))
                };
                if value.is_some() {
                    valid_devices.push(canonical.clone());
                }
                batch.readings.push(ThermalReading {
                    id: format!("hwmon:{}/{stem}", canonical.display()),
                    label: format!("{name}: {label}"),
                    celsius: value,
                    throttled: None,
                    thermal_events: None,
                    source: input.display().to_string(),
                });
            }
        }
        valid_devices
    }
    fn zones(root: &Path, hwmon: &[PathBuf], batch: &mut Batch) {
        for zone in entries(root, &mut batch.notes) {
            if !zone
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("thermal_zone"))
            {
                continue;
            }
            let canonical = std::fs::canonicalize(&zone).unwrap_or_else(|_| zone.clone());
            if hwmon.iter().any(|device| device.starts_with(&canonical)) {
                continue;
            }
            if batch.readings.len() == MAX_READINGS {
                batch
                    .notes
                    .push("Достигнут лимит thermal zone датчиков".into());
                return;
            }
            let name = text(zone.join("type")).unwrap_or_else(|_| {
                zone.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            let value = text(zone.join("temp"))
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(|value| celsius(value as f64 / 1000.0));
            batch.readings.push(ThermalReading {
                id: format!("thermal:{}", canonical.display()),
                label: format!("Thermal zone {name}"),
                celsius: value,
                throttled: None,
                thermal_events: None,
                source: zone.join("temp").display().to_string(),
            });
        }
    }
    fn counters(root: &Path, batch: &mut Batch) -> Vec<EventCounter> {
        let mut cpus = entries(root, &mut batch.notes)
            .into_iter()
            .filter_map(|path| {
                let number = path
                    .file_name()?
                    .to_str()?
                    .strip_prefix("cpu")?
                    .parse::<u32>()
                    .ok()?;
                Some((number, path))
            })
            .collect::<Vec<_>>();
        cpus.sort_by_key(|(cpu, _)| *cpu);
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for (_, cpu) in cpus {
            let Some(package) = text(cpu.join("topology/physical_package_id"))
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            let core = text(cpu.join("topology/core_id"))
                .ok()
                .and_then(|value| value.parse::<u32>().ok());
            for (id, label, file) in [
                Some((
                    format!("package:{package}"),
                    format!("CPU package {package}: новые thermal-переходы"),
                    "package_throttle_count",
                )),
                core.map(|core| {
                    (
                        format!("core:{package}:{core}"),
                        format!("CPU package {package} core {core}: новые thermal-переходы"),
                        "core_throttle_count",
                    )
                }),
            ]
            .into_iter()
            .flatten()
            {
                if seen.contains(&id) {
                    continue;
                }
                let path = cpu.join("thermal_throttle").join(file);
                let Some(value) = text(&path).ok().and_then(|value| value.parse::<u64>().ok())
                else {
                    continue;
                };
                seen.insert(id.clone());
                if result.len() == MAX_READINGS {
                    batch
                        .notes
                        .push("Достигнут лимит thermal event counters".into());
                    return result;
                }
                result.push(EventCounter {
                    id,
                    label,
                    path: path.display().to_string(),
                    value,
                });
            }
        }
        result
    }
    impl Platform {
        pub(super) fn sample(&mut self) -> Batch {
            self.sample_paths(
                Path::new("/sys/class/hwmon"),
                Path::new("/sys/class/thermal"),
                Path::new("/sys/devices/system/cpu"),
                Instant::now(),
            )
        }
        fn sample_paths(
            &mut self,
            hwmon_root: &Path,
            zones_root: &Path,
            cpu_root: &Path,
            now: Instant,
        ) -> Batch {
            let mut batch = Batch::default();
            let devices = hwmon(hwmon_root, &mut batch);
            zones(zones_root, &devices, &mut batch);
            let counters = counters(cpu_root, &mut batch);
            if counters.is_empty() {
                batch.notes.push("Intel thermal counters: источник не предоставлен ядром/оборудованием; это не подтверждение отсутствия троттлинга".into());
            }
            batch.readings.extend(self.events.record(counters, now));
            if batch.readings.iter().all(|row| row.celsius.is_none()) {
                batch.notes.push("hwmon/thermal: доступные температурные значения не предоставлены (в том числе возможное ограничение VM/WSL)".into());
            }
            batch
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        fn write(path: &Path, value: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        }
        #[test]
        fn hwmon_units_mirrored_zones_and_cpu_topology_are_handled_without_double_counts() {
            let dir = tempfile::tempdir().unwrap();
            let hwmon_root = dir.path().join("hwmon");
            let zones_root = dir.path().join("thermal");
            let cpu_root = dir.path().join("cpu");
            let zone = zones_root.join("thermal_zone0");
            let sensor = zone.join("hwmon0");
            write(&sensor.join("name"), "acpitz");
            write(&sensor.join("temp1_input"), "42500");
            write(&sensor.join("temp1_label"), "Board sensor");
            write(&sensor.join("temp2_input"), "1700");
            write(&sensor.join("temp2_type"), "4");
            write(&zone.join("type"), "acpitz");
            write(&zone.join("temp"), "42500");
            std::fs::create_dir_all(&hwmon_root).unwrap();
            std::os::unix::fs::symlink(&sensor, hwmon_root.join("hwmon0")).unwrap();
            for cpu in 0..2 {
                let root = cpu_root.join(format!("cpu{cpu}"));
                write(&root.join("topology/physical_package_id"), "0");
                write(&root.join("topology/core_id"), "0");
                write(&root.join("thermal_throttle/core_throttle_count"), "10");
                write(&root.join("thermal_throttle/package_throttle_count"), "20");
            }
            let mut source = Platform::default();
            let now = Instant::now();
            let first = source.sample_paths(&hwmon_root, &zones_root, &cpu_root, now);
            assert_eq!(
                first.readings.len(),
                4,
                "two hwmon channels + deduplicated core/package"
            );
            assert_eq!(
                first
                    .readings
                    .iter()
                    .filter(|row| row.celsius == Some(42.5))
                    .count(),
                1
            );
            assert!(
                first
                    .readings
                    .iter()
                    .any(|row| row.label == "acpitz: Board sensor")
            );
            assert!(
                first
                    .readings
                    .iter()
                    .any(|row| row.label == "acpitz: temp2" && row.celsius.is_none())
            );
            assert!(
                first
                    .notes
                    .iter()
                    .any(|note| note.contains("analog thermistor"))
            );
            assert!(
                first
                    .readings
                    .iter()
                    .all(|row| row.thermal_events.is_none() && row.throttled.is_none())
            );
            for cpu in 0..2 {
                write(
                    &cpu_root.join(format!("cpu{cpu}/thermal_throttle/package_throttle_count")),
                    "21",
                );
            }
            let next = source.sample_paths(
                &hwmon_root,
                &zones_root,
                &cpu_root,
                now + Duration::from_secs(5),
            );
            assert_eq!(
                next.readings
                    .iter()
                    .find(|row| row.id == "package:0")
                    .unwrap()
                    .thermal_events,
                Some(1)
            );
            assert_eq!(
                next.readings
                    .iter()
                    .find(|row| row.id == "core:0:0")
                    .unwrap()
                    .thermal_events,
                Some(0)
            );
            std::fs::remove_file(sensor.join("temp1_input")).unwrap();
            std::fs::remove_file(zone.join("temp")).unwrap();
            let failed = source.sample_paths(
                &hwmon_root,
                &zones_root,
                &cpu_root,
                now + Duration::from_secs(10),
            );
            assert!(
                failed.readings.iter().all(|row| row.celsius.is_none()),
                "failed sensors kept stale temperature"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_thermal_sources_and_worker_report_real_values_or_explicit_unavailability() {
        let stopped = Arc::new(AtomicBool::new(false));
        let sampler = ThermalSampler::start(Arc::clone(&stopped));
        let deadline = Instant::now() + Duration::from_secs(8);
        let snapshot = loop {
            let snapshot = sampler.snapshot();
            if snapshot.updated_at.is_some() {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "Thermal worker did not publish bounded result"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        stopped.store(true, Ordering::Relaxed);
        assert!(snapshot.readings.len() <= MAX_READINGS);
        assert!(!snapshot.status.is_empty());
        assert!(snapshot.readings.iter().all(|row| {
            row.celsius
                .is_none_or(|value| value.is_finite() && value > -273.15)
        }));
        eprintln!(
            "Native thermal sources: {}\n{:?}",
            snapshot.status, snapshot.readings
        );
        if std::env::var_os("PC_ACTIVITY_REQUIRE_NVIDIA_THERMAL").is_some() {
            assert!(
                snapshot
                    .readings
                    .iter()
                    .any(|row| row.id.starts_with("nvidia:")
                        && row.celsius.is_some()
                        && row.throttled.is_some()),
                "NVIDIA thermal source required"
            );
        }
        #[cfg(target_os = "windows")]
        if std::env::var_os("PC_ACTIVITY_REQUIRE_WINDOWS_ACPI").is_some() {
            assert!(
                snapshot
                    .readings
                    .iter()
                    .any(|row| row.id.starts_with("acpi:")
                        && row.celsius.is_some()
                        && row.throttled.is_none()),
                "ACPI zone source required"
            );
        }
    }

    #[test]
    fn nvidia_explicit_flags_are_independent_of_temperature_and_unknowns() {
        let rows=nvidia_rows("GPU-abc, GPU One, 95, Not Active, Not Active\nGPU-def, GPU Two, N/A, Active, [Not Supported]\nGPU-ghi, GPU Three, 45, Not Active, N/A\n",true).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].celsius, Some(95.0));
        assert_eq!(rows[0].throttled, Some(false));
        assert_eq!(rows[1].celsius, None);
        assert_eq!(rows[1].throttled, Some(true));
        assert_eq!(rows[2].throttled, None);
        let fallback = nvidia_rows("GPU-abc, GPU One, 55\n", false).unwrap();
        assert_eq!(fallback[0].celsius, Some(55.0));
        assert_eq!(fallback[0].throttled, None);
        assert!(nvidia_rows("unexpected driver output", true).is_err());
    }
    #[test]
    fn temperature_units_and_invalid_values_do_not_become_zero() {
        assert_eq!(celsius(45.0), Some(45.0));
        assert!((kelvin(339.0).unwrap() - 65.85).abs() < 0.001);
        assert_eq!(kelvin(0.0), None);
        assert_eq!(celsius(f64::NAN), None);
        assert_eq!(celsius(f64::INFINITY), None);
        assert_eq!(celsius(-999.0), None);
    }
    #[test]
    fn nvidia_rows_are_bounded_and_deduplicated_by_uuid() {
        let duplicate = "GPU-abc, One, 45\nGPU-abc, One, 45\n";
        assert_eq!(nvidia_rows(duplicate, false).unwrap().len(), 1);
        let input = (0..300)
            .map(|index| format!("GPU-{index}, GPU, 40\n"))
            .collect::<String>();
        assert_eq!(nvidia_rows(&input, false).unwrap().len(), MAX_READINGS);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn thermal_event_counters_never_claim_current_throttling_and_break_on_resets_gaps() {
        let mut ledger = EventLedger::default();
        let now = Instant::now();
        let counter = |value, path: &str| {
            vec![EventCounter {
                id: "package:0".into(),
                label: "Package 0".into(),
                path: path.into(),
                value,
            }]
        };
        assert_eq!(
            ledger.record(counter(10, "cpu0"), now)[0].thermal_events,
            None
        );
        let next = ledger.record(counter(12, "cpu0"), now + Duration::from_secs(5));
        assert_eq!(next[0].thermal_events, Some(2));
        assert_eq!(next[0].throttled, None);
        assert_eq!(
            ledger.record(counter(12, "cpu0"), now + Duration::from_secs(10))[0].thermal_events,
            Some(0)
        );
        assert_eq!(
            ledger.record(counter(1, "cpu0"), now + Duration::from_secs(15))[0].thermal_events,
            None
        );
        assert_eq!(
            ledger.record(counter(10, "cpu1"), now + Duration::from_secs(20))[0].thermal_events,
            None
        );
        assert_eq!(
            ledger.record(counter(20, "cpu1"), now + Duration::from_secs(40))[0].thermal_events,
            None
        );
        ledger.record(Vec::new(), now + Duration::from_secs(45));
        assert_eq!(
            ledger.record(counter(21, "cpu1"), now + Duration::from_secs(50))[0].thermal_events,
            None
        );
    }
}
