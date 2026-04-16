//! Load and save the user configuration (TOML) from the XDG config dir.
//!
//! Config lives at `~/.config/rmsync/config.toml`. On first run the file is
//! created with defaults and the sync directory (+ `raw/` and `.rmsync/`
//! subdirs) is provisioned.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::device::connection::{AuthMethod, ConnectionConfig};

const CONFIG_FILENAME: &str = "config.toml";
const APP_CONFIG_SUBDIR: &str = "rmsync";
const SYNC_RAW: &str = "raw";
const SYNC_META: &str = ".rmsync";

fn default_true() -> bool {
    true
}
fn default_host() -> String {
    "10.11.99.1".to_string()
}
fn default_port() -> u16 {
    22
}
fn default_username() -> String {
    "root".to_string()
}
fn default_timeout() -> u64 {
    5
}
fn default_width() -> i32 {
    1200
}
fn default_height() -> i32 {
    800
}
fn default_sidebar() -> i32 {
    280
}

fn default_sync_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Documents")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("reMarkable")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncConfig {
    #[serde(default = "default_sync_dir")]
    pub sync_dir: PathBuf,
    #[serde(default)]
    pub auto_sync_on_connect: bool,
    #[serde(default = "default_true")]
    pub confirm_deletes: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            sync_dir: default_sync_dir(),
            auto_sync_on_connect: false,
            confirm_deletes: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            username: default_username(),
            key_path: None,
            timeout_secs: default_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiConfig {
    #[serde(default = "default_width")]
    pub window_width: i32,
    #[serde(default = "default_height")]
    pub window_height: i32,
    #[serde(default = "default_sidebar")]
    pub sidebar_width: i32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            window_width: default_width(),
            window_height: default_height(),
            sidebar_width: default_sidebar(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub device: DeviceConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

impl AppConfig {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_CONFIG_SUBDIR)
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join(CONFIG_FILENAME)
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        let cfg = Self::load_from(&path)?;
        cfg.ensure_directories()?;
        if !path.exists() {
            cfg.save_to(&path)?;
        }
        Ok(cfg)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: AppConfig = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn ensure_directories(&self) -> Result<()> {
        let sync = &self.sync.sync_dir;
        std::fs::create_dir_all(sync)
            .with_context(|| format!("creating sync dir {}", sync.display()))?;
        std::fs::create_dir_all(sync.join(SYNC_RAW))
            .with_context(|| format!("creating raw dir under {}", sync.display()))?;
        std::fs::create_dir_all(sync.join(SYNC_META))
            .with_context(|| format!("creating .rmsync dir under {}", sync.display()))?;
        Ok(())
    }

    pub fn to_connection_config(&self) -> ConnectionConfig {
        let cfg_dir = Self::config_dir();
        let key_path = self
            .device
            .key_path
            .clone()
            .unwrap_or_else(|| cfg_dir.join("id_rmsync"));
        let auth = match &self.device.key_path {
            Some(p) => AuthMethod::KeyFile(p.clone()),
            None => AuthMethod::Password(String::new()),
        };
        ConnectionConfig {
            host: self.device.host.clone(),
            port: self.device.port,
            username: self.device.username.clone(),
            auth,
            timeout_secs: self.device.timeout_secs,
            known_hosts_path: cfg_dir.join("known_hosts"),
            key_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_has_expected_values() {
        let c = AppConfig::default();
        assert_eq!(c.device.host, "10.11.99.1");
        assert_eq!(c.device.port, 22);
        assert_eq!(c.device.username, "root");
        assert_eq!(c.device.timeout_secs, 5);
        assert!(c.sync.confirm_deletes);
        assert!(!c.sync.auto_sync_on_connect);
        assert_eq!(c.ui.window_width, 1200);
        assert_eq!(c.ui.window_height, 800);
        assert_eq!(c.ui.sidebar_width, 280);
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = AppConfig::default();
        c.sync.sync_dir = dir.path().join("sync");
        c.sync.auto_sync_on_connect = true;
        c.device.host = "192.168.1.2".into();
        c.device.port = 2222;
        c.device.key_path = Some(PathBuf::from("/home/u/.config/rmsync/id_rmsync"));
        c.ui.window_width = 1440;
        c.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, c);
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempdir().unwrap();
        let c = AppConfig::load_from(&dir.path().join("no.toml")).unwrap();
        assert_eq!(c, AppConfig::default());
    }

    #[test]
    fn partial_toml_fills_defaults_for_missing_sections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(
            &path,
            "[sync]\nauto_sync_on_connect = true\nconfirm_deletes = false\n",
        )
        .unwrap();
        let c = AppConfig::load_from(&path).unwrap();
        assert!(c.sync.auto_sync_on_connect);
        assert!(!c.sync.confirm_deletes);
        assert_eq!(c.device, DeviceConfig::default());
        assert_eq!(c.ui, UiConfig::default());
    }

    #[test]
    fn ensure_directories_creates_raw_and_rmsync() {
        let dir = tempdir().unwrap();
        let mut c = AppConfig::default();
        c.sync.sync_dir = dir.path().join("sync");
        c.ensure_directories().unwrap();
        assert!(c.sync.sync_dir.join("raw").is_dir());
        assert!(c.sync.sync_dir.join(".rmsync").is_dir());
    }

    #[test]
    fn to_connection_config_uses_password_by_default() {
        let c = AppConfig::default();
        let cc = c.to_connection_config();
        assert_eq!(cc.host, "10.11.99.1");
        matches!(cc.auth, AuthMethod::Password(_));
    }

    #[test]
    fn to_connection_config_uses_keyfile_when_set() {
        let mut c = AppConfig::default();
        c.device.key_path = Some(PathBuf::from("/tmp/k"));
        let cc = c.to_connection_config();
        match cc.auth {
            AuthMethod::KeyFile(p) => assert_eq!(p, PathBuf::from("/tmp/k")),
            _ => panic!("wrong auth"),
        }
    }

    #[test]
    fn config_path_contains_app_subdir() {
        let p = AppConfig::config_path();
        assert!(p.to_string_lossy().contains("rmsync"));
        assert!(p.file_name().unwrap() == CONFIG_FILENAME);
    }
}
