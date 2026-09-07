//! Snapshots, which make a clean reversible.
//!
//! A clean never deletes anything. It detaches usages from the database and moves the orphaned
//! blobs into a snapshot directory. Moving is a rename, so it is atomic and costs no extra
//! disk, which matters when a clean displaces tens of gigabytes. The bytes come back on
//! restore, and the space is reclaimed when the user deletes the snapshot.

use crate::error::SnapshotError;
use crate::plan::Candidate;
use crate::safety::is_safe_path;
use crate::storage::{Library, blob_relative_path};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Manifest format version. Bumped whenever [`Manifest`] changes shape.
pub const FORMAT_VERSION: u32 = 1;

/// Name of the manifest inside a snapshot directory.
const MANIFEST_FILE: &str = "manifest.json";

/// Directory holding the moved blobs.
const BLOBS_DIR: &str = "blobs";

/// Prefix for a snapshot still being written.
const TEMP_PREFIX: &str = ".tmp-";

/// Everything needed to undo one clean.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version of this manifest.
    pub format_version: u32,
    /// When the snapshot was taken.
    pub created: jiff::Timestamp,
    /// Version of the tool that wrote it.
    pub app_version: String,
    /// Schema version of the database at the time.
    pub schema_version: u64,
    /// Usages that were detached, in the order they were removed.
    pub detached: Vec<Candidate>,
    /// Hashes of blobs moved into the snapshot, with their sizes.
    pub blobs: Vec<(String, u64)>,
}

impl Manifest {
    /// Total bytes the snapshot holds.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.blobs.iter().map(|(_, size)| size).sum()
    }
}

/// A snapshot on disk.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Directory holding it.
    pub dir: PathBuf,
    /// Its manifest.
    pub manifest: Manifest,
}

impl Snapshot {
    /// Identifier shown to the user, which is the directory name.
    #[must_use]
    pub fn id(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Lists snapshots in a library, oldest first.
///
/// Directories that are half-written or unreadable are skipped rather than failing the whole
/// listing, so one bad snapshot never hides the rest.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] only if the snapshots directory itself cannot be read.
pub fn list(library: &Library) -> Result<Vec<Snapshot>, SnapshotError> {
    let root = library.snapshots_dir();
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&root).map_err(|source| SnapshotError::Io {
        action: "listing snapshots",
        path: root.clone(),
        source,
    })?;

    let mut snapshots: Vec<Snapshot> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
        .filter_map(|entry| {
            let dir = entry.path();
            read_manifest(&dir)
                .ok()
                .map(|manifest| Snapshot { dir, manifest })
        })
        .collect();

    snapshots.sort_by_key(|s| s.manifest.created);
    Ok(snapshots)
}

/// Reads and validates one snapshot's manifest.
fn read_manifest(dir: &Path) -> Result<Manifest, SnapshotError> {
    let path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|source| SnapshotError::Io {
        action: "reading a snapshot manifest",
        path: path.clone(),
        source,
    })?;

    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|source| SnapshotError::Manifest {
            path: path.clone(),
            source,
        })?;

    if manifest.format_version != FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedFormat {
            path: dir.to_path_buf(),
            found: manifest.format_version,
            supported: FORMAT_VERSION,
        });
    }

    Ok(manifest)
}

/// Creates an empty snapshot directory and returns where to build it.
///
/// The directory is named `.tmp-*` until [`finalise`] renames it, so an interrupted clean
/// leaves nothing that [`list`] will show.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] if the directory cannot be created, or
/// [`SnapshotError::CrossVolume`] if a rename from the library would cross a filesystem
/// boundary.
pub fn begin(library: &Library) -> Result<PathBuf, SnapshotError> {
    let root = library.snapshots_dir();
    let blobs = root.join(format!(
        "{TEMP_PREFIX}{}",
        jiff::Timestamp::now().as_nanosecond()
    ));

    std::fs::create_dir_all(blobs.join(BLOBS_DIR)).map_err(|source| SnapshotError::Io {
        action: "creating a snapshot directory",
        path: blobs.clone(),
        source,
    })?;

    ensure_same_volume(library, &blobs)?;
    Ok(blobs)
}

