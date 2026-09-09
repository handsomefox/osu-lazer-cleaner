//! Error types, one per concern.

use std::path::PathBuf;

/// Failures while locating an osu!lazer library.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The directory exists but holds no `client.realm`.
    #[error("no osu!lazer database at {path}")]
    NotALibrary {
        /// Directory that was checked.
        path: PathBuf,
    },

    /// None of the default locations held a library.
    #[error("no osu!lazer library found; looked in {}", format_paths(tried))]
    NotFound {
        /// Directories that were checked.
        tried: Vec<PathBuf>,
    },
}

/// Failures while scanning a library.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The database could not be read.
    #[error("could not read the osu!lazer database: {0}")]
    Realm(#[from] cleaner_realm::RealmError),

    /// A file operation failed.
    #[error("{action} failed for {path}: {source}")]
    Io {
        /// What was being attempted.
        action: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying cause.
        source: std::io::Error,
    },

    /// osu!lazer is running, or another process holds the database.
    #[error("the osu!lazer database is in use; close osu!lazer and try again")]
    DatabaseInUse,
}

/// Failures while creating, restoring, or deleting a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Another cleaner process is changing this library.
    #[error("another cleaner operation is using this library; wait for it to finish")]
    OperationInProgress,

    /// Another snapshot depends on files this deletion would remove.
    #[error(
        "snapshot {dependent} needs files from {snapshot}; restore the newer snapshot first, or delete {dependent} first"
    )]
    SnapshotDependency {
        /// Snapshot whose deletion was requested.
        snapshot: String,
        /// Retained snapshot that needs these files.
        dependent: String,
    },

    /// A restored file does not match its recovery data.
    #[error("{path} does not match the snapshot; keeping the snapshot")]
    BlobMismatch {
        /// File that failed verification.
        path: PathBuf,
    },
    /// The database no longer matches the selected scan results.
    #[error("the library changed since the scan; scan again before cleaning")]
    StalePlan,
    /// A file operation failed.
    #[error("{action} failed for {path}: {source}")]
    Io {
        /// What was being attempted.
        action: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying cause.
        source: std::io::Error,
    },

    /// The database could not be updated.
    #[error("could not update the osu!lazer database: {0}")]
    Realm(#[from] cleaner_realm::RealmError),

    /// A manifest could not be read or written.
    #[error("snapshot manifest at {path} is not readable: {source}")]
    Manifest {
        /// Path of the manifest.
        path: PathBuf,
        /// Underlying cause.
        source: serde_json::Error,
    },

    /// The manifest was written by an incompatible version.
    #[error(
        "snapshot at {path} uses format version {found}, but this build understands {supported}"
    )]
    UnsupportedFormat {
        /// Path of the snapshot.
        path: PathBuf,
        /// Version found in the manifest.
        found: u32,
        /// Version this build writes.
        supported: u32,
    },

    /// The snapshot directory and the library are on different volumes.
    #[error(
        "snapshots must sit on the same volume as {library}, because a snapshot holds links to \
         the library's own files rather than copies of them"
    )]
    CrossVolume {
        /// The library root.
        library: PathBuf,
    },

    /// A path fell outside the directories this tool may touch.
    #[error("refusing to touch {path}: outside the osu!lazer library")]
    OutsideLibrary {
        /// Offending path.
        path: PathBuf,
    },

    /// The snapshot does not hold the file the library is about to give up.
    #[error("refusing to remove a file the snapshot does not hold: {path} should be {bytes} bytes")]
    NotPreserved {
        /// Where the snapshot should hold the file.
        path: PathBuf,
        /// Size the snapshot's copy must have.
        bytes: u64,
    },
}

/// Renders a path list for an error message.
fn format_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "no candidate directories".to_owned();
    }

    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders an error with every source behind it.
///
/// `Display` alone shows the outermost message, which is usually the one that says least. A log
/// line that has to be enough to diagnose from needs the chain.
#[must_use]
pub fn chain(error: &dyn std::error::Error) -> String {
    use std::fmt::Write as _;

    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        // Writing to a String cannot fail, so the result carries no information.
        let _ = write!(text, ": {current}");
        source = current.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::chain;

    #[test]
    fn a_chain_carries_every_source() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access is denied");
        let error = super::SnapshotError::Io {
            action: "linking a file into the snapshot",
            path: std::path::PathBuf::from("/library/files/a/ab/abcdef"),
            source: io,
        };

        let text = chain(&error);
        assert!(text.contains("linking a file into the snapshot"), "{text}");
        assert!(text.ends_with("access is denied"), "{text}");
    }

    #[test]
    fn an_error_with_no_source_is_just_its_message() {
        let error = super::SnapshotError::OperationInProgress;
        assert_eq!(chain(&error), error.to_string());
    }
}
