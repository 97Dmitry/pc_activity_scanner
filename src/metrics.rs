//! Typed supplementary measurements. Missing keys mean unavailable, never zero.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MetricKind {
    IoRead,
    IoWrite,
    DiskActive,
    GpuDedicated,
    GpuShared,
    PrivateCommit,
    ResidentShared,
    Swap,
    MinorFaults,
    MajorFaults,
    PageFaults,
}
impl MetricKind {
    pub const ALL: [Self; 11] = [
        Self::IoRead,
        Self::IoWrite,
        Self::DiskActive,
        Self::GpuDedicated,
        Self::GpuShared,
        Self::PrivateCommit,
        Self::ResidentShared,
        Self::Swap,
        Self::MinorFaults,
        Self::MajorFaults,
        Self::PageFaults,
    ];
    pub fn unit(self) -> &'static str {
        match self {
            Self::IoRead | Self::IoWrite => "bytes/s",
            Self::DiskActive => "%",
            Self::MinorFaults | Self::MajorFaults | Self::PageFaults => "faults/s",
            _ => "bytes",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::IoRead => "I/O чтение/с",
            Self::IoWrite => "I/O запись/с",
            Self::DiskActive => "Активность накопителя %",
            Self::GpuDedicated => "GPU dedicated",
            Self::GpuShared => "GPU shared",
            Self::PrivateCommit => "Private commit",
            Self::ResidentShared => "File/shmem RSS",
            Self::Swap => "Swap",
            Self::MinorFaults => "Minor faults/с",
            Self::MajorFaults => "Major faults/с",
            Self::PageFaults => "Page faults/с",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::IoRead | Self::IoWrite => {
                "Linux: /proc/PID/io read_bytes/write_bytes, учёт storage I/O. Windows: Process I/O transfer bytes, включает файловые, сетевые и device операции. Не физический трафик конкретного диска."
            }
            Self::DiskActive => {
                "Доля времени занятости устройства. Не сумма процентов разделов и не показатель насыщения параллельного SSD."
            }
            Self::GpuDedicated => {
                "Выделенная GPU-память процесса по WDDM; сумма по адаптерам, может включать разделяемые ресурсы. У GPU Process Memory известны ошибки провайдера. Не уникальная физическая память и не доказательство утечки."
            }
            Self::GpuShared => {
                "Shared GPU memory процесса по WDDM; системная память, доступная GPU, не VRAM."
            }
            Self::PrivateCommit => {
                "Windows: SystemProcessInformation.PrivatePageCount, private commit (соответствует PrivateUsage). Не размер swap/pagefile. Linux: недоступен; VmSize не является commit."
            }
            Self::ResidentShared => {
                "Linux RssFile + RssShmem: file-backed/shared resident, может включать приватные file-backed страницы. Не суммировать как уникальную физическую память."
            }
            Self::Swap => {
                "Linux VmSwap: swap анонимных приватных страниц, без shmem swap. Windows: недоступен отдельно от commit."
            }
            Self::MinorFaults => {
                "Linux minflt: ошибки без I/O, в секунду. Windows: недоступен отдельно."
            }
            Self::PageFaults => {
                "Windows PageFaultCount: все page faults в секунду, включая soft и hard. Не счётчик дискового чтения."
            }
            Self::MajorFaults => {
                "Linux majflt: ошибки с загрузкой страницы, в секунду. Не количество байтов и не доказанная причина задержки."
            }
        }
    }
    pub fn format(self, value: Option<f64>) -> String {
        value.map_or_else(
            || "—".into(),
            |v| match self.unit() {
                "bytes" => crate::model::format_size(v),
                "bytes/s" => crate::model::format_rate(Some(v)),
                "%" => format!("{v:.1}%"),
                _ => format!("{v:.1}/с"),
            },
        )
    }
}
pub type Measurements = BTreeMap<MetricKind, f64>;

#[derive(Clone, Debug, Default)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub metrics: Measurements,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn units_distinguish_throughput_capacity_and_activity() {
        assert_eq!(MetricKind::IoRead.unit(), "bytes/s");
        assert_eq!(MetricKind::GpuShared.unit(), "bytes");
        assert_eq!(MetricKind::DiskActive.unit(), "%");
    }
}
