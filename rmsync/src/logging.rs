//! Application logging: a user-selectable verbosity level, a rotating log
//! file, and a handle that lets the level change while the app is running.
//!
//! Logs go to two places at once: stderr (useful when launched from a
//! terminal) and `~/.local/share/rmsync/logs/rmsync.log`. The file is what
//! matters for support — a user can reproduce a problem, then send the file.
//!
//! Every line carries a timestamp, a level, and the module that emitted it
//! (`tracing`'s target), so a log can be triaged without guessing which
//! subsystem a message came from.
//!
//! Rotation is by size rather than by date: a stuck sync loop can produce
//! megabytes in minutes, and a date-based scheme would let a single day's
//! file grow without limit. When the active file exceeds [`MAX_LOG_BYTES`]
//! it becomes `rmsync.log.1`, the previous `.1` becomes `.2`, and anything
//! past [`MAX_LOG_FILES`] is deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::reload;

/// Rotate once the active log file passes this size.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// How many rotated files to keep besides the active one.
pub const MAX_LOG_FILES: usize = 5;

const LOG_DIR: &str = "rmsync/logs";
const LOG_FILENAME: &str = "rmsync.log";

/// How much the app logs. Ordered from quietest to loudest.
///
/// The levels differ in two dimensions, not just one: how much *rmsync*
/// says, and whether the libraries it uses (russh, sqlite, GTK) say
/// anything at all. Dependency logs are voluminous and rarely what you
/// want, so only the two upper levels let them through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Warnings and errors only — nothing about normal operation.
    ErrorsOnly,
    /// Default. rmsync reports what it does; dependencies stay quiet
    /// unless something is wrong.
    #[default]
    Production,
    /// Adds general informational output from dependencies too — useful
    /// for questions like "did the SSH layer even try to connect?".
    Information,
    /// Everything, including per-file and per-page detail. Verbose enough
    /// to slow a large sync down; meant for reproducing a specific bug.
    Debug,
}

impl LogLevel {
    pub const ALL: [LogLevel; 4] = [
        LogLevel::ErrorsOnly,
        LogLevel::Production,
        LogLevel::Information,
        LogLevel::Debug,
    ];

    /// Stable identifier used in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::ErrorsOnly => "errors_only",
            LogLevel::Production => "production",
            LogLevel::Information => "information",
            LogLevel::Debug => "debug",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        LogLevel::ALL
            .into_iter()
            .find(|l| l.as_str().eq_ignore_ascii_case(s))
    }

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::ErrorsOnly => "Errors only",
            LogLevel::Production => "Production",
            LogLevel::Information => "Information",
            LogLevel::Debug => "Debug",
        }
    }

    /// One-line explanation shown beneath the selector.
    pub fn description(self) -> &'static str {
        match self {
            LogLevel::ErrorsOnly => "Only warnings and errors.",
            LogLevel::Production => "Normal use: what rmSync does, plus any problems.",
            LogLevel::Information => "Adds informational messages from supporting libraries.",
            LogLevel::Debug => "Everything, including per-file and per-page detail.",
        }
    }

    /// The severity rmsync's own modules are filtered at.
    pub fn app_filter(self) -> LevelFilter {
        match self {
            LogLevel::ErrorsOnly => LevelFilter::WARN,
            LogLevel::Production => LevelFilter::INFO,
            LogLevel::Information => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::DEBUG,
        }
    }

    /// The severity everything else is filtered at.
    fn dependency_filter(self) -> LevelFilter {
        match self {
            LogLevel::ErrorsOnly => LevelFilter::WARN,
            LogLevel::Production => LevelFilter::WARN,
            LogLevel::Information => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::INFO,
        }
    }

    /// Build the filter: a global default for dependencies, overridden for
    /// this crate. `RUST_LOG` still wins when set, so a developer can ask
    /// for something the UI cannot express.
    fn env_filter(self) -> EnvFilter {
        if let Ok(from_env) = EnvFilter::try_from_default_env() {
            return from_env;
        }
        EnvFilter::default()
            .add_directive(self.dependency_filter().into())
            .add_directive(
                format!("rmsync={}", self.app_filter())
                    .parse()
                    .expect("static directive is valid"),
            )
    }
}

/// Environment override for the log directory, so logs can be redirected
/// somewhere writable (a sandbox, a support capture) without editing the
/// config.
pub const LOG_DIR_ENV: &str = "RMSYNC_LOG_DIR";

/// Directory holding the log files.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(LOG_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(LOG_DIR)
}

/// Path of the active log file.
pub fn log_path() -> PathBuf {
    log_dir().join(LOG_FILENAME)
}

/// Changes the log level of the running process.
#[derive(Clone)]
pub struct LogHandle {
    reload: reload::Handle<EnvFilter, tracing_subscriber::Registry>,
    path: Option<PathBuf>,
}

impl LogHandle {
    /// Apply a new level immediately, without restarting.
    pub fn set_level(&self, level: LogLevel) -> Result<(), String> {
        self.reload
            .reload(level.env_filter())
            .map_err(|e| format!("applying log level: {e}"))?;
        tracing::info!(level = level.as_str(), "log level changed");
        Ok(())
    }

