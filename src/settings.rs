use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub close_to_tray: bool,
    pub poll_interval_ms: u64,
    pub enable_linux_traffic: bool,
    pub enable_windows_traffic: bool,
    pub network_setup_complete: bool,
    pub history_enabled: bool,
    pub history_retention_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            poll_interval_ms: 2000,
            enable_linux_traffic: false,
            enable_windows_traffic: false,
            network_setup_complete: false,
            history_enabled: true,
            history_retention_days: 1,
        }
    }
}

impl Settings {
    pub fn network_enabled(&self) -> bool {
        (cfg!(target_os = "linux") && self.enable_linux_traffic)
            || (cfg!(target_os = "windows") && self.enable_windows_traffic)
    }

    pub fn should_offer_network_setup(&self) -> bool {
        !self.network_setup_complete && !self.network_enabled()
    }

    pub fn choose_network(&mut self, enabled: bool) {
        self.network_setup_complete = true;
        if cfg!(target_os = "linux") {
            self.enable_linux_traffic = enabled;
        }
        if cfg!(target_os = "windows") {
            self.enable_windows_traffic = enabled;
        }
    }

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
        if ![1, 3, 7].contains(&self.history_retention_days) {
            self.history_retention_days = 1;
        }
        self
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = Self::file_path().ok_or("Не удалось определить каталог настроек")?;
        self.save_to(&path)?;
        Ok(path)
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or("Settings path has no parent directory")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let contents = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        // Keep the old file intact until replacement succeeds, on Windows too.
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        tmp.write_all(&contents).map_err(|e| e.to_string())?;
        tmp.as_file().sync_all().map_err(|e| e.to_string())?;
        tmp.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_disabled_network_offers_setup_without_requesting_privileges() {
        let config: Settings = serde_json::from_str(
            r#"{"enable_linux_traffic":false,"enable_windows_traffic":false}"#,
        )
        .unwrap();
        assert!(config.should_offer_network_setup());
        assert!(!config.network_enabled());
    }

    #[test]
    fn explicit_off_survives_reload_and_does_not_offer_setup() {
        let mut config = Settings::default();
        config.choose_network(false);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert!(!restored.network_enabled());
        assert!(!restored.should_offer_network_setup());
    }

    #[test]
    fn network_consent_persists_for_retry_and_future_launches() {
        let mut config = Settings::default();
        config.choose_network(true);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert!(restored.network_enabled());
        assert!(!restored.should_offer_network_setup());
    }

    #[test]
    fn existing_settings_enable_one_day_history_by_default() {
        let old_json =
            r#"{"close_to_tray":false,"poll_interval_ms":5000,"enable_linux_traffic":true}"#;
        let config: Settings = serde_json::from_str(old_json).unwrap();
        let saved = serde_json::to_value(config.validated()).unwrap();
        assert_eq!(saved["enable_windows_traffic"], false);
        assert_eq!(saved["history_enabled"], true);
        assert_eq!(saved["history_retention_days"], 1);
        assert_eq!(saved["poll_interval_ms"], 5000);
    }

    #[test]
    fn history_preferences_round_trip_without_enabling_recording() {
        let config: Settings =
            serde_json::from_str(r#"{"history_enabled":false,"history_retention_days":7}"#)
                .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        config.validated().save_to(&path).unwrap();
        let loaded: Settings = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let saved = serde_json::to_value(loaded.validated()).unwrap();
        assert_eq!(saved["history_enabled"], false);
        assert_eq!(saved["history_retention_days"], 7);
    }

    #[test]
    fn unsupported_history_retention_falls_back_to_one_day() {
        for (requested, expected) in [(0, 1), (1, 1), (2, 1), (3, 3), (7, 7), (365, 1)] {
            let config: Settings = serde_json::from_value(serde_json::json!({
                "history_retention_days": requested,
            }))
            .unwrap();
            let saved = serde_json::to_value(config.validated()).unwrap();
            assert_eq!(
                saved["history_retention_days"], expected,
                "requested {requested}"
            );
        }
    }

    #[test]
    fn invalid_interval_does_not_create_busy_loop() {
        let config = Settings {
            poll_interval_ms: 0,
            ..Settings::default()
        }
        .validated();
        assert_eq!(config.poll_interval_ms, 2000);
    }

    #[test]
    fn saving_does_not_overwrite_an_unrelated_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let unrelated = directory.path().join("settings.json.tmp");
        fs::write(&unrelated, b"do not overwrite").unwrap();
        Settings::default().save_to(&path).unwrap();
        assert_eq!(fs::read(&unrelated).unwrap(), b"do not overwrite");
    }

    #[test]
    fn repeated_save_replaces_existing_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        Settings::default().save_to(&path).unwrap();
        Settings {
            poll_interval_ms: 5000,
            close_to_tray: false,
            ..Settings::default()
        }
        .save_to(&path)
        .unwrap();
        let saved: Settings = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.poll_interval_ms, 5000);
        assert!(!saved.close_to_tray);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_replacement_keeps_existing_contents_and_cleans_temp_file() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("settings.json");
        fs::create_dir(&destination).unwrap();
        let existing = destination.join("keep.json");
        fs::write(&existing, b"existing settings").unwrap();
        assert!(Settings::default().save_to(&destination).is_err());
        assert_eq!(fs::read(existing).unwrap(), b"existing settings");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
