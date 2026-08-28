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

use std::sync::OnceLock;

use tokio::runtime::Runtime;

use crate::config::AppConfig;
use crate::device::monitor::{DeviceEvent, DeviceMonitor};
use crate::logging::LogHandle;
use crate::remarkable::document::DocumentTree;
use crate::sync::engine::{SyncOrchestrator, SyncPhase, SyncProgressEvent};
use crate::ui::device_status::DeviceStatusWidget;
use crate::ui::folder_browser::FolderBrowser;
use crate::ui::sync_controls::{ensure_device_configured, setup_folder_selector, SyncControls};
use crate::ui::viewer::DocumentViewer;
use crate::ui::window::MainWindow;

/// Shared tokio runtime for background subscriptions (device monitor event
/// forwarding). The per-sync orchestrator runs on its own std::thread +
/// current-thread runtime since rusqlite is !Sync.
static MONITOR_RT: OnceLock<Runtime> = OnceLock::new();

fn monitor_runtime() -> &'static Runtime {
    MONITOR_RT.get_or_init(|| Runtime::new().expect("build monitor tokio runtime"))
}

pub const APP_ID: &str = "com.rmsync.app";

pub struct RmSyncApp {
    app: adw::Application,
}

impl RmSyncApp {
    pub fn new(log_handle: LogHandle) -> Self {
        let config = match AppConfig::load() {
            Ok(cfg) => {
                tracing::info!(
                    sync_dir = %cfg.sync.sync_dir.display(),
                    host = %cfg.device.host,
                    auto_sync = cfg.sync.auto_sync_on_connect,
                    "configuration loaded"
                );
                cfg
            }
            Err(e) => {
                tracing::error!(
                    error = format!("{e:#}"),
                    "could not load configuration; using defaults"
                );
                AppConfig::default()
            }
        };
        let config = Rc::new(RefCell::new(config));
        let app = adw::Application::builder()
            .application_id(APP_ID)
            .build();
        let config_for_activate = config.clone();
        app.connect_activate(move |app| {
            let cfg = config_for_activate.borrow().clone();
            tracing::debug!("building main window");
            let main = MainWindow::new(app, &cfg);
            wire_settings_button(
                &main.settings_button,
                &main.window,
                config_for_activate.clone(),
                log_handle.clone(),
            );
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
                main.window.clone(),
                config_for_activate.clone(),
            );
            setup_device_monitoring(
                &main.device_status,
                &main.sync_controls,
                &cfg,
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
                if let Err(e) = cfg.save() {
                    tracing::warn!(error = format!("{e:#}"), "could not save window geometry");
                }
                tracing::debug!(width, height, "main window closing");
                gtk::glib::Propagation::Proceed
            });
            tracing::info!("main window ready");
            main.window.present();
        });
        Self { app }
    }

    pub fn run(&self) -> i32 {
        self.app.run().value()
    }
}

fn wire_settings_button(
    button: &gtk::Button,
    window: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    log_handle: LogHandle,
) {
    let window = window.clone();
    button.connect_clicked(move |_| {
        tracing::debug!("opening settings");
        crate::ui::settings::present_settings(&window, config.clone(), log_handle.clone());
    });
}

fn load_folder_tree(browser: &FolderBrowser, config: &AppConfig) {
    let raw = config.sync.sync_dir.join("raw");
    if !raw.is_dir() {
        tracing::warn!(
            path = %raw.display(),
            "raw directory missing; sidebar will be empty until the first sync"
        );
        return;
    }
    match DocumentTree::build_from_directory(&raw) {
        Ok(tree) => {
            tracing::info!(
                documents = tree.flat_list().len(),
                path = %raw.display(),
                "loaded document tree"
            );
            browser.load_tree(&tree);
        }
        Err(e) => tracing::error!(
            path = %raw.display(),
            error = format!("{e:#}"),
            "could not build document tree"
        ),
    }
}

fn wire_folder_browser_to_viewer(
    browser: &FolderBrowser,
    viewer: &DocumentViewer,
    config: Rc<RefCell<AppConfig>>,
) {
    // DocumentViewer is Clone — its fields are GTK widget refs (cheap
    // GObject increments) and an Rc<RefCell<_>>. The closure runs on the
    // GTK main thread so `!Send` is not a concern.
    let viewer = viewer.clone();
    browser.connect_document_selected(move |uuid| {
        let sync_dir = config.borrow().sync.sync_dir.clone();
        if let Err(e) = viewer.load_document(&uuid, &sync_dir) {
            tracing::warn!("failed to open {uuid}: {e}");
            viewer.show_error("Could not open document", &e.to_string());
        }
    });
}

