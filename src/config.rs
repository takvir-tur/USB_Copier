use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use log::{info, warn};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub destination_path: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        let base_dir = UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
            .or_else(|| UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            destination_path: base_dir.join("USB_Backups"),
        }
    }
}

impl AppConfig {
    /// Returns the OS-appropriate config file path inside AppData (Windows) or Application Support (macOS).
    pub fn config_file_path() -> Result<PathBuf> {
        let proj_dirs = ProjectDirs::from("com", "usbbackup", "USBBackup")
            .context("Failed to resolve OS application support directory")?;
        let config_dir = proj_dirs.config_dir();
        if !config_dir.exists() {
            fs::create_dir_all(config_dir).with_context(|| {
                format!("Failed to create config directory: {}", config_dir.display())
            })?;
        }
        Ok(config_dir.join("config.json"))
    }

    /// Loads the configuration from disk, or initializes and saves the default config if missing.
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(cfg) => {
                if let Err(err) = fs::create_dir_all(&cfg.destination_path) {
                    warn!(
                        "Could not pre-create backup destination directory {:?}: {}",
                        cfg.destination_path, err
                    );
                }
                cfg
            }
            Err(err) => {
                warn!("Falling back to default configuration ({}):", err);
                let default_cfg = Self::default();
                let _ = fs::create_dir_all(&default_cfg.destination_path);
                let _ = default_cfg.save();
                default_cfg
            }
        }
    }

    fn try_load() -> Result<Self> {
        let path = Self::config_file_path()?;
        if !path.exists() {
            let default_cfg = Self::default();
            default_cfg.save()?;
            info!("Created default configuration at {}", path.display());
            return Ok(default_cfg);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file at {}", path.display()))?;
        let cfg: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse config file at {}", path.display()))?;
        info!(
            "Loaded configuration from {} (destination: {})",
            path.display(),
            cfg.destination_path.display()
        );
        Ok(cfg)
    }

    /// Persists the current configuration to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_file_path()?;
        let serialized =
            serde_json::to_string_pretty(self).context("Failed to serialize AppConfig")?;
        fs::write(&path, serialized)
            .with_context(|| format!("Failed to write config file at {}", path.display()))?;
        info!("Saved configuration to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_roundtrip_serialization() -> Result<()> {
        let cfg = AppConfig {
            destination_path: PathBuf::from("/tmp/custom_usb_backups"),
        };
        let json = serde_json::to_string_pretty(&cfg)?;
        let restored: AppConfig = serde_json::from_str(&json)?;
        assert_eq!(restored.destination_path, cfg.destination_path);
        Ok(())
    }
}

