//! Library facade so integration tests in `tests/` can reach the internal
//! modules. Binary entrypoint lives in `main.rs` and pulls these via
//! `rmsync::<module>`.

pub mod app;
pub mod config;
pub mod device;
pub mod error;
pub mod logging;
pub mod remarkable;
pub mod sync;
pub mod ui;
