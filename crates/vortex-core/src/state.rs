use crate::model::{AppSettings, DownloadItem, DownloadPackage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub downloads: Vec<DownloadItem>,
    pub packages: Vec<DownloadPackage>,
}

pub fn app_data_dir() -> PathBuf {
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("VortexDL")
}

pub fn bin_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("VortexDL").join("bin")
}

pub fn settings_path() -> PathBuf {
    app_data_dir().join("settings.json")
}

pub fn state_path() -> PathBuf {
    app_data_dir().join("state.json")
}

pub fn load_settings() -> AppSettings {
    let path = settings_path();
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(parsed) = serde_json::from_str::<AppSettings>(&raw) {
            return parsed;
        }
    }
    AppSettings::default()
}

pub fn save_settings(settings: &AppSettings) -> Result<()> {
    let dir = app_data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = settings_path();
    let tmp = dir.join("settings.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn load_state() -> PersistedState {
    let path = state_path();
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(mut parsed) = serde_json::from_str::<PersistedState>(&raw) {
            for item in &mut parsed.downloads {
                if item.status.is_active() {
                    item.status = crate::model::DownloadStatus::Paused;
                    item.speed_bps = 0;
                    item.upload_speed_bps = 0;
                    item.eta_seconds = None;
                }
            }
            return parsed;
        }
    }
    PersistedState::default()
}

pub fn save_state(state: &PersistedState) -> Result<()> {
    let dir = app_data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = state_path();
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
