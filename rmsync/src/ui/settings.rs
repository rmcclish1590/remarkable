//! Settings dialog — currently the logging controls (spec: MCC-50).
//!
//! The level applies the moment it is picked (no restart, no OK button)
//! and is written to the config file so it survives one. The dialog also
//! surfaces where the log file lives, because the usual reason to open
//! this dialog is to raise the level, reproduce a problem, and then send
//! someone the log.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::logging::{LogHandle, LogLevel, MAX_LOG_BYTES, MAX_LOG_FILES};

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
    let location = adw::ActionRow::builder()
        .title("Log file")
        .subtitle(
            log_handle
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "Not available — logging to terminal only".to_string()),
        )
        .build();
    group.add(&location);

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
    window.add(&page);
    window.present();
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
