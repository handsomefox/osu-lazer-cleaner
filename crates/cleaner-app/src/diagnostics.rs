//! Logging and panic reporting.
//!
//! Modelled on dlss-updater. A desktop application has nowhere to print, so a log file is the
//! only way to find out what happened after the fact.

use std::path::PathBuf;

/// Directory the log file goes in.
///
/// Beside the executable is wrong for an installed application, so this uses the per-user data
/// directory and falls back to the temporary directory when that is unavailable.
fn log_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
    };

    base.unwrap_or_else(std::env::temp_dir)
        .join("osu-lazer-cleaner")
}

/// Starts logging and installs a panic hook that records the panic before the window dies.
pub(crate) fn install() {
    let directory = log_dir();
    let _ = std::fs::create_dir_all(&directory);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    match std::fs::File::create(directory.join("osu-lazer-cleaner.log")) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        previous(info);
    }));
}
