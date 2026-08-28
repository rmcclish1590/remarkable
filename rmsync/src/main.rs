//! rmSync — bi-directional sync for reMarkable 2 tablets.
//!
//! Entry point: initialises logging, builds the libadwaita application,
//! and launches the main window.

use rmsync::config::AppConfig;
use rmsync::logging::{self, LogLevel};

fn main() -> std::process::ExitCode {
    // The configured level has to be known before the subscriber exists,
    // so read the config first and fall back to the default if it cannot
    // be read — a broken config should not cost us the log that explains
    // why it is broken. `AppConfig::load` runs again inside the app; this
    // read only decides the log level.
    let (level, config_error) = match AppConfig::load_from(&AppConfig::config_path()) {
        Ok(cfg) => (cfg.logging.level, None),
        Err(e) => (LogLevel::default(), Some(e)),
    };

    let handle = logging::init(level);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log_level = level.as_str(),
        log_file = handle.path().map(|p| p.display().to_string()),
        config_file = %AppConfig::config_path().display(),
        "rmSync starting"
    );
    if let Some(e) = config_error {
        tracing::warn!(error = format!("{e:#}"), "could not read config; using defaults");
    }

    let exit_code = rmsync::app::RmSyncApp::new(handle).run();
    tracing::info!(exit_code, "rmSync exiting");
    std::process::ExitCode::from(exit_code as u8)
}
