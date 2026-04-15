//! Sync button, progress bar, and sync-folder selector.
//!
//! Spec 17 wires the Browse button to a native `gtk::FileDialog` that lets
//! the user pick a sync destination. The choice is validated (writable dir
//! tree), persisted to `AppConfig`, and reflected back into the path entry.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::sync::transfer::TransferProgress;

/// Wire the Browse button to a folder-chooser dialog. The returned entry is
/// updated and `config` is saved whenever the user picks a folder.
pub fn setup_folder_selector(
    browse_button: &gtk::Button,
    path_entry: &gtk::Entry,
    window: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
) {
    let window = window.clone();
    let path_entry = path_entry.clone();
    let config_outer = config.clone();
    browse_button.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Choose Sync Destination")
            .modal(true)
            .build();
        let initial = gtk::gio::File::for_path(&config_outer.borrow().sync.sync_dir);
        dialog.set_initial_folder(Some(&initial));

        let config = config_outer.clone();
        let path_entry = path_entry.clone();
        let window_for_dialog = window.clone();
        dialog.select_folder(
            Some(&window),
            gtk::gio::Cancellable::NONE,
            move |res: Result<gtk::gio::File, glib::Error>| match res {
                Ok(file) => {
                    let Some(path) = file.path() else { return };
                    match apply_sync_dir(&config, &path) {
                        Ok(()) => path_entry.set_text(&path.to_string_lossy()),
                        Err(e) => show_error_dialog(&window_for_dialog, &e.to_string()),
                    }
                }
                Err(e) => {
                    // Cancelled-by-user surfaces as a DIALOG_ERROR quark;
                    // treat any error matching the dismissed pattern as a
                    // no-op, everything else as a real error to show.
                    let msg = e.message();
                    if msg.to_ascii_lowercase().contains("dismiss")
                        || msg.to_ascii_lowercase().contains("cancel")
                    {
                        return;
                    }
                    show_error_dialog(&window_for_dialog, msg);
                }
            },
        );
    });
}

/// Apply a new sync directory: update the config, create required
/// subdirectories, and persist to disk. Returns an error if the directory
/// tree cannot be created (e.g. read-only filesystem).
pub fn apply_sync_dir(config: &Rc<RefCell<AppConfig>>, path: &Path) -> anyhow::Result<()> {
    {
        let mut cfg = config.borrow_mut();
        cfg.sync.sync_dir = path.to_path_buf();
        cfg.ensure_directories()?;
        cfg.save()?;
    }
    Ok(())
}

fn show_error_dialog(window: &adw::ApplicationWindow, message: &str) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Cannot use that folder")
        .body(message)
        .build();
    dialog.add_response("ok", "_OK");
    dialog.set_default_response(Some("ok"));
    dialog.connect_response(None, |d, _| d.close());
    dialog.present();
}

/// Ensure the path entry reflects the current config value and that the
/// required subdirectories exist. Call once at window construction.
pub fn sync_entry_initial_state(path_entry: &gtk::Entry, config: &AppConfig) {
    path_entry.set_text(&config.sync.sync_dir.to_string_lossy());
    let _ = config.ensure_directories();
}

// =========================================================================
// Sync button + progress bar (spec 19)
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncUiState {
    Idle,
    Syncing,
    Cancelling,
}

#[derive(Clone)]
pub struct SyncControls {
    pub sync_button: gtk::Button,
    pub progress_bar: gtk::ProgressBar,
    pub progress_box: gtk::Box,
    pub status_label: gtk::Label,
    pub cancel_button: gtk::Button,
    state: Rc<RefCell<SyncUiState>>,
    device_connected: Rc<RefCell<bool>>,
}

impl SyncControls {
    /// Build the sync button and progress area. The button is intended for
    /// the header bar; the progress box for the bottom of the window.
    pub fn new() -> Self {
        let sync_button = gtk::Button::builder()
            .label("Sync Now")
            .sensitive(false)
            .build();
        sync_button.add_css_class("suggested-action");

        let status_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .margin_start(12)
            .margin_top(4)
            .build();
        let progress_bar = gtk::ProgressBar::builder().hexpand(true).build();
        let cancel_button = gtk::Button::builder().label("Cancel").build();

        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();
        row.append(&progress_bar);
        row.append(&cancel_button);

        let progress_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .visible(false)
            .build();
        progress_box.append(&status_label);
        progress_box.append(&row);

        Self {
            sync_button,
            progress_bar,
            progress_box,
            status_label,
            cancel_button,
            state: Rc::new(RefCell::new(SyncUiState::Idle)),
            device_connected: Rc::new(RefCell::new(false)),
        }
    }

    pub fn is_syncing(&self) -> bool {
        matches!(*self.state.borrow(), SyncUiState::Syncing)
    }

    pub fn state(&self) -> SyncUiState {
        *self.state.borrow()
    }

    pub fn set_device_connected(&self, connected: bool) {
        *self.device_connected.borrow_mut() = connected;
        if matches!(*self.state.borrow(), SyncUiState::Idle) {
            self.sync_button.set_sensitive(connected);
        }
    }

    pub fn start_sync(&self) {
        *self.state.borrow_mut() = SyncUiState::Syncing;
        self.sync_button.set_sensitive(false);
        self.sync_button.set_label("Syncing...");
        self.sync_button.remove_css_class("suggested-action");
        self.sync_button.remove_css_class("destructive-action");
        self.sync_button.add_css_class("flat");
        self.progress_bar.set_fraction(0.0);
        self.status_label.set_text("Starting sync...");
        self.progress_box.set_visible(true);
        self.cancel_button.set_visible(true);
    }

