//! rmSync — bi-directional sync for reMarkable 2 tablets.
//!
//! Entry point: initialises logging, builds the libadwaita application,
//! and launches the main window.

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt().init();
    let exit_code = rmsync::app::RmSyncApp::new().run();
    std::process::ExitCode::from(exit_code as u8)
}