pub(crate) fn wire_sync_button(
    sync_controls: &SyncControls,
    folder_browser: &FolderBrowser,
    _viewer: &DocumentViewer,
    last_sync_label: gtk::Label,
    window: adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
) {
    let controls = sync_controls.clone();
    let browser = folder_browser.clone();
    sync_controls.connect_sync_clicked(move || {
        // If SSH is not yet set up, prompt for the root password first.
        // ensure_device_configured runs the sync-start closure only after
        // the keypair is successfully installed and saved.
        let controls = controls.clone();
        let browser = browser.clone();
        let config = config.clone();
        let last_sync_label = last_sync_label.clone();
        ensure_device_configured(&window, config.clone(), move || {
            start_sync(
                controls.clone(),
                browser.clone(),
                last_sync_label.clone(),
                config.clone(),
            );
        });
    });
}

fn start_sync(
    controls: SyncControls,
    browser: FolderBrowser,
    last_sync_label: gtk::Label,
    config: Rc<RefCell<AppConfig>>,
) {
    let cfg = config.borrow().clone();
    let (tx, rx) = async_channel::unbounded::<SyncProgressEvent>();
    controls.start_sync();

    // Sync state (rusqlite Connection) is not Sync, so we run the
    // orchestrator on a dedicated std::thread with its own current-thread
    // tokio runtime — the whole orchestrator lives on that thread and only
    // sends events back through the async channel.
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

    glib::spawn_future_local(async move {
        // Track whether any Error event fired so that a Complete with no
        // progress doesn't overwrite the visible error message with a
        // misleading "Sync complete: 0 pulled..." summary.
        let mut saw_error = false;
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
                    saw_error = true;
                    controls.show_error(&msg);
                }
                SyncProgressEvent::Complete(report) => {
                    if !report.errors.is_empty() {
                        controls.show_error(&report.errors[0]);
                    } else if saw_error {
                        // leave the error UI as-is; the earlier Error event
                        // already displayed a meaningful message.
                    } else {
                        controls.finish_sync(&report.summary());
                        last_sync_label.set_text(
                            &crate::ui::sync_controls::format_last_sync_relative(
                                Some(now_unix()),
                                now_unix(),
                            ),
                        );
                        let raw = config.borrow().sync.sync_dir.join("raw");
                        if let Ok(tree) = DocumentTree::build_from_directory(&raw) {
                            browser.load_tree(&tree);
                        }
                    }
                }
                SyncProgressEvent::ScanProgress(_) => {}
            }
        }
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

/// Create a DeviceMonitor, bind the status widget, and forward connect/
/// disconnect events to the sync controls. Also kicks off a one-shot
/// check_now so the UI reflects current state at startup.
pub(crate) fn setup_device_monitoring(
    device_status: &DeviceStatusWidget,
    sync_controls: &SyncControls,
    config: &AppConfig,
) {
    let conn_config = config.to_connection_config();
    let auto_sync = config.sync.auto_sync_on_connect;
    let runtime = monitor_runtime();

    // Build the monitor on the tokio runtime, then bubble two streams back
    // to the GTK main thread: one for the status widget (all events) and
    // one for the sync controls (connect/disconnect).
    let (event_tx_status, event_rx_status) = async_channel::unbounded::<DeviceEvent>();
    let (event_tx_sync, event_rx_sync) = async_channel::unbounded::<DeviceEvent>();

    runtime.spawn(async move {
        let (monitor, _rx) = DeviceMonitor::new(conn_config);
        let mut subscriber = monitor.subscribe();
        monitor.check_now().await;
        monitor.start();
        while let Ok(ev) = subscriber.recv().await {
            let _ = event_tx_status.send(ev.clone()).await;
            let _ = event_tx_sync.send(ev).await;
        }
    });

    let status = device_status.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = event_rx_status.recv().await {
            status.apply_event(ev);
        }
    });

    let controls = sync_controls.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = event_rx_sync.recv().await {
            match ev {
                DeviceEvent::Connected => {
                    controls.set_device_connected(true);
                    if auto_sync && !controls.is_syncing() {
                        controls.sync_button.emit_clicked();
                    }
                }
                DeviceEvent::Disconnected => {
                    controls.set_device_connected(false);
                }
                DeviceEvent::UsbDetected => {}
                DeviceEvent::ConnectionFailed(reason) => {
                    tracing::warn!("device connection failed: {reason}");
                }
            }
        }
    });
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
