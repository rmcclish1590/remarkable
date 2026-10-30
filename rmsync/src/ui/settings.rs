//! Settings dialog — logging (MCC-50, MCC-131), device, connection and library.
//!
//! The level applies the moment it is picked (no restart, no OK button)
//! and is written to the config file so it survives one. The dialog also
//! surfaces where the log file lives, because the usual reason to open
//! this dialog is to raise the level, reproduce a problem, and then send
//! someone the log.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::sync::cleanup;
use crate::sync::state_db::StateDb;
use crate::logging::{LogHandle, LogLevel, MAX_LOG_BYTES, MAX_LOG_FILES};
use crate::ui::log_viewer;

/// Build and present the settings dialog.
pub fn present_settings(
    parent: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    log_handle: LogHandle,
) {
    let window = adw::PreferencesWindow::builder()
        .title("Settings")
        .transient_for(parent)
        .modal(true)
        .search_enabled(false)
        .build();

    let page = adw::PreferencesPage::builder().title("Logging").build();
    let group = adw::PreferencesGroup::builder()
        .title("Logging")
        .description(
            "Raise the level to capture more detail when reproducing a problem. \
             Higher levels write more to disk and can slow a large sync down.",
        )
        .build();

    let labels: Vec<&str> = LogLevel::ALL.iter().map(|l| l.label()).collect();
    let model = gtk::StringList::new(&labels);
    let current = config.borrow().logging.level;
    let selected = LogLevel::ALL
        .iter()
        .position(|l| *l == current)
        .unwrap_or(0) as u32;

    let row = adw::ComboRow::builder()
        .title("Log level")
        .subtitle(current.description())
        .model(&model)
        .selected(selected)
        .build();

    let config_for_row = config.clone();
    let row_for_notify = row.clone();
    let log_handle_for_row = log_handle.clone();
    row.connect_selected_notify(move |r| {
        let Some(level) = LogLevel::ALL.get(r.selected() as usize).copied() else {
            return;
        };
        row_for_notify.set_subtitle(level.description());

        if let Err(e) = log_handle_for_row.set_level(level) {
            tracing::error!(error = %e, "could not apply log level");
            return;
        }

        let mut cfg = config_for_row.borrow_mut();
        cfg.logging.level = level;
        // Persist immediately: the point of the setting is that it
        // survives the restart the user is probably about to do.
        if let Err(e) = cfg.save() {
            tracing::error!(error = format!("{e:#}"), "could not save log level");
        } else {
            tracing::info!(level = level.as_str(), "log level saved");
        }
    });
    group.add(&row);

    // Where the log lives, and how big it is allowed to get.
    group.add(&log_file_row(&window, log_handle.path()));

    let limits = adw::ActionRow::builder()
        .title("Retention")
        .subtitle(format!(
            "Rotates at {} MB, keeping {} previous file{}.",
            MAX_LOG_BYTES / (1024 * 1024),
            MAX_LOG_FILES,
            if MAX_LOG_FILES == 1 { "" } else { "s" }
        ))
        .build();
    group.add(&limits);

    page.add(&group);
    page.add(&device_group(config.clone()));
    page.add(&connection_group(parent, config.clone()));
    page.add(&library_group(&window, config));
    window.add(&page);
    window.present();
}

/// The log file's full path, with a button that opens a read-only copy of it
/// in the default text viewer (MCC-131). Activating the row (click, Enter or
/// Space) presses the button.
fn log_file_row(window: &adw::PreferencesWindow, path: Option<&Path>) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Log file").build();
    let Some(path) = path else {
        row.set_subtitle("Not available — logging to terminal only");
        return row;
    };
    // Subtitles are markup; a path may contain '&' or '<'.
    row.set_subtitle(&format!(
        "{}\nOpens a read-only copy; the newest entries are at the bottom.",
        glib::markup_escape_text(&path.display().to_string())
    ));
    row.set_tooltip_text(Some(log_viewer::OPEN_TOOLTIP));

    let button = gtk::Button::builder()
        .label("Open…")
        .valign(gtk::Align::Center)
        .tooltip_text(log_viewer::OPEN_TOOLTIP)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(log_viewer::OPEN_TOOLTIP)]);
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));

    let (window, path) = (window.clone(), path.to_path_buf());
    button.connect_clicked(move |b| log_viewer::open_log_file(&window, b, path.clone()));
    row
}

