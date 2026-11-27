use crate::model::{AppSettings, DownloadItem, DownloadPackage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub downloads: Vec<DownloadItem>,
    pub packages: Vec<DownloadPackage>,
}

pub fn portable_root_from_exe(exe_path: &Path) -> Option<PathBuf> {
    let dir = exe_path.parent()?;
    if dir.join("portable.flag").is_file() || dir.join("data").join("settings.json").is_file() {
        Some(dir.to_path_buf())
    } else {
        None
    }
}

pub fn portable_root_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    portable_root_from_exe(&exe)
}

pub fn is_portable_mode() -> bool {
    portable_root_dir().is_some()
}

pub fn app_data_dir() -> PathBuf {
    if let Some(root) = portable_root_dir() {
        return root.join("data");
    }
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("VortexDL")
}

pub fn bin_dir() -> PathBuf {
    if let Some(root) = portable_root_dir() {
        return root.join("data").join("bin");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_root_detects_flag_or_existing_data_settings() {
        let base = std::env::temp_dir().join(format!("vortex_portable_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let fake_exe = base.join("vortex-gui.exe");

        assert!(portable_root_from_exe(&fake_exe).is_none());

        std::fs::write(base.join("portable.flag"), b"").unwrap();
        assert_eq!(portable_root_from_exe(&fake_exe), Some(base.clone()));

        std::fs::remove_file(base.join("portable.flag")).unwrap();
        assert!(portable_root_from_exe(&fake_exe).is_none());

        std::fs::create_dir_all(base.join("data")).unwrap();
        std::fs::write(base.join("data").join("settings.json"), b"{}").unwrap();
        assert_eq!(portable_root_from_exe(&fake_exe), Some(base.clone()));

        let _ = std::fs::remove_dir_all(&base);
    }
}
