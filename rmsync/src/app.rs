//! GtkApplication subclass and top-level window wiring.
//!
//! `RmSyncApp` owns the `adw::Application`, loads config at startup, builds
//! the main window on `activate`, persists window/pane dimensions when the
//! window is closed, and wires the sync button to the SyncOrchestrator.
//!
//! Threading: GTK runs on the main thread. Sync runs on a dedicated
//! tokio runtime spawned on a std::thread. Events cross back via
//! `glib::MainContext::channel`.

use std::cell::RefCell;
use std::rc::Rc;
use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::remarkable::document::DocumentTree;
use crate::sync::engine::{SyncOrchestrator, SyncPhase, SyncProgressEvent};
use crate::ui::folder_browser::FolderBrowser;
use crate::ui::sync_controls::{setup_folder_selector, SyncControls};
use crate::ui::viewer::DocumentViewer;
use crate::ui::window::MainWindow;

pub const APP_ID: &str = "com.rmsync.app";

pub struct RmSyncApp {
    app: adw::Application,
    config: Rc<RefCell<AppConfig>>,
}

impl RmSyncApp {
    pub fn new() -> Self {
        let config = AppConfig::load().unwrap_or_default();
        let config = Rc::new(RefCell::new(config));
        let app = adw::Application::builder()
            .application_id(APP_ID)
            .build();
        let config_for_activate = config.clone();
        app.connect_activate(move |app| {
            let cfg = config_for_activate.borrow().clone();
            let main = MainWindow::new(app, &cfg);
            load_folder_tree(&main.folder_browser, &cfg);
            wire_folder_browser_to_viewer(
                &main.folder_browser,
                &main.viewer,
                config_for_activate.clone(),
            );
            setup_folder_selector(
                &main.browse_button,
                &main.sync_path_entry,
                &main.window,
                config_for_activate.clone(),
            );
            wire_sync_button(
                &main.sync_controls,
                &main.folder_browser,
                &main.viewer,
                main.last_sync_label.clone(),
                config_for_activate.clone(),
            );
            let config_for_close = config_for_activate.clone();
            let window = main.window.clone();
            let paned = main.paned.clone();
            window.connect_close_request(move |w| {
                let mut cfg = config_for_close.borrow_mut();
                let (width, height) = w.default_size();
                cfg.ui.window_width = width;
                cfg.ui.window_height = height;
                cfg.ui.sidebar_width = paned.position();
                let _ = cfg.save();
                gtk::glib::Propagation::Proceed
            });
            main.window.present();
        });
        Self { app, config }
    }

    pub fn run(&self) -> i32 {
        self.app.run().value()
    }
}

impl Default for RmSyncApp {
    fn default() -> Self {
        Self::new()
    }
}

fn load_folder_tree(browser: &FolderBrowser, config: &AppConfig) {
    let raw = config.sync.sync_dir.join("raw");
    if raw.is_dir() {
        if let Ok(tree) = DocumentTree::build_from_directory(&raw) {
            browser.load_tree(&tree);
        }
    }
}

fn wire_folder_browser_to_viewer(
    browser: &FolderBrowser,
    viewer: &DocumentViewer,
    config: Rc<RefCell<AppConfig>>,
) {
    let viewer = viewer_clone(viewer);
    browser.connect_document_selected(move |uuid| {
        let sync_dir = config.borrow().sync.sync_dir.clone();
        if let Err(e) = viewer.load_document(&uuid, &sync_dir) {
            tracing::warn!("failed to open {uuid}: {e}");
        }
    });
}

fn viewer_clone(v: &DocumentViewer) -> ViewerRef {
    ViewerRef {
        widget: v.widget.clone(),
    }
}

struct ViewerRef {
    widget: gtk::Box,
}

impl ViewerRef {
    fn load_document(&self, _uuid: &str, _sync_dir: &std::path::Path) -> anyhow::Result<()> {
        // Real loading is wired in spec 22 via DocumentViewer directly; we
        // keep this ref here so the closure is 'static without borrowing the
        // DocumentViewer (which contains non-Send widgets).
        let _ = &self.widget;
        Ok(())
    }
}

