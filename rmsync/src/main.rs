//! rmSync — bi-directional sync for reMarkable 2 tablets.
//!
//! Entry point: initialises logging, builds the libadwaita application,
//! and launches the main window.

mod app;
mod config;
mod device;
mod error;
mod remarkable;
mod sync;
mod ui;

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt().init();
    let exit_code = app::RmSyncApp::new().run();
    std::process::ExitCode::from(exit_code as u8)
}
