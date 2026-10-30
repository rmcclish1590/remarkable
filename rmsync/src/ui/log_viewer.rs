//! "Open log file" from the Settings dialog (MCC-131).
//!
//! The viewer never gets the live log. Each click copies it to a read-only
//! snapshot under the cache directory and hands *that* to the desktop's
//! default text handler, so an editor cannot save over (or truncate) the
//! file the app is still appending to, and rotation is unaffected by
//! whatever the viewer holds open. The copy runs on a worker thread and the
//! launch is asynchronous, so a large log never blocks the UI.
//!
//! The decisions — what to copy, what to tell the user — are plain
//! functions so they can be tested without a display.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

pub const OPEN_TOOLTIP: &str = "Open log file";

/// Snapshots older than the newest few are pruned on each open. A handful
/// are kept because a viewer may still have a recent one on screen.
const KEEP_SNAPSHOTS: usize = 3;
const SNAPSHOT_PREFIX: &str = "rmsync-log-";
const SNAPSHOT_SUFFIX: &str = ".log";

/// Where read-only snapshots go. Separate from the log directory so the
/// rotating writer never sees them.
pub fn snapshot_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rmsync")
        .join("log-view")
}

/// Why the log could not be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenLogFailure {
    /// Nothing has been written yet (or the file was removed).
    NoLogFile,
    /// The snapshot could not be made or the viewer could not be launched.
    Failed { log_path: PathBuf, error: String },
}

/// What to show the user when opening did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub heading: &'static str,
    pub body: String,
    /// When set, the dialog offers to copy this path to the clipboard.
    pub copy_path: Option<PathBuf>,
}

impl OpenLogFailure {
    pub fn notice(&self) -> Notice {
        match self {
            OpenLogFailure::NoLogFile => Notice {
                heading: "No log file yet",
                body: "Nothing has been logged to a file yet. It is created the first \
                       time the app writes a log entry."
                    .to_string(),
                copy_path: None,
            },
            OpenLogFailure::Failed { log_path, error } => Notice {
                heading: "Could not open the log file",
                body: format!(
                    "No text viewer could open it ({error}).\n\nThe log file is at:\n{}",
                    log_path.display()
                ),
                copy_path: Some(log_path.clone()),
            },
        }
    }
}

/// The outcome of the viewer launch, given the log it was meant to show.
pub fn launch_result(log_path: &Path, result: Result<(), String>) -> Result<(), OpenLogFailure> {
    result.map_err(|error| OpenLogFailure::Failed {
        log_path: log_path.to_path_buf(),
        error,
    })
}

/// Copy `log_path` to a new read-only file in `dir`, named by `stamp`
/// (any increasing number; the caller passes the time in milliseconds).
/// The live log is only read, never opened for writing.
pub fn snapshot_log(log_path: &Path, dir: &Path, stamp: u128) -> Result<PathBuf, OpenLogFailure> {
    match fs::metadata(log_path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            return Err(OpenLogFailure::Failed {
                log_path: log_path.to_path_buf(),
                error: "not a regular file".to_string(),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(OpenLogFailure::NoLogFile),
        Err(e) => return Err(failed(log_path, e)),
    }

    let make = || -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        // Zero-padded so name order is age order.
        let snapshot = dir.join(format!("{SNAPSHOT_PREFIX}{stamp:020}{SNAPSHOT_SUFFIX}"));
        if snapshot.exists() {
            // A previous snapshot with the same stamp is read-only; replace it.
            fs::remove_file(&snapshot)?;
        }
        fs::copy(log_path, &snapshot)?;
        let mut perms = fs::metadata(&snapshot)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&snapshot, perms)?;
        Ok(snapshot)
    };
    let snapshot = make().map_err(|e| failed(log_path, e))?;
    prune_snapshots(dir, KEEP_SNAPSHOTS);
    Ok(snapshot)
}

fn failed(log_path: &Path, e: io::Error) -> OpenLogFailure {
    OpenLogFailure::Failed {
        log_path: log_path.to_path_buf(),
        error: e.to_string(),
    }
}

/// Delete all but the newest `keep` snapshots. Only files this module
/// named are touched; anything else in the directory is left alone.
fn prune_snapshots(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(SNAPSHOT_PREFIX) && n.ends_with(SNAPSHOT_SUFFIX))
        .collect();
    names.sort_unstable();
    let excess = names.len().saturating_sub(keep);
    for name in &names[..excess] {
        if let Err(e) = fs::remove_file(dir.join(name)) {
            tracing::debug!(file = name.as_str(), error = %e, "could not prune log snapshot");
        }
    }
}

/// Snapshot the log and open it in the default text viewer. `button` is
/// disabled while this runs so repeated clicks do not stack launches.
pub fn open_log_file(window: &adw::PreferencesWindow, button: &gtk::Button, log_path: PathBuf) {
    button.set_sensitive(false);
    let (window, button) = (window.clone(), button.clone());
    glib::spawn_future_local(async move {
        let result = open(&window, &log_path).await;
        if let Err(failure) = result {
            show_notice(&window, &failure.notice());
        }
        button.set_sensitive(true);
    });
}

