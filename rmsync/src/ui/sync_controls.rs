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

// Silence unused-import warning when gtk4-tests feature is off.
fn _silence_glib() {
    let _ = glib::ExitCode::SUCCESS;
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
