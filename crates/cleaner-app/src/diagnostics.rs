//! Logging and panic reporting, through `app-diagnostics`.
//!
//! A desktop application has nowhere to print, so a log file is the only way to find out what
//! happened after the fact. Every run appends to a dated file in [`logs_dir`], which the About
//! window opens, and a crash that happened yesterday is still there today.

use std::path::PathBuf;

/// The folder this app keeps its own data in, and the prefix of its log files.
///
/// `%LOCALAPPDATA%\osu-lazer-cleaner` on Windows, `$XDG_DATA_HOME/osu-lazer-cleaner` elsewhere.
/// eframe keeps the window's saved state in the same folder, because `eframe::storage_dir`
/// follows the same rule.
const APP_DIR: &str = "osu-lazer-cleaner";

/// The folder the log files go to.
pub(crate) fn logs_dir() -> Option<PathBuf> {
    app_folders::app_data_dir(APP_DIR).map(|dir| dir.join("logs"))
}

/// Starts logging and installs a panic hook that records the panic before the process dies.
///
/// The file gets everything at `info`. Set `RUST_LOG` to override the level. Without a file,
/// logs go to stderr instead.
pub(crate) fn install() {
    let logs = app_diagnostics::init(logs_dir().as_deref(), APP_DIR, "info");
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        logs = logs.as_ref().map(|dir| dir.display().to_string()),
        "osu-lazer-cleaner starting"
    );
}
