//! Sync button, progress bar, and sync-folder selector.
//!
//! Spec 17 wires the Browse button to a native `gtk::FileDialog` that lets
//! the user pick a sync destination. The choice is validated (writable dir
//! tree), persisted to `AppConfig`, and reflected back into the path entry.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::device::connection::{AuthMethod, DeviceConnection};
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
                        Err(e) => show_error_dialog_with_heading(
                            &window_for_dialog,
                            "Cannot use that folder",
                            &e.to_string(),
                        ),
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
                    show_error_dialog_with_heading(
                        &window_for_dialog,
                        "Cannot use that folder",
                        msg,
                    );
                }
            },
        );
    });
}

/// Apply a new sync directory: update the config, create required
/// subdirectories, and persist to disk. Returns an error if the directory
/// tree cannot be created (e.g. read-only filesystem).
pub fn apply_sync_dir(config: &Rc<RefCell<AppConfig>>, path: &Path) -> anyhow::Result<()> {
    apply_sync_dir_to(config, path, &AppConfig::config_path())
}

/// `apply_sync_dir` with an explicit destination for the config file.
///
/// The destination is a parameter rather than something this function
/// looks up so that tests can exercise it without writing to the config
/// of whoever is running them — reaching for `AppConfig::config_path()`
/// internally meant every `cargo test` run silently repointed the
/// developer's own `sync_dir` at a temp directory that was then deleted
/// (MCC-51).
pub fn apply_sync_dir_to(
    config: &Rc<RefCell<AppConfig>>,
    path: &Path,
    config_path: &Path,
) -> anyhow::Result<()> {
    let mut cfg = config.borrow_mut();
    cfg.sync.sync_dir = path.to_path_buf();
    cfg.ensure_directories()?;
    cfg.save_to(config_path)?;
    Ok(())
}

fn show_error_dialog_with_heading(
    window: &adw::ApplicationWindow,
    heading: &str,
    message: &str,
) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading(heading)
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

/// Prompt for the reMarkable's root password, try to connect, install an
/// Ed25519 keypair for future passwordless logins, and persist the
/// resulting `key_path` to config. `on_ready` fires on the GTK main
/// thread once the device is configured (or immediately if a key is
/// already set). On cancel or failure `on_ready` is NOT called.
pub fn ensure_device_configured<F: Fn() + 'static>(
    window: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    on_ready: F,
) {
    if config.borrow().device.key_path.is_some() {
        on_ready();
        return;
    }

    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading("Set up reMarkable access")
        .body(
            "First-time connection — enter your tablet's SSH password.\n\n\
             On the tablet: Settings → Help → Copyrights and licenses. \
             Scroll to the bottom — the root password is listed under \
             GPLv3 Compliance (older firmware: Settings → General → \
             Software → \"Developer Mode\" must be enabled first).\n\n\
             This password is used once to install an SSH key — future \
             syncs will be automatic.",
        )
        .build();

    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(16)
        .margin_end(16)
        .build();
    dialog.set_extra_child(Some(&entry));

    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("connect", "_Connect");
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("connect"));
    dialog.set_close_response("cancel");

    let window_for_error = window.clone();
    let on_ready = Rc::new(on_ready);
    let entry_for_response = entry.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response != "connect" {
            dialog.close();
            return;
        }
        let password = entry_for_response.text().to_string();
        if password.is_empty() {
            show_error_dialog_with_heading(
                &window_for_error,
                "Password required",
                "Password cannot be empty.",
            );
            return;
        }
        dialog.close();
        run_setup_key_auth(
            &window_for_error,
            config.clone(),
            password,
            on_ready.clone(),
        );
    });

    dialog.present();
}

