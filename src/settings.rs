use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub close_to_tray: bool,
    pub poll_interval_ms: u64,
    pub enable_linux_traffic: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            poll_interval_ms: 2000,
            enable_linux_traffic: false,
        }
    }
}

impl Settings {
    pub fn file_path() -> Option<PathBuf> {
        ProjectDirs::from("dev", "97Dmitry", "PCActivityScanner")
            .map(|dirs| dirs.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        Self::file_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            .unwrap_or_default()
            .validated()
    }

    pub fn validated(mut self) -> Self {
        if ![1000, 2000, 5000].contains(&self.poll_interval_ms) {
            self.poll_interval_ms = 2000;
        }
        self
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = Self::file_path().ok_or("Не удалось определить каталог настроек")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let contents = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, contents).map_err(|e| e.to_string())?;
        // Windows cannot atomically rename onto an existing destination.
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_interval_does_not_create_busy_loop() {
        let config = Settings {
            poll_interval_ms: 0,
            ..Settings::default()
        }
        .validated();
        assert_eq!(config.poll_interval_ms, 2000);
    }
}
