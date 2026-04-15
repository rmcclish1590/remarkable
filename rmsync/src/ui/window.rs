//! Main application window (root of the widget tree).
//!
//! Three-panel layout: an `adw::HeaderBar` up top with device status and sync
//! button placeholders, a toolbar row with the sync-destination entry and
//! browse button, and a resizable `GtkPaned` splitting sidebar/viewer.

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::ui::device_status::DeviceStatusWidget;
use crate::ui::folder_browser::FolderBrowser;
use crate::ui::sync_controls::SyncControls;

const MIN_SIDEBAR_WIDTH: i32 = 200;
const MIN_VIEWER_WIDTH: i32 = 400;

pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub header_bar: adw::HeaderBar,
    pub sync_path_entry: gtk::Entry,
    pub browse_button: gtk::Button,
    pub last_sync_label: gtk::Label,
    pub paned: gtk::Paned,
    pub sidebar_scroll: gtk::ScrolledWindow,
    pub viewer_scroll: gtk::ScrolledWindow,
    pub device_status: DeviceStatusWidget,
    pub sync_controls: SyncControls,
    pub folder_browser: FolderBrowser,
}

impl MainWindow {
    pub fn new(app: &adw::Application, config: &AppConfig) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("rmSync")
            .default_width(config.ui.window_width)
            .default_height(config.ui.window_height)
            .build();

        let device_status = DeviceStatusWidget::new();
        let sync_controls = SyncControls::new();
        let header_bar = adw::HeaderBar::builder()
            .title_widget(&gtk::Label::builder().label("rmSync").build())
            .build();
        header_bar.pack_start(&device_status.widget);
        header_bar.pack_end(&sync_controls.sync_button);

        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let sync_to_label = gtk::Label::builder().label("Sync to:").build();
        let sync_path_entry = gtk::Entry::builder()
            .text(config.sync.sync_dir.to_string_lossy())
            .editable(false)
            .hexpand(true)
            .build();
        let browse_button = gtk::Button::builder().label("Browse").build();
        let sep = gtk::Separator::new(gtk::Orientation::Vertical);
        let last_sync_label = gtk::Label::builder().label("Last sync: Never").build();
        toolbar.append(&sync_to_label);
        toolbar.append(&sync_path_entry);
        toolbar.append(&browse_button);
        toolbar.append(&sep);
        toolbar.append(&last_sync_label);

        let folder_browser = FolderBrowser::new();
        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .width_request(MIN_SIDEBAR_WIDTH)
            .child(&folder_browser.widget)
            .build();
        sidebar_scroll.add_css_class("sidebar");

        let viewer_placeholder = gtk::Label::builder()
            .label("Select a document to view")
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let viewer_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .width_request(MIN_VIEWER_WIDTH)
            .child(&viewer_placeholder)
            .build();

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar_scroll)
            .end_child(&viewer_scroll)
            .resize_start_child(false)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(config.ui.sidebar_width)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&header_bar);
        root.append(&toolbar);
        root.append(&paned);
        root.append(&sync_controls.progress_box);
        paned.set_vexpand(true);
        window.set_content(Some(&root));

        Self {
            window,
            header_bar,
            sync_path_entry,
            browse_button,
            last_sync_label,
            paned,
            sidebar_scroll,
            viewer_scroll,
            device_status,
            sync_controls,
            folder_browser,
        }
    }

    pub fn sidebar_container(&self) -> &gtk::ScrolledWindow {
        &self.sidebar_scroll
    }

    pub fn viewer_container(&self) -> &gtk::ScrolledWindow {
        &self.viewer_scroll
    }

    pub fn set_sync_path(&self, path: &str) {
        self.sync_path_entry.set_text(path);
    }

    pub fn set_last_sync(&self, timestamp: Option<u64>) {
        let text = match timestamp {
            None => "Last sync: Never".to_string(),
            Some(ts) => format!("Last sync: {}", format_unix_time(ts)),
        };
        self.last_sync_label.set_text(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_time_known_timestamp() {
        // 2025-06-15 12:34 UTC = 1750000440
        let s = format_unix_time(1_750_000_440);
        assert!(s.starts_with("2025-06-15"));
    }

    #[test]
    fn format_unix_time_epoch() {
        assert_eq!(format_unix_time(0), "1970-01-01 00:00");
    }
}

fn format_unix_time(ts: u64) -> String {
    // Reuse engine's civil_from_days to avoid a chrono dependency.
    let days = (ts / 86400) as i64;
    let (y, m, d) = crate::sync::engine::civil_from_days_public(days);
    let secs_of_day = ts % 86400;
    let h = secs_of_day / 3600;
    let mi = (secs_of_day % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}