async fn open(window: &adw::PreferencesWindow, log_path: &Path) -> Result<(), OpenLogFailure> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let (path, dir) = (log_path.to_path_buf(), snapshot_dir());
    let snapshot = gio::spawn_blocking(move || snapshot_log(&path, &dir, stamp))
        .await
        .unwrap_or_else(|_| {
            Err(OpenLogFailure::Failed {
                log_path: log_path.to_path_buf(),
                error: "snapshot worker panicked".to_string(),
            })
        });
    let snapshot = match snapshot {
        Ok(s) => s,
        Err(OpenLogFailure::NoLogFile) => {
            tracing::debug!(log = %log_path.display(), "open log file: no log file yet");
            return Err(OpenLogFailure::NoLogFile);
        }
        Err(f) => {
            tracing::warn!(log = %log_path.display(), failure = ?f, "could not snapshot log file");
            return Err(f);
        }
    };

    tracing::debug!(
        log = %log_path.display(),
        snapshot = %snapshot.display(),
        "opening log file in default viewer"
    );
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&snapshot)));
    let launched = launcher
        .launch_future(Some(window))
        .await
        .map_err(|e| e.message().to_string());
    if let Err(error) = &launched {
        tracing::warn!(snapshot = %snapshot.display(), error = error.as_str(), "log viewer launch failed");
    }
    launch_result(log_path, launched)
}

fn show_notice(window: &adw::PreferencesWindow, notice: &Notice) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading(notice.heading)
        .body(&notice.body)
        .build();
    dialog.add_response("ok", "_OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    if let Some(path) = notice.copy_path.clone() {
        dialog.add_response("copy", "_Copy Path");
        dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("copy"));
        let window = window.clone();
        dialog.connect_response(Some("copy"), move |d, _| {
            d.clipboard().set_text(&path.display().to_string());
            tracing::debug!(path = %path.display(), "log file path copied to clipboard");
            window.add_toast(adw::Toast::new("Log file path copied"));
        });
    }
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_log(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("rmsync.log");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn existing_log_is_snapshotted_read_only_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_log(dir.path(), "first\nlast entry\n");
        let before = fs::metadata(&log).unwrap();

        let snap = snapshot_log(&log, &dir.path().join("view"), 1).unwrap();

        assert_eq!(fs::read_to_string(&snap).unwrap(), "first\nlast entry\n");
        assert!(fs::metadata(&snap).unwrap().permissions().readonly());
        // The live log keeps its content and stays writable for the app.
        let after = fs::metadata(&log).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "first\nlast entry\n");
        assert_eq!(after.len(), before.len());
        assert!(!after.permissions().readonly());
    }

    #[test]
    fn successful_launch_shows_nothing() {
        assert_eq!(launch_result(Path::new("/x/rmsync.log"), Ok(())), Ok(()));
    }

    #[test]
    fn missing_log_reports_no_log_file_yet() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("view");
        let err = snapshot_log(&dir.path().join("rmsync.log"), &view, 1).unwrap_err();
        assert_eq!(err, OpenLogFailure::NoLogFile);
        let notice = err.notice();
        assert_eq!(notice.heading, "No log file yet");
        assert_eq!(notice.copy_path, None);
        assert!(!view.exists(), "nothing created when there is nothing to show");
    }

    #[test]
    fn launch_failure_shows_the_path_and_offers_to_copy_it() {
        let log = Path::new("/home/u/.local/share/rmsync/logs/rmsync.log");
        let err = launch_result(log, Err("No application is registered".into())).unwrap_err();
        let notice = err.notice();
        assert_eq!(notice.heading, "Could not open the log file");
        assert!(notice.body.contains(&log.display().to_string()));
        assert!(notice.body.contains("No application is registered"));
        assert_eq!(notice.copy_path.as_deref(), Some(log));
    }

    #[test]
    fn snapshot_failure_offers_the_live_path() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_log(dir.path(), "x");
        // A file where the snapshot directory should be makes the copy fail.
        let blocker = dir.path().join("view");
        fs::write(&blocker, "").unwrap();
        let err = snapshot_log(&log, &blocker, 1).unwrap_err();
        assert!(matches!(&err, OpenLogFailure::Failed { log_path, .. } if *log_path == log));
        assert_eq!(err.notice().copy_path.as_deref(), Some(log.as_path()));
    }

    #[test]
    fn directory_in_place_of_log_is_a_failure_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = snapshot_log(dir.path(), &dir.path().join("view"), 1).unwrap_err();
        assert!(matches!(err, OpenLogFailure::Failed { .. }));
    }

    #[test]
    fn same_stamp_replaces_the_read_only_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("view");
        let log = write_log(dir.path(), "old");
        snapshot_log(&log, &view, 7).unwrap();
        fs::write(&log, "new").unwrap();
        let snap = snapshot_log(&log, &view, 7).unwrap();
        assert_eq!(fs::read_to_string(snap).unwrap(), "new");
    }

    #[test]
    fn only_the_newest_snapshots_are_kept_and_other_files_survive() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("view");
        let log = write_log(dir.path(), "x");
        fs::create_dir_all(&view).unwrap();
        fs::write(view.join("unrelated.txt"), "keep me").unwrap();

        let snaps: Vec<PathBuf> = (1..=5).map(|s| snapshot_log(&log, &view, s).unwrap()).collect();

        for old in &snaps[..5 - KEEP_SNAPSHOTS] {
            assert!(!old.exists(), "{} should be pruned", old.display());
        }
        for recent in &snaps[5 - KEEP_SNAPSHOTS..] {
            assert!(recent.exists());
        }
        assert!(view.join("unrelated.txt").exists());
    }

    #[test]
    fn large_log_is_copied_whole() {
        let dir = tempfile::tempdir().unwrap();
        let line = "2026-10-30T15:05:50Z INFO rmsync: a fairly ordinary log line\n";
        let content = line.repeat(6 * 1024 * 1024 / line.len());
        let log = write_log(dir.path(), &content);
        let snap = snapshot_log(&log, &dir.path().join("view"), 1).unwrap();
        assert_eq!(fs::metadata(&snap).unwrap().len(), content.len() as u64);
        assert!(fs::read_to_string(&snap).unwrap().ends_with(line));
    }
}