/// Moves a blob out of the library and into a snapshot.
///
/// Uses a rename, so the move is atomic and frees no space until the snapshot is deleted.
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if the blob is not inside the library's `files/`
/// tree, or [`SnapshotError::Io`] if the rename fails for a reason other than the blob having
/// already gone.
pub fn stash_blob(
    library: &Library,
    snapshot_dir: &Path,
    hash: &str,
) -> Result<bool, SnapshotError> {
    let source = library.blob_path(hash);
    let files_dir = library.files_dir();

    // Re-check immediately before moving, independently of the scan-time check. The plan may
    // be minutes old by now.
    if !is_safe_path(&source, &[&files_dir]) {
        return Err(SnapshotError::OutsideLibrary { path: source });
    }

    if !source.exists() {
        // Already gone: lazer's own cleanup, or a previous run, got there first.
        return Ok(false);
    }

    let destination = snapshot_dir.join(BLOBS_DIR).join(hash);
    std::fs::rename(&source, &destination).map_err(|error| SnapshotError::Io {
        action: "moving a file into the snapshot",
        path: source,
        source: error,
    })?;

    Ok(true)
}

/// Writes the manifest and makes the snapshot visible.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] if the manifest cannot be written or the directory cannot be
/// renamed.
pub fn finalise(temp_dir: &Path, manifest: &Manifest) -> Result<PathBuf, SnapshotError> {
    let text =
        serde_json::to_string_pretty(manifest).map_err(|source| SnapshotError::Manifest {
            path: temp_dir.join(MANIFEST_FILE),
            source,
        })?;

    let manifest_path = temp_dir.join(MANIFEST_FILE);
    std::fs::write(&manifest_path, text).map_err(|source| SnapshotError::Io {
        action: "writing the snapshot manifest",
        path: manifest_path,
        source,
    })?;

    let final_dir = unique_dir(temp_dir, manifest.created);
    std::fs::rename(temp_dir, &final_dir).map_err(|source| SnapshotError::Io {
        action: "finalising the snapshot",
        path: temp_dir.to_path_buf(),
        source,
    })?;

    Ok(final_dir)
}

/// Moves a snapshot's blobs back into the library, in parallel.
///
/// Returns how many blobs were restored. Restoring the database rows is the caller's job,
/// because only it holds a writable realm.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] if a blob cannot be moved back.
///
/// # Panics
///
/// Panics if a worker thread panics while holding the shared error slot, which would mean the
/// move itself panicked rather than returning an error.
pub fn restore_blobs(
    library: &Library,
    snapshot: &Snapshot,
    progress: &mut impl FnMut(usize, usize),
) -> Result<usize, SnapshotError> {
    let blobs = &snapshot.manifest.blobs;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let failure: std::sync::Mutex<Option<SnapshotError>> = std::sync::Mutex::new(None);

    std::thread::scope(|scope| {
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);

        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((hash, _)) = blobs.get(index) else {
                        break;
                    };

                    if let Err(error) = restore_one(library, &snapshot.dir, hash) {
                        let mut slot = failure.lock().expect("restore mutex was poisoned");
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        break;
                    }

                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }

        // Report from this thread, so the caller's closure never has to be `Sync`.
        while next.load(std::sync::atomic::Ordering::Relaxed) < blobs.len() {
            progress(done.load(std::sync::atomic::Ordering::Relaxed), blobs.len());
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });

    if let Some(error) = failure.into_inner().expect("restore mutex was poisoned") {
        return Err(error);
    }

    Ok(done.load(std::sync::atomic::Ordering::Relaxed))
}

/// Moves one blob out of a snapshot and back into the library.
fn restore_one(library: &Library, snapshot_dir: &Path, hash: &str) -> Result<(), SnapshotError> {
    let source = snapshot_dir.join(BLOBS_DIR).join(hash);
    if !source.exists() {
        return Ok(());
    }

    let destination = library.blob_path(hash);
    if destination.exists() {
        // The blob came back another way, most likely a re-import. Ours is redundant.
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|source_error| SnapshotError::Io {
            action: "recreating a blob directory",
            path: parent.to_path_buf(),
            source: source_error,
        })?;
    }

    std::fs::rename(&source, &destination).map_err(|source_error| SnapshotError::Io {
        action: "restoring a file from the snapshot",
        path: source,
        source: source_error,
    })
}

/// Deletes a snapshot, reclaiming its space.
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if the snapshot is not directly inside the
/// library's snapshots directory, or [`SnapshotError::Io`] if removal fails.
pub fn delete(library: &Library, snapshot: &Snapshot) -> Result<(), SnapshotError> {
    let snapshots_dir = library.snapshots_dir();

    // Defence in depth, adapted from eldenring-backuptool's retention guard: a snapshot must
    // be exactly one level under the snapshots directory before anything recursive runs.
    if snapshot.dir.parent() != Some(snapshots_dir.as_path()) {
        return Err(SnapshotError::OutsideLibrary {
            path: snapshot.dir.clone(),
        });
    }

    if !is_safe_path(&snapshot.dir, &[&snapshots_dir]) {
        return Err(SnapshotError::OutsideLibrary {
            path: snapshot.dir.clone(),
        });
    }

    std::fs::remove_dir_all(&snapshot.dir).map_err(|source| SnapshotError::Io {
        action: "deleting a snapshot",
        path: snapshot.dir.clone(),
        source,
    })
}

