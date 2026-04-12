//! Load and save the user configuration (TOML) from the XDG config dir.

/// Application configuration persisted to `~/.config/rmsync/config.toml`.
pub struct Config;

impl Config {
    /// Load configuration from disk, creating defaults if absent.
    pub fn load() -> Self {
        todo!()
    }
}
