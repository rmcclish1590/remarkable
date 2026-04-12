//! rmSync — bi-directional sync for reMarkable 2 tablets.
//!
//! Entry point: initialises logging, builds the libadwaita application,
//! and launches the main window.

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

mod app;
mod config;
mod device;
mod error;
mod remarkable;
mod sync;
mod ui;

const APP_ID: &str = "com.rmsync.app";

fn main() -> gtk::glib::ExitCode {
    tracing_subscriber::fmt().init();

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let label = gtk::Label::builder()
        .label("rmSync — Ready")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("rmSync")
        .default_width(1200)
        .default_height(800)
        .content(&label)
        .build();

    window.present();
}
