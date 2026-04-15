//! GtkApplication subclass and top-level window wiring.
//!
//! `RmSyncApp` owns the `adw::Application`, loads config at startup, builds
//! the main window on `activate`, and persists window/pane dimensions when
//! the window is closed.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::config::AppConfig;
use crate::ui::sync_controls::setup_folder_selector;
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
            setup_folder_selector(
                &main.browse_button,
                &main.sync_path_entry,
                &main.window,
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