    /// Where logs are being written, or `None` if the file could not be
    /// opened (logging then continues to stderr alone).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Install the global subscriber. Call once, early in `main`.
///
/// A log file that cannot be opened is reported and then ignored: failing
/// to write logs is not a reason to refuse to start.
pub fn init(level: LogLevel) -> LogHandle {
    let (filter, reload_handle) = reload::Layer::new(level.env_filter());

    let path = log_path();
    let file_writer = match RotatingWriter::new(&path, MAX_LOG_BYTES, MAX_LOG_FILES) {
        Ok(w) => Some(Arc::new(Mutex::new(w))),
        Err(e) => {
            eprintln!("rmsync: logging to file disabled ({}): {e}", path.display());
            None
        }
    };

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_writer(io::stderr);

    let file_layer = file_writer.clone().map(|w| {
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_ansi(false)
            .with_writer(move || WriterHandle(w.clone()))
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    LogHandle {
        reload: reload_handle,
        path: file_writer.map(|_| path),
    }
}

/// Per-write handle onto the shared rotating file.
struct WriterHandle(Arc<Mutex<RotatingWriter>>);

impl Write for WriterHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut w) => w.write(buf),
            // A poisoned lock means another thread panicked mid-write.
            // Drop the line rather than cascade the panic into whatever
            // was being logged about.
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.lock() {
            Ok(mut w) => w.flush(),
            Err(_) => Ok(()),
        }
    }
}

/// Append-only file that rotates itself once it outgrows `max_bytes`.
struct RotatingWriter {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: usize,
}

impl RotatingWriter {
    fn new(path: &Path, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_bytes,
            max_files,
        })
    }

    /// Shift `rmsync.log` → `.1` → `.2` … dropping the oldest.
    fn rotate(&mut self) -> io::Result<()> {
        let numbered = |n: usize| -> PathBuf {
            let mut name = self.path.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".{n}"));
            self.path.with_file_name(name)
        };

        if self.max_files == 0 {
            // Keep no history: just start the active file over.
            self.file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&self.path)?;
            self.written = 0;
            return Ok(());
        }

        let oldest = numbered(self.max_files);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for n in (1..self.max_files).rev() {
            let from = numbered(n);
            if from.exists() {
                fs::rename(&from, numbered(n + 1))?;
            }
        }
        fs::rename(&self.path, numbered(1))?;

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.max_bytes {
            // A failed rotation must not lose the line being written, so
            // keep appending to the current file and try again next time.
            if let Err(e) = self.rotate() {
                eprintln!("rmsync: log rotation failed: {e}");
                self.written = 0;
            }
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn level_strings_round_trip() {
        for level in LogLevel::ALL {
            assert_eq!(LogLevel::from_str_opt(level.as_str()), Some(level));
        }
        assert_eq!(LogLevel::from_str_opt("PRODUCTION"), Some(LogLevel::Production));
        assert_eq!(LogLevel::from_str_opt("nonsense"), None);
    }

    #[test]
    fn levels_are_ordered_quietest_to_loudest() {
        // Each level lets through at least as much as the one before it.
        for pair in LogLevel::ALL.windows(2) {
            assert!(
                pair[0].app_filter() <= pair[1].app_filter(),
                "{:?} should not be louder than {:?}",
                pair[0],
                pair[1]
            );
            assert!(pair[0].dependency_filter() <= pair[1].dependency_filter());
        }
    }

    #[test]
    fn errors_only_suppresses_info() {
        assert_eq!(LogLevel::ErrorsOnly.app_filter(), LevelFilter::WARN);
        assert_eq!(LogLevel::Debug.app_filter(), LevelFilter::DEBUG);
    }

    #[test]
    fn production_is_the_default() {
        assert_eq!(LogLevel::default(), LogLevel::Production);
    }

    #[test]
    fn every_level_builds_a_usable_filter() {
        for level in LogLevel::ALL {
            // Must not panic on the static directive.
            let _ = level.env_filter();
        }
    }

    #[test]
    fn writer_rotates_when_over_the_size_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rmsync.log");
        let mut w = RotatingWriter::new(&path, 64, 3).unwrap();

        for _ in 0..10 {
            w.write_all(&[b'x'; 32]).unwrap();
        }
        w.flush().unwrap();

        assert!(path.exists(), "active log still present");
        assert!(dir.path().join("rmsync.log.1").exists(), "rotated once");
        // The active file is always smaller than the whole run's output.
        let active = fs::metadata(&path).unwrap().len();
        assert!(active < 320, "active file should have been rotated: {active}");
    }

    #[test]
    fn rotation_keeps_at_most_max_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rmsync.log");
        let mut w = RotatingWriter::new(&path, 16, 2).unwrap();

        for _ in 0..40 {
            w.write_all(&[b'y'; 16]).unwrap();
        }
        w.flush().unwrap();

        assert!(dir.path().join("rmsync.log.1").exists());
        assert!(dir.path().join("rmsync.log.2").exists());
        assert!(
            !dir.path().join("rmsync.log.3").exists(),
            "history must be capped at max_files"
        );
    }

    #[test]
    fn writer_appends_to_an_existing_log() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rmsync.log");
        fs::write(&path, b"previous session\n").unwrap();

        let mut w = RotatingWriter::new(&path, 1024, 3).unwrap();
        w.write_all(b"this session\n").unwrap();
        w.flush().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("previous session"));
        assert!(contents.contains("this session"));
    }

    #[test]
    fn writer_creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deeper/rmsync.log");
        let mut w = RotatingWriter::new(&path, 1024, 1).unwrap();
        w.write_all(b"hello\n").unwrap();
        w.flush().unwrap();
        assert!(path.exists());
    }

    #[test]
    fn log_path_is_under_the_app_data_dir() {
        let p = log_path();
        assert!(p.to_string_lossy().contains("rmsync"));
        assert_eq!(p.file_name().unwrap(), LOG_FILENAME);
    }
}
