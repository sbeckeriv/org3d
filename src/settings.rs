use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    pub base_folder: String,
    pub port: u16,
    pub db_path: String,
    pub thumb_dir: String,
    pub preferred_slicer: Option<String>,
    pub rescan_on_startup: bool,
}

impl Settings {
    pub fn config_dir() -> PathBuf {
        // Explicit override, used by the Docker image (/config on appdata).
        if let Some(dir) = std::env::var_os("ORG3D_CONFIG_DIR").filter(|d| !d.is_empty()) {
            return PathBuf::from(dir);
        }
        #[cfg(target_os = "macos")]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join("Library/Application Support/org3d")
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                    PathBuf::from(home).join(".config")
                })
                .join("org3d")
        }
    }

    pub fn config_file() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::config_file()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", Self::config_file().display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)?;
        let s = toml::to_string_pretty(self)?;
        // Write-then-rename so a crash mid-write can't leave a truncated config.
        let tmp = dir.join("config.toml.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(&tmp, Self::config_file())?;
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.base_folder.is_empty()
    }
}