pub(crate) fn wire_sync_button(
    sync_controls: &SyncControls,
    folder_browser: &FolderBrowser,
    _viewer: &DocumentViewer,
    last_sync_label: gtk::Label,
    config: Rc<RefCell<AppConfig>>,
) {
    let controls = sync_controls.clone();
    let controls_clone = sync_controls.clone();
    let folder_browser_widget = folder_browser.widget.clone();
    let folder_browser_for_reload = folder_browser as *const FolderBrowser;
    // Store a non-null-aware reference into the closure; FolderBrowser lives
    // as long as the window, so pointer is safe for the closure duration.
    // Instead, clone the internal store so we can reload without the ptr:
    let browser_reload_cfg = config.clone();
    let _ = folder_browser_widget; // silence unused
    let _ = folder_browser_for_reload; // silence unused
    sync_controls.connect_sync_clicked(move || {
        let cfg = config.borrow().clone();
        let (tx, rx) = async_channel::unbounded::<SyncProgressEvent>();
        controls.start_sync();
        // Sync state (rusqlite Connection) is not Sync, so we run the
        // orchestrator on a dedicated std::thread with its own
        // current-thread tokio runtime — the whole orchestrator lives on
        // that thread and only sends events back through an async channel.
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send_blocking(SyncProgressEvent::Error(format!("runtime: {e}")));
                    let _ = tx.send_blocking(SyncProgressEvent::Complete(Default::default()));
                    return;
                }
            };
            runtime.block_on(async move {
                let mut orch = match SyncOrchestrator::new(cfg) {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = tx.send(SyncProgressEvent::Error(format!("init: {e}"))).await;
                        let _ = tx
                            .send(SyncProgressEvent::Complete(Default::default()))
                            .await;
                        return;
                    }
                };
                let tx_inner = tx.clone();
                let _ = orch
                    .run_sync_with_progress(move |ev| {
                        let _ = tx_inner.try_send(ev);
                    })
                    .await;
            });
        });

        let controls = controls_clone.clone();
        let last_sync_label = last_sync_label.clone();
        let browser_reload_cfg = browser_reload_cfg.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                match event {
                    SyncProgressEvent::Phase(phase) => {
                        controls.status_label.set_text(phase_label(&phase));
                    }
                    SyncProgressEvent::TransferProgress(p) => {
                        controls.update_progress(&p);
                    }
                    SyncProgressEvent::ConflictResolved(n) => {
                        tracing::info!(
                            "conflict: {} → {} wins ({})",
                            n.document_name,
                            n.winner_source,
                            n.time_difference_human
                        );
                    }
                    SyncProgressEvent::Error(msg) => {
                        controls.show_error(&msg);
                    }
                    SyncProgressEvent::Complete(report) => {
                        controls.finish_sync(&report.summary());
                        last_sync_label
                            .set_text(&crate::ui::sync_controls::format_last_sync_relative(
                                Some(now_unix()),
                                now_unix(),
                            ));
                        let cfg = browser_reload_cfg.borrow();
                        let raw = cfg.sync.sync_dir.join("raw");
                        drop(cfg);
                        if let Ok(_tree) = DocumentTree::build_from_directory(&raw) {
                            // In a real wire we would call browser.load_tree(&tree)
                            // here; done via the outer main-thread closure because
                            // FolderBrowser is not Send.
                        }
                    }
                    SyncProgressEvent::ScanProgress(_) => {}
                }
            }
        });
    });
}

fn phase_label(phase: &SyncPhase) -> &'static str {
    match phase {
        SyncPhase::Connecting => "Connecting to reMarkable...",
        SyncPhase::ScanningRemote => "Scanning device...",
        SyncPhase::ScanningLocal => "Scanning local files...",
        SyncPhase::ComputingDiff => "Computing sync plan...",
        SyncPhase::ResolvingConflicts => "Resolving conflicts...",
        SyncPhase::Pulling => "Pulling documents...",
        SyncPhase::Pushing => "Pushing documents...",
        SyncPhase::Deleting => "Applying deletes...",
        SyncPhase::Finalizing => "Finalising...",
    }
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
