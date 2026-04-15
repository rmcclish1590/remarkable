# Spec 10 — Configuration Persistence

**Layer:** 2 — Local Infrastructure  
**Dependencies:** 01 (project scaffolding)  
**Estimated effort:** 1 hour  

## Objective

Implement TOML-based configuration persistence for application settings including sync destination, connection parameters, and UI preferences.

## Context

The application needs to remember the user's chosen sync directory, SSH credentials/key path, and UI preferences between sessions. Configuration is stored in `~/.config/rmsync/config.toml`.

## Technical Requirements

### 1. Config struct (`src/config.rs`)

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub device: DeviceConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Path to the local sync destination directory.
    /// Default: ~/Documents/reMarkable
    pub sync_dir: PathBuf,
    /// Whether to auto-sync when device connects.
    #[serde(default)]
    pub auto_sync_on_connect: bool,
    /// Whether to confirm before deleting files during sync.
    #[serde(default = "default_true")]
    pub confirm_deletes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// reMarkable IP address. Default: 10.11.99.1
    #[serde(default = "default_host")]
    pub host: String,
    /// SSH port. Default: 22
    #[serde(default = "default_port")]
    pub port: u16,
    /// SSH username. Default: root
    #[serde(default = "default_username")]
    pub username: String,
    /// Path to SSH private key. None = password auth.
    pub key_path: Option<PathBuf>,
    /// Connection timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Main window width.
    #[serde(default = "default_width")]
    pub window_width: i32,
    /// Main window height.
    #[serde(default = "default_height")]
    pub window_height: i32,
    /// Sidebar width in pixels.
    #[serde(default = "default_sidebar")]
    pub sidebar_width: i32,
}

// Default value functions
fn default_true() -> bool { true }
fn default_host() -> String { "10.11.99.1".to_string() }
fn default_port() -> u16 { 22 }
fn default_username() -> String { "root".to_string() }
fn default_timeout() -> u64 { 5 }
fn default_width() -> i32 { 1200 }
fn default_height() -> i32 { 800 }
fn default_sidebar() -> i32 { 280 }
```

### 2. Config operations

```rust
impl AppConfig {
    /// Load config from the default path (~/.config/rmsync/config.toml).
    /// If the file doesn't exist, return defaults and create the file.
    pub fn load() -> Result<Self>

    /// Load from a specific path.
    pub fn load_from(path: &Path) -> Result<Self>

    /// Save current config to the default path.
    pub fn save(&self) -> Result<()>

    /// Save to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<()>

    /// Get the config directory path (~/.config/rmsync/).
    pub fn config_dir() -> PathBuf

    /// Get the default config file path.
    pub fn config_path() -> PathBuf

    /// Convert device config to a ConnectionConfig (from Spec 06).
    pub fn to_connection_config(&self) -> ConnectionConfig
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            sync: SyncConfig {
                sync_dir: dirs::document_dir()
                    .unwrap_or_else(|| dirs::home_dir().unwrap().join("Documents"))
                    .join("reMarkable"),
                auto_sync_on_connect: false,
                confirm_deletes: true,
            },
            device: DeviceConfig::default(),
            ui: UiConfig::default(),
        }
    }
}
```

### 3. File format

Generated `config.toml` example:

```toml
[sync]
sync_dir = "/home/ryan/Documents/reMarkable"
auto_sync_on_connect = false
confirm_deletes = true

[device]
host = "10.11.99.1"
port = 22
username = "root"
timeout_secs = 5
# key_path = "/home/ryan/.config/rmsync/id_rmsync"  # Uncommented after key setup

[ui]
window_width = 1200
window_height = 800
sidebar_width = 280
```

### 4. Directory creation

On first run, `load()` should:
1. Create `~/.config/rmsync/` if it doesn't exist.
2. Write a default `config.toml` if none exists.
3. Create the `sync_dir` directory if it doesn't exist.
4. Create `{sync_dir}/raw/` and `{sync_dir}/.rmsync/` subdirectories.

## Files to Create/Modify

- `src/config.rs` — full implementation
- Add `dirs = "5"` to Cargo.toml if not already present.

## Test Strategy

1. **Default config** — verify `AppConfig::default()` produces expected values.
2. **Roundtrip** — create a config, save to temp file, load it back, verify equality.
3. **Missing file** — load from a non-existent path, verify defaults are returned.
4. **Partial TOML** — write a TOML file with only `[sync]` section, verify device and UI sections use defaults.
5. **Directory creation** — load config with a temp dir as sync_dir, verify subdirectories are created.

## Acceptance Criteria

1. Config loads from `~/.config/rmsync/config.toml` with sensible defaults.
2. Missing fields in the TOML file fall back to defaults (forward-compatible).
3. Config saves cleanly and roundtrips without data loss.
4. Required directories are created on first run.
5. All unit tests pass.