/// "Device" group: whether a sync that changed the tablet restarts its screen.
fn device_group(config: Rc<RefCell<AppConfig>>) -> adw::PreferencesGroup {
    let config_for_days = config.clone();
    let group = adw::PreferencesGroup::builder().title("Device and backups").build();
    let row = adw::ActionRow::builder()
        .title("Refresh the reMarkable screen after changes")
        .subtitle(
            "After a sync that adds, changes or removes documents on the tablet, its \
             interface restarts so they appear. This ends whatever is open on the screen, \
             so unsaved work there could be lost; it is skipped if the tablet was edited in \
             the last two minutes and never happens when nothing changed. The tablet itself \
             is never rebooted.",
        )
        .build();
    let switch = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .active(config.borrow().sync.restart_device_ui)
        .build();
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));
    switch.connect_active_notify(move |sw| {
        let mut cfg = config.borrow_mut();
        cfg.sync.restart_device_ui = sw.is_active();
        if let Err(e) = cfg.save() {
            tracing::error!(error = format!("{e:#}"), "could not save device refresh setting");
        } else {
            tracing::info!(enabled = sw.is_active(), "device refresh setting saved");
        }
    });
    group.add(&row);

    let retention = adw::ActionRow::builder()
        .title("Keep backups of deleted notebooks")
        .subtitle(
            "Days to keep the copy made when you delete a notebook, before it is removed \
             for good. 0 keeps them forever. Backups are in the sync folder under \
             .rmsync/backups/user-deleted.",
        )
        .build();
    let days = gtk::SpinButton::with_range(0.0, 3650.0, 1.0);
    days.set_valign(gtk::Align::Center);
    days.set_value(config_for_days.borrow().sync.deleted_backup_retention_days as f64);
    days.connect_value_changed(move |sb| {
        let mut cfg = config_for_days.borrow_mut();
        cfg.sync.deleted_backup_retention_days = sb.value() as u32;
        if let Err(e) = cfg.save() {
            tracing::error!(error = format!("{e:#}"), "could not save backup retention");
        } else {
            tracing::info!(days = sb.value() as u32, "backup retention saved");
        }
    });
    retention.add_suffix(&days);
    group.add(&retention);
    group
}

/// "Connection" group: guided setup of a new SSH key and pinned host key.
fn connection_group(
    parent: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Connection").build();
    let row = adw::ActionRow::builder()
        .title("Set up new connection")
        .subtitle(
            "Create a new SSH key and trust the tablet's current identity. Use this \
             after a software update, factory reset or host key warning.",
        )
        .activatable(true)
        .build();
    let button = gtk::Button::builder()
        .label("Set Up…")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&button);
    let parent = parent.clone();
    button.connect_clicked(move |_| {
        crate::ui::connection_setup::present_connection_setup(&parent, config.clone(), || {});
    });
    group.add(&row);
    group
}