fn run_setup_key_auth<F: Fn() + 'static>(
    window: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    password: String,
    on_ready: Rc<F>,
) {
    let (result_tx, result_rx) = async_channel::bounded::<Result<std::path::PathBuf, String>>(1);
    let conn_config = {
        let mut cc = config.borrow().to_connection_config();
        cc.auth = AuthMethod::Password(password);
        cc
    };
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = result_tx.send_blocking(Err(format!("runtime: {e}")));
                return;
            }
        };
        runtime.block_on(async move {
            let mut conn = DeviceConnection::new(conn_config);
            if let Err(e) = conn.connect().await {
                let _ = result_tx.send(Err(format!("connect: {e}"))).await;
                return;
            }
            match conn.setup_key_auth().await {
                Ok(key_path) => {
                    conn.disconnect().await;
                    let _ = result_tx.send(Ok(key_path)).await;
                }
                Err(e) => {
                    conn.disconnect().await;
                    let _ = result_tx.send(Err(format!("install key: {e}"))).await;
                }
            }
        });
    });

    let window = window.clone();
    let on_ready = on_ready.clone();
    glib::spawn_future_local(async move {
        match result_rx.recv().await {
            Ok(Ok(key_path)) => {
                {
                    let mut cfg = config.borrow_mut();
                    cfg.device.key_path = Some(key_path);
                    let _ = cfg.save();
                }
                on_ready();
            }
            Ok(Err(msg)) => {
                if msg.contains("Host key mismatch") {
                    // Do NOT suggest deleting known_hosts as a routine fix —
                    // that file is exactly what protects against a
                    // man-in-the-middle presenting a different key. Only a
                    // deliberate, informed choice (e.g. the tablet was
                    // factory-reset or re-flashed) should ever clear it.
                    show_error_dialog_with_heading(
                        &window,
                        "Device identity changed",
                        "The reMarkable presented a different SSH host key than the one \
                         previously recorded for it. This normally means the tablet was \
                         reset or reinstalled — but it can also mean another device on the \
                         network is intercepting the connection.\n\n\
                         Only continue if you are sure this is your own tablet after a \
                         reset. To do so, remove its entry from \
                         ~/.config/rmsync/known_hosts yourself, then retry.",
                    );
                } else {
                    show_error_dialog_with_heading(&window, "SSH setup failed", &msg);
                }
            }
            Err(_) => {
                show_error_dialog_with_heading(
                    &window,
                    "SSH setup failed",
                    "Setup task exited unexpectedly.",
                );
            }
        }
    });
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
        let config_path = dir.path().join("cfg").join("config.toml");
        let sync = dir.path().join("sync");
        let cfg = Rc::new(RefCell::new(AppConfig::default()));

        apply_sync_dir_to(&cfg, &sync, &config_path).unwrap();

        assert_eq!(cfg.borrow().sync.sync_dir, sync);
        assert!(sync.join("raw").is_dir());
        assert!(sync.join(".rmsync").is_dir());

        // The new directory must actually reach the file, not just memory.
        let written = AppConfig::load_from(&config_path).unwrap();
        assert_eq!(written.sync.sync_dir, sync);
    }

    #[test]
    fn apply_sync_dir_writes_only_to_the_path_it_is_given() {
        // Regression test for MCC-51: this function used to resolve the
        // real user config internally, so running the test suite rewrote
        // the developer's own sync_dir to a temp path that was deleted
        // moments later, leaving the app pointing at nothing.
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let real_path = AppConfig::config_path();
        let real_before = std::fs::read(&real_path).ok();

        let cfg = Rc::new(RefCell::new(AppConfig::default()));
        apply_sync_dir_to(&cfg, &dir.path().join("sync"), &config_path).unwrap();

        assert!(config_path.exists(), "the given path should be written");
        assert_eq!(
            std::fs::read(&real_path).ok(),
            real_before,
            "the real user config at {} must be left untouched",
            real_path.display()
        );
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
        let config_path = dir.path().join("config.toml");
        assert!(apply_sync_dir_to(&cfg, &target, &config_path).is_err());
        assert!(
            !config_path.exists(),
            "a failed directory setup must not persist the new path"
        );
    }
}