    pub fn update_progress(&self, progress: &TransferProgress) {
        let fraction = progress_fraction(progress);
        self.progress_bar.set_fraction(fraction);
        let current_idx = progress.files_done + 1;
        self.status_label.set_text(&format!(
            "Syncing: \"{}\" ({} of {})",
            progress.current_file, current_idx, progress.files_total
        ));
    }

    pub fn finish_sync(&self, summary: &str) {
        *self.state.borrow_mut() = SyncUiState::Idle;
        self.progress_bar.set_fraction(1.0);
        self.status_label.set_text(summary);
        self.cancel_button.set_visible(false);
        self.sync_button.set_label("Sync Now");
        self.sync_button.remove_css_class("flat");
        self.sync_button.remove_css_class("destructive-action");
        self.sync_button.add_css_class("suggested-action");
        self.sync_button
            .set_sensitive(*self.device_connected.borrow());
        let progress_box = self.progress_box.clone();
        glib::timeout_add_seconds_local_once(3, move || {
            progress_box.set_visible(false);
        });
    }

    pub fn show_error(&self, message: &str) {
        *self.state.borrow_mut() = SyncUiState::Idle;
        self.status_label.set_text(&format!("Sync failed: {message}"));
        self.progress_bar.set_fraction(0.0);
        self.cancel_button.set_visible(false);
        self.sync_button.set_label("Retry Sync");
        self.sync_button.remove_css_class("flat");
        self.sync_button.remove_css_class("suggested-action");
        self.sync_button.add_css_class("destructive-action");
        self.sync_button.set_sensitive(true);
    }

    pub fn connect_sync_clicked<F: Fn() + 'static>(&self, callback: F) {
        self.sync_button.connect_clicked(move |_| callback());
    }

    pub fn connect_cancel_clicked<F: Fn() + 'static>(&self, callback: F) {
        let state = self.state.clone();
        self.cancel_button.connect_clicked(move |_| {
            *state.borrow_mut() = SyncUiState::Cancelling;
            callback();
        });
    }
}

impl Default for SyncControls {
    fn default() -> Self {
        Self::new()
    }
}

pub fn progress_fraction(progress: &TransferProgress) -> f64 {
    if progress.files_total == 0 {
        0.0
    } else {
        (progress.files_done as f64 / progress.files_total as f64).clamp(0.0, 1.0)
    }
}

/// Humanise a unix timestamp relative to now, e.g. "2 minutes ago".
pub fn format_last_sync_relative(last_sync_unix: Option<u64>, now_unix: u64) -> String {
    match last_sync_unix {
        None => "Last sync: Never".to_string(),
        Some(ts) => {
            let delta = now_unix.saturating_sub(ts);
            let (value, unit) = if delta >= 86400 {
                (delta / 86400, "day")
            } else if delta >= 3600 {
                (delta / 3600, "hour")
            } else if delta >= 60 {
                (delta / 60, "minute")
            } else {
                (delta.max(1), "second")
            };
            let plural = if value == 1 { "" } else { "s" };
            format!("Last sync: {value} {unit}{plural} ago")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn apply_sync_dir_writes_config_and_creates_subdirs() {
        let dir = tempdir().unwrap();
        let cfg_dir = dir.path().join("cfg");
        let sync = dir.path().join("sync");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg = Rc::new(RefCell::new(AppConfig::default()));
        apply_sync_dir(&cfg, &sync).unwrap();
        assert_eq!(cfg.borrow().sync.sync_dir, sync);
        assert!(sync.join("raw").is_dir());
        assert!(sync.join(".rmsync").is_dir());
    }

    #[test]
    fn progress_fraction_handles_zero_total() {
        let p = TransferProgress {
            current_file: "x".into(),
            current_uuid: "u".into(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
        };
        assert_eq!(progress_fraction(&p), 0.0);
    }

    #[test]
    fn progress_fraction_computes_ratio() {
        let p = TransferProgress {
            current_file: "x".into(),
            current_uuid: "u".into(),
            files_done: 3,
            files_total: 10,
            bytes_done: 0,
            bytes_total: 0,
        };
        assert!((progress_fraction(&p) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn last_sync_never_when_none() {
        assert_eq!(format_last_sync_relative(None, 100), "Last sync: Never");
    }

    #[test]
    fn last_sync_minutes_and_hours() {
        let now = 10_000u64;
        assert_eq!(
            format_last_sync_relative(Some(now - 120), now),
            "Last sync: 2 minutes ago"
        );
        assert_eq!(
            format_last_sync_relative(Some(now - 3600), now),
            "Last sync: 1 hour ago"
        );
    }

    #[test]
    fn last_sync_days() {
        let now = 10_000_000u64;
        assert_eq!(
            format_last_sync_relative(Some(now - 86400 * 3), now),
            "Last sync: 3 days ago"
        );
    }

    #[test]
    fn apply_sync_dir_returns_error_on_unwritable_parent() {
        // A path under a non-existent root file (not dir) cannot be created.
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let target = blocker.join("under-a-file");
        let cfg = Rc::new(RefCell::new(AppConfig::default()));
        assert!(apply_sync_dir(&cfg, &target).is_err());
    }
}
