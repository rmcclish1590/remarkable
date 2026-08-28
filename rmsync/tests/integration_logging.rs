//! End-to-end check of the logging subsystem (MCC-50).
//!
//! `logging::init` installs a *global* subscriber, so it can only run once
//! per process — hence a dedicated integration binary with a single test
//! that walks the whole path: install the subscriber, emit at every level,
//! change the level at runtime, and confirm the file on disk matches.

use std::fs;

use rmsync::logging::{self, LogLevel, LOG_DIR_ENV};

#[test]
fn logging_writes_a_structured_file_and_respects_level_changes() {
    let dir = tempfile::tempdir().expect("temp dir");
    // SAFETY: single-threaded test, set before the subscriber reads it.
    unsafe { std::env::set_var(LOG_DIR_ENV, dir.path()) };

    let handle = logging::init(LogLevel::Production);
    let log_path = handle.path().expect("log file should be open").to_path_buf();
    assert!(
        log_path.starts_with(dir.path()),
        "the env override should redirect the log: {}",
        log_path.display()
    );

    // Messages are attributed to `rmsync::…` the way the real modules are;
    // the level applies per-crate, so emitting from this test binary's own
    // target would exercise the dependency filter instead.
    const APP: &str = "rmsync::sync::engine";

    // --- Production: app info through, app debug suppressed ---
    tracing::info!(target: APP, marker = "prod_info", "visible at production level");
    tracing::debug!(target: APP, marker = "prod_debug", "should be filtered out");
    tracing::error!(target: APP, marker = "prod_error", "errors always appear");
    // A dependency at info level stays quiet in Production — that is the
    // difference between Production and Information.
    tracing::info!(target: "russh::client", marker = "dep_info", "dependency chatter");

    let contents = read_log(&log_path);
    assert!(contents.contains("prod_info"), "info missing: {contents}");
    assert!(contents.contains("prod_error"), "error missing: {contents}");
    assert!(
        !contents.contains("prod_debug"),
        "debug must be filtered at production level: {contents}"
    );
    assert!(
        !contents.contains("dep_info"),
        "dependency info must be filtered at production level: {contents}"
    );

    // Structure: timestamp, level, and the emitting component.
    let info_line = contents
        .lines()
        .find(|l| l.contains("prod_info"))
        .expect("info line");
    assert!(info_line.contains("INFO"), "level missing: {info_line}");
    assert!(
        info_line.contains(APP),
        "component/target missing: {info_line}"
    );
    assert!(
        info_line.starts_with("2"),
        "line should start with an ISO-ish timestamp: {info_line}"
    );

    // --- Information: dependencies become audible ---
    handle.set_level(LogLevel::Information).expect("level change");
    tracing::info!(target: "russh::client", marker = "dep_info_visible", "dependency chatter");
    let contents = read_log(&log_path);
    assert!(
        contents.contains("dep_info_visible"),
        "dependency info should appear at information level: {contents}"
    );

    // --- Raise to Debug at runtime: no restart ---
    handle.set_level(LogLevel::Debug).expect("level change");
    tracing::debug!(target: APP, marker = "debug_after_change", "now visible");
    let contents = read_log(&log_path);
    assert!(
        contents.contains("debug_after_change"),
        "debug should appear after raising the level: {contents}"
    );

    // --- Drop to ErrorsOnly: info suppressed again ---
    handle.set_level(LogLevel::ErrorsOnly).expect("level change");
    tracing::info!(target: APP, marker = "info_after_quiet", "should be filtered out");
    tracing::error!(target: APP, marker = "error_after_quiet", "still visible");
    let contents = read_log(&log_path);
    assert!(
        !contents.contains("info_after_quiet"),
        "info must be suppressed at errors-only: {contents}"
    );
    assert!(contents.contains("error_after_quiet"));
}

fn read_log(path: &std::path::Path) -> String {
    // The writer appends synchronously per line, so no flush wait is
    // needed; read as bytes in case a partial multi-byte write lands.
    String::from_utf8_lossy(&fs::read(path).expect("read log")).into_owned()
}
