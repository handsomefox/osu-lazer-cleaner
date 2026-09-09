//! Logging and panic reporting.
//!
//! Modelled on dlss-updater. A desktop application has nowhere to print, so a log file is the
//! only way to find out what happened after the fact. Every run appends to a dated file in
//! `cleaner_core::folders::logs_dir`, which the About window opens, and a crash that happened
//! yesterday is still there today.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Days of log files to keep.
const MAX_LOG_FILES: usize = 7;

/// Starts logging and installs a panic hook that records the panic before the window dies.
///
/// Returns the log file path when file logging could be set up. Logging goes to stderr
/// otherwise, which is where it is useful anyway when the tool is run from a terminal.
pub(crate) fn install() -> Option<PathBuf> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let path = open_log_file();
    match path.as_ref().and_then(|path| {
        std::fs::File::options()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    }) {
        Some(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }

    install_panic_hook();
    path
}

/// Chooses today's log file, pruning older ones first.
fn open_log_file() -> Option<PathBuf> {
    let directory = cleaner_core::folders::logs_dir()?;
    let today = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
    prepare_log_file(&directory, &today)
}

fn prepare_log_file(directory: &Path, date: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(directory).ok()?;
    prune_old_logs(directory);
    Some(directory.join(format!("osu-lazer-cleaner-{date}.log")))
}

/// Deletes the oldest log files, so the directory holds at most [`MAX_LOG_FILES`] of them.
fn prune_old_logs(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };

    let mut logs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_log_file(path))
        .collect();
    if logs.len() < MAX_LOG_FILES {
        return;
    }

    // Dated names sort chronologically, and today's file is about to join them.
    logs.sort();
    let excess = logs.len() + 1 - MAX_LOG_FILES;
    for path in logs.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a path is one of our own log files, so pruning leaves everything else alone.
fn is_log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("osu-lazer-cleaner-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
        })
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\n{backtrace}");
        // The subscriber writes through a mutex-guarded file, so make a best effort to get the
        // message out before the process dies.
        let _ = std::io::stderr().flush();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todays_log_is_chosen_and_only_old_logs_of_ours_are_pruned() {
        let directory = tempfile::tempdir().unwrap();
        for day in 1..=9 {
            std::fs::write(
                directory
                    .path()
                    .join(format!("osu-lazer-cleaner-2026-09-{day:02}.log")),
                b"log",
            )
            .unwrap();
        }
        std::fs::write(directory.path().join("notes.txt"), b"keep").unwrap();
        std::fs::write(directory.path().join("other.log"), b"keep").unwrap();

        let chosen = prepare_log_file(directory.path(), "2026-09-10").unwrap();
        assert_eq!(
            chosen,
            directory.path().join("osu-lazer-cleaner-2026-09-10.log")
        );

        let kept: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_log_file(Path::new(name)))
            .collect();
        assert_eq!(kept.len(), MAX_LOG_FILES - 1);
        assert!(
            kept.iter()
                .all(|name| name.as_str() >= "osu-lazer-cleaner-2026-09-04.log")
        );
        assert!(directory.path().join("notes.txt").exists());
        assert!(directory.path().join("other.log").exists());
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, b"file").unwrap();

        prune_old_logs(&file);
        assert!(prepare_log_file(&file, "2026-09-10").is_none());
    }
}