/// "Library" group: one-off cleanup of the "(conflict ...)" duplicates that
/// earlier versions of the sync created (MCC-104). Nothing changes until the
/// user has seen the list and confirmed it.
fn library_group(
    window: &adw::PreferencesWindow,
    config: Rc<RefCell<AppConfig>>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Library").build();
    let row = adw::ActionRow::builder()
        .title("Clean up conflict duplicates")
        .subtitle(
            "Find “(conflict YYYY-MM-DD)” copies left by earlier syncs. \
             You review the list first; removed copies are kept in a backup folder.",
        )
        .build();
    let button = gtk::Button::builder()
        .label("Review…")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));

    let window = window.clone();
    button.connect_clicked(move |b| {
        let sync_dir = config.borrow().sync.sync_dir.clone();
        b.set_sensitive(false);
        let (window, button) = (window.clone(), b.clone());
        gtk::glib::spawn_future_local(async move {
            let dir = sync_dir.clone();
            let planned = gtk::gio::spawn_blocking(move || cleanup::plan_cleanup(&dir)).await;
            match planned {
                Ok(Ok(findings)) if findings.is_empty() => {
                    message(&window, "Nothing to clean up", "No conflict duplicates were found.");
                    button.set_sensitive(true);
                }
                Ok(Ok(findings)) => confirm_cleanup(&window, &button, sync_dir, findings),
                Ok(Err(e)) => {
                    tracing::error!(error = format!("{e:#}"), "conflict duplicate scan failed");
                    message(&window, "Scan failed", &format!("{e:#}"));
                    button.set_sensitive(true);
                }
                Err(_) => button.set_sensitive(true),
            }
        });
    });
    group.add(&row);
    group
}

fn confirm_cleanup(
    window: &adw::PreferencesWindow,
    button: &gtk::Button,
    sync_dir: std::path::PathBuf,
    findings: Vec<cleanup::DuplicateFinding>,
) {
    let list = findings.iter().map(|f| format!("• {}", f.describe())).collect::<Vec<_>>().join("\n");
    let label = gtk::Label::builder()
        .label(&list)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&label)
        .min_content_height(120)
        .max_content_height(320)
        .propagate_natural_height(true)
        .build();

    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading(format!("Remove {} conflict duplicate(s)?", findings.len()))
        .body(
            "Nothing is deleted: each duplicate is moved to the backup folder. The next sync \
             also removes it from your reMarkable.",
        )
        .extra_child(&scroller)
        .build();
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("clean", "_Clean up");
    dialog.set_response_appearance("clean", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let (window, button) = (window.clone(), button.clone());
    dialog.connect_response(None, move |_, response| {
        if response != "clean" {
            tracing::info!("conflict duplicate cleanup cancelled");
            button.set_sensitive(true);
            return;
        }
        let (window, button) = (window.clone(), button.clone());
        let (sync_dir, findings) = (sync_dir.clone(), findings.clone());
        gtk::glib::spawn_future_local(async move {
            // rusqlite is !Sync, so the DB is opened on the worker thread.
            let result = gtk::gio::spawn_blocking(move || -> anyhow::Result<cleanup::CleanupReport> {
                let db_path = sync_dir.join(".rmsync").join("state.db");
                if let Some(parent) = db_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let db = StateDb::open(&db_path)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(cleanup::apply_cleanup(&sync_dir, &db, &findings, now))
            })
            .await;
            match result {
                Ok(Ok(report)) if report.errors.is_empty() => {
                    message(&window, "Cleanup complete", &report.summary())
                }
                Ok(Ok(report)) => message(
                    &window,
                    "Cleanup finished with errors",
                    &format!("{}\n\n{}", report.summary(), report.errors.join("\n")),
                ),
                Ok(Err(e)) => message(&window, "Cleanup failed", &format!("{e:#}")),
                Err(_) => {}
            }
            button.set_sensitive(true);
        });
    });
    dialog.present();
}

fn message(window: &adw::PreferencesWindow, heading: &str, body: &str) {
    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .modal(true)
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("ok", "_OK");
    dialog.set_default_response(Some("ok"));
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_has_a_distinct_label_and_description() {
        let mut labels: Vec<&str> = LogLevel::ALL.iter().map(|l| l.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), LogLevel::ALL.len());

        let mut descriptions: Vec<&str> =
            LogLevel::ALL.iter().map(|l| l.description()).collect();
        descriptions.sort_unstable();
        descriptions.dedup();
        assert_eq!(descriptions.len(), LogLevel::ALL.len());
    }

    #[test]
    fn default_level_is_present_in_the_selector_list() {
        // The dialog falls back to index 0 when the configured level is
        // missing from ALL; that fallback must never be the normal path.
        assert!(LogLevel::ALL.contains(&LogLevel::default()));
    }
}
