//! Makes a small copy of a real library's database, for faster local test runs.
//!
//! `ref/client.realm` is a personal library of tens of thousands of beatmap sets, and the tests
//! that use it copy the whole 250 MB file every time they open it. This writes a copy holding
//! the first few hundred sets instead, which those tests prefer when it exists.
//!
//! The result is still personal data, so it stays in `ref/` and out of the repository. Tests
//! that must run everywhere use `cleaner_realm::fixture` instead, which needs no library at all.
//!
//! ```
//! cargo run -p cleaner-realm --features test-support --example slim -- \
//!     ref/client.realm ref/client-slim.realm 400
//! ```

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a developer tool reports what it did"
)]

use cleaner_realm::{Realm, fixture};
use std::path::Path;
use std::process::ExitCode;

/// Beatmap sets to keep when the count is not given.
const DEFAULT_SETS: usize = 400;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(source), Some(destination)) = (args.next(), args.next()) else {
        eprintln!("usage: slim <source.realm> <destination.realm> [sets]");
        return ExitCode::FAILURE;
    };
    let keep = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SETS);

    match slim(Path::new(&source), Path::new(&destination), keep) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Copies `source`, keeps the first `keep` beatmap sets, and compacts the result.
fn slim(source: &Path, destination: &Path, keep: usize) -> Result<(), Box<dyn std::error::Error>> {
    for suffix in ["", ".lock", ".note"] {
        let _ = std::fs::remove_file(with_suffix(destination, suffix));
    }
    let _ = std::fs::remove_dir_all(with_suffix(destination, ".management"));

    std::fs::copy(source, destination)?;
    let before = std::fs::metadata(destination)?.len();

    let removed = {
        let realm = Realm::open_for_write(destination)?;
        let removed = fixture::keep_first_sets(&realm, keep)?;
        realm.compact()?;
        removed
    };

    let after = std::fs::metadata(destination)?.len();
    println!(
        "kept {keep} beatmap sets, removed {removed}; {} -> {}",
        cleaner_bytes(before),
        cleaner_bytes(after)
    );
    Ok(())
}

/// Appends a sidecar suffix to a database path.
fn with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Rounds a byte count to one decimal, in the largest unit that keeps it above one.
fn cleaner_bytes(bytes: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "display-only approximation of a size"
    )]
    let mut value = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} TB")
}