/// Confirms a rename from the library into the snapshot directory stays on one volume.
///
/// `rename` fails across filesystems, and copying instead would need the space twice.
fn ensure_same_volume(library: &Library, snapshot_dir: &Path) -> Result<(), SnapshotError> {
    let probe = snapshot_dir.join(".volume-probe");
    std::fs::write(&probe, b"").map_err(|source| SnapshotError::Io {
        action: "checking the snapshot volume",
        path: probe.clone(),
        source,
    })?;

    let target = library.files_dir().join(".volume-probe");
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let same_volume = std::fs::rename(&probe, &target).is_ok();
    let _ = std::fs::remove_file(if same_volume { &target } else { &probe });

    if same_volume {
        Ok(())
    } else {
        Err(SnapshotError::CrossVolume {
            library: library.root().to_path_buf(),
        })
    }
}

/// Picks a final directory name, appending a counter if one already exists.
fn unique_dir(temp_dir: &Path, created: jiff::Timestamp) -> PathBuf {
    let parent = temp_dir.parent().unwrap_or(temp_dir);
    let stamp = created.strftime("%Y%m%d-%H%M%S").to_string();

    let mut candidate = parent.join(&stamp);
    let mut counter = 1;
    while candidate.exists() {
        candidate = parent.join(format!("{stamp}-{counter}"));
        counter += 1;
    }

    candidate
}

/// Where a blob lives inside the library, relative to `files/`. Re-exported for callers that
/// build paths without a [`Library`].
#[must_use]
pub fn blob_location(hash: &str) -> PathBuf {
    blob_relative_path(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library_with_blob(hash: &str, bytes: &[u8]) -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(crate::storage::DATABASE_FILENAME), b"stub").unwrap();

        let blob = root
            .join(crate::storage::FILES_DIRECTORY)
            .join(blob_relative_path(hash));
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, bytes).unwrap();

        let library = Library::open(root).unwrap();
        (dir, library)
    }

    fn manifest(blobs: Vec<(String, u64)>) -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            created: jiff::Timestamp::now(),
            app_version: "test".to_owned(),
            schema_version: 51,
            detached: Vec::new(),
            blobs,
        }
    }

    #[test]
    fn stashing_moves_the_blob_out_of_the_library() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let temp = begin(&library).unwrap();
        assert!(stash_blob(&library, &temp, &hash).unwrap());

        assert!(!library.blob_path(&hash).exists(), "blob should have moved");
        assert!(temp.join(BLOBS_DIR).join(&hash).exists());
    }

    #[test]
    fn stashing_a_missing_blob_is_not_an_error() {
        let hash = "b".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();

        std::fs::remove_file(library.blob_path(&hash)).unwrap();
        assert!(!stash_blob(&library, &temp, &hash).unwrap());
    }

    #[test]
    fn a_snapshot_round_trips() {
        let hash = "c".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let temp = begin(&library).unwrap();
        stash_blob(&library, &temp, &hash).unwrap();
        finalise(&temp, &manifest(vec![(hash.clone(), 7)])).unwrap();

        let snapshots = list(&library).unwrap();
        assert_eq!(snapshots.len(), 1);

        assert_eq!(
            restore_blobs(&library, &snapshots[0], &mut |_, _| {}).unwrap(),
            1
        );
        assert_eq!(
            std::fs::read(library.blob_path(&hash)).unwrap(),
            b"payload",
            "restored bytes must be identical"
        );
    }

    #[test]
    fn deleting_a_snapshot_reclaims_it() {
        let hash = "d".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let temp = begin(&library).unwrap();
        stash_blob(&library, &temp, &hash).unwrap();
        finalise(&temp, &manifest(vec![(hash, 7)])).unwrap();

        let snapshots = list(&library).unwrap();
        delete(&library, &snapshots[0]).unwrap();

        assert!(list(&library).unwrap().is_empty());
    }

    #[test]
    fn a_snapshot_outside_the_library_is_refused() {
        let hash = "e".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let elsewhere = tempfile::tempdir().unwrap();

        let snapshot = Snapshot {
            dir: elsewhere.path().to_path_buf(),
            manifest: manifest(Vec::new()),
        };

        assert!(matches!(
            delete(&library, &snapshot),
            Err(SnapshotError::OutsideLibrary { .. })
        ));
        assert!(elsewhere.path().exists(), "the directory must survive");
    }

    #[test]
    fn unfinished_snapshots_are_not_listed() {
        let hash = "f".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        begin(&library).unwrap();
        assert!(list(&library).unwrap().is_empty());
    }
}
