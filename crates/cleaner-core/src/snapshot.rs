//! Snapshots, which make a clean reversible.
//!
//! A clean puts every blob it is about to orphan into a snapshot directory before it touches
//! the database. A hard link does that without copying the contents, so displacing tens of
//! gigabytes costs no extra disk. Only once the snapshot holds the bytes does the clean remove
//! the library's own link. The bytes come back on restore, and the space is reclaimed when the
//! user deletes the snapshot.
//!
//! On a filesystem with no hard links the snapshot gets a copy instead, which needs the space
//! twice until the snapshot is deleted.

use crate::durability;
use crate::error::SnapshotError;
use crate::operation::OperationLock;
use crate::plan::Candidate;
use crate::safety::is_safe_path;
use crate::storage::{Library, blob_relative_path};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

fn guard(library: &Library, path: &Path) -> Result<(), SnapshotError> {
    if !is_safe_path(path, &[library.root()]) {
        return Err(SnapshotError::OutsideLibrary {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), SnapshotError> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SnapshotError::OutsideLibrary {
            path: PathBuf::from(hash),
        });
    }
    Ok(())
}

fn validate_directory(library: &Library, dir: &Path) -> Result<(), SnapshotError> {
    if dir.parent() != Some(library.snapshots_dir().as_path()) {
        return Err(SnapshotError::OutsideLibrary {
            path: dir.to_path_buf(),
        });
    }
    guard(library, dir)
}

/// Size of a regular file, or `None` when nothing is there.
///
/// A symlink where a blob should be is an error rather than a miss: following it would move
/// or delete something outside the library.
fn file_len(path: &Path) -> Result<Option<u64>, SnapshotError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err(SnapshotError::Io {
            action: "checking a blob",
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SnapshotError::Io {
            action: "checking a blob",
            path: path.to_path_buf(),
            source,
        }),
    }
}

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

/// Lists snapshots in a library, newest first.
///
/// Directories that are half-written or unreadable are skipped rather than failing the whole
/// listing, so one bad snapshot never hides the rest.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] only if the snapshots directory itself cannot be read.
pub fn list(library: &Library) -> Result<Vec<Snapshot>, SnapshotError> {
    read_snapshots(library, false)
}

/// Deletion must not overlook an unreadable manifest that could describe a dependency.
fn read_snapshots(library: &Library, strict: bool) -> Result<Vec<Snapshot>, SnapshotError> {
    let root = library.snapshots_dir();
    guard(library, &root)?;
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&root).map_err(|source| SnapshotError::Io {
        action: "listing snapshots",
        path: root.clone(),
        source,
    })?;

    let mut snapshots = Vec::new();
    for entry in entries {
        let read = (|| {
            let entry = entry.map_err(|source| SnapshotError::Io {
                action: "listing snapshots",
                path: root.clone(),
                source,
            })?;
            if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                return Ok(None);
            }
            let kind = entry.file_type().map_err(|source| SnapshotError::Io {
                action: "checking a snapshot directory",
                path: entry.path(),
                source,
            })?;
            if !kind.is_dir() {
                return Ok(None);
            }
            let dir = entry.path();
            guard(library, &dir.join(MANIFEST_FILE))?;
            Ok(Some(Snapshot {
                manifest: read_manifest(&dir)?,
                dir,
            }))
        })();
        match read {
            Ok(Some(snapshot)) => snapshots.push(snapshot),
            Err(error) if strict => return Err(error),
            Ok(None) | Err(_) => {}
        }
    }

    // Newest first. Snapshots have to be restored in reverse order, because each one was
    // taken against the library as the one before it left it, so the most recent is the only
    // one that can be restored on its own.
    snapshots.sort_by_key(|s| std::cmp::Reverse(s.manifest.created));
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

pub(crate) fn reload(library: &Library, snapshot: &Snapshot) -> Result<Snapshot, SnapshotError> {
    validate_snapshot_dir(library, &snapshot.dir)?;
    guard(library, &snapshot.dir.join(MANIFEST_FILE))?;
    Ok(Snapshot {
        dir: snapshot.dir.clone(),
        manifest: read_manifest(&snapshot.dir)?,
    })
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
    let dir = library.snapshots_dir().join(format!(
        "{TEMP_PREFIX}{}",
        jiff::Timestamp::now().as_nanosecond()
    ));

    guard(library, &dir.join(BLOBS_DIR))?;
    guard(library, &library.files_dir())?;

    std::fs::create_dir_all(dir.join(BLOBS_DIR)).map_err(|source| SnapshotError::Io {
        action: "creating a snapshot directory",
        path: dir.clone(),
        source,
    })?;

    if let Err(error) = ensure_same_volume(library, &dir) {
        abandon(library, &dir);
        return Err(error);
    }
    Ok(dir)
}

/// Removes a snapshot directory that a failed clean left behind.
///
/// A clean that gives up before it commits leaves hard links to blobs the library still owns.
/// Those are invisible to [`list`], so nothing would ever offer to remove them.
pub(crate) fn abandon(library: &Library, dir: &Path) {
    if validate_directory(library, dir).is_err() {
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %dir.display(), %error, "could not remove an abandoned snapshot");
    }
}

/// Copies a blob into a snapshot before its database references are detached.
///
/// Returns the blob's size, or `None` when it is already gone from the library. A hard link
/// costs nothing and keeps the bytes alive even if osu!lazer sweeps the library entry a moment
/// later. Where the filesystem has no hard links, the contents are copied and flushed to disk
/// before this returns, so the caller may commit as soon as it does.
///
/// `snapshot_dir` must already have passed [`validate_snapshot_dir`].
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if the hash is malformed or the blob is not inside
/// the library's `files/` tree, or [`SnapshotError::Io`] if the copy fails.
pub(crate) fn preserve_blob(
    library: &Library,
    snapshot_dir: &Path,
    hash: &str,
) -> Result<Option<u64>, SnapshotError> {
    let source = blob_source(library, hash)?;
    let Some(bytes) = file_len(&source)? else {
        return Ok(None);
    };

    let destination = snapshot_dir.join(BLOBS_DIR).join(hash);
    guard(library, &destination)?;
    match std::fs::hard_link(&source, &destination) {
        Ok(()) => {
            durability::sync_file(&destination).map_err(|source| SnapshotError::Io {
                action: "flushing a snapshot link",
                path: destination,
                source,
            })?;
            Ok(Some(bytes))
        }
        // The blob went away between the check above and the link.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => {
            // Every other failure is treated as "this filesystem has no hard links". Copying
            // refuses an existing destination, so a substituted symlink cannot be written
            // through.
            copy_blob(&source, &destination).map_err(|source| SnapshotError::Io {
                action: "copying a blob into the snapshot",
                path: destination,
                source,
            })?;
            Ok(Some(bytes))
        }
    }
}

/// Copies one blob, flushing it to disk before returning.
fn copy_blob(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut input = std::fs::File::open(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing parent"))?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut input, &mut output)?;
    output.as_file().sync_all()?;
    // A failed copy cannot leave a partial blob at the name a retry will inspect.
    let file = output
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    file.sync_all()
}

/// Removes the library's link to a blob the snapshot already holds.
///
/// Returns whether a link was removed. Nothing is destroyed here: the snapshot holds the same
/// bytes, and deleting the snapshot is what finally reclaims the space.
///
/// `snapshot_dir` must already have passed [`validate_snapshot_dir`].
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if the hash is malformed, the blob is not inside
/// the library's `files/` tree, or the snapshot does not hold `bytes` of this blob.
pub(crate) fn release_blob(
    library: &Library,
    snapshot_dir: &Path,
    hash: &str,
    bytes: u64,
) -> Result<bool, SnapshotError> {
    let source = blob_source(library, hash)?;

    // Never unlink until the snapshot demonstrably holds the same file. This is the one check
    // standing between a clean and a file the user cannot get back.
    let preserved = snapshot_dir.join(BLOBS_DIR).join(hash);
    guard(library, &preserved)?;
    if file_len(&preserved)? != Some(bytes) {
        return Err(SnapshotError::NotPreserved {
            path: preserved,
            bytes,
        });
    }

    match std::fs::remove_file(&source) {
        Ok(()) => Ok(true),
        // Already gone: lazer's own cleanup, or a previous run, got there first.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(SnapshotError::Io {
            action: "removing a file from the library",
            path: source,
            source: error,
        }),
    }
}

/// Resolves a blob's path and re-checks that it is inside the library.
///
/// The check is repeated immediately before every touch, independently of the one made while
/// scanning, because a plan can be minutes old by the time it runs.
fn blob_source(library: &Library, hash: &str) -> Result<PathBuf, SnapshotError> {
    validate_hash(hash)?;
    let source = library.blob_path(hash);

    // Rooted at the library so that `files/` itself is checked for a symlink, and confined to
    // `files/` so that no other part of the library can be reached.
    if !source.starts_with(library.files_dir()) || !is_safe_path(&source, &[library.root()]) {
        return Err(SnapshotError::OutsideLibrary { path: source });
    }
    Ok(source)
}

/// Checks that a directory is a snapshot of this library, once per clean.
///
/// Blob operations take this as given, so that a clean moving hundreds of thousands of files
/// does not walk the same directory chain hundreds of thousands of times.
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if the directory is not directly inside the
/// library's snapshots directory.
pub(crate) fn validate_snapshot_dir(library: &Library, dir: &Path) -> Result<(), SnapshotError> {
    validate_directory(library, dir)?;
    guard(library, &dir.join(BLOBS_DIR))
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
    let mut file =
        std::fs::File::create_new(&manifest_path).map_err(|source| SnapshotError::Io {
            action: "writing the snapshot manifest",
            path: manifest_path.clone(),
            source,
        })?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| SnapshotError::Io {
            action: "saving the snapshot manifest",
            path: manifest_path,
            source,
        })?;
    drop(file);

    // Persist the hard links and manifest entry before publishing their directory name.
    durability::sync_directories(&temp_dir.join(BLOBS_DIR), temp_dir).map_err(|source| {
        SnapshotError::Io {
            action: "flushing the snapshot directory",
            path: temp_dir.to_path_buf(),
            source,
        }
    })?;

    let final_dir = unique_dir(temp_dir, manifest.created);
    durability::rename(temp_dir, &final_dir).map_err(|source| SnapshotError::Io {
        action: "finalising the snapshot",
        path: temp_dir.to_path_buf(),
        source,
    })?;

    Ok(final_dir)
}

/// Makes the published name durable before the database commit. The caller already records
/// the renamed path, so a flush failure can still remove its uncommitted snapshot.
pub(crate) fn sync_published(library: &Library, dir: &Path) -> Result<(), SnapshotError> {
    validate_snapshot_dir(library, dir)?;
    durability::sync_directories(dir, library.root()).map_err(|source| SnapshotError::Io {
        action: "publishing recovery data",
        path: dir.to_path_buf(),
        source,
    })
}

/// Links a snapshot's blobs back into the library, in parallel, retaining the snapshot copies.
///
/// Returns how many blobs were restored. Restoring the database rows is the caller's job,
/// because only it holds a writable realm. The caller must hold the Realm write transaction
/// through this operation and the subsequent commit, so the game cannot sweep restored files.
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
    validate_snapshot_dir(library, &snapshot.dir)?;
    for (hash, _) in &snapshot.manifest.blobs {
        validate_hash(hash)?;
    }
    for candidate in &snapshot.manifest.detached {
        validate_hash(&candidate.hash)?;
    }
    // Older snapshots can refer to shared blobs that they never stored. Check those too,
    // before committing any restored references to files that may have disappeared since.
    let mut expected: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for (hash, size) in &snapshot.manifest.blobs {
        if expected
            .insert(hash, *size)
            .is_some_and(|previous| previous != *size)
        {
            return Err(SnapshotError::BlobMismatch {
                path: snapshot.dir.join(BLOBS_DIR).join(hash),
            });
        }
    }
    for candidate in &snapshot.manifest.detached {
        expected.entry(&candidate.hash).or_insert(candidate.bytes);
    }
    let blobs: Vec<_> = expected.into_iter().collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let failure: std::sync::Mutex<Option<SnapshotError>> = std::sync::Mutex::new(None);

    std::thread::scope(|scope| {
        let workers = std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .min(blobs.len());
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            handles.push(scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((hash, bytes)) = blobs.get(index) else {
                        break;
                    };

                    if let Err(error) = restore_one(library, &snapshot.dir, hash, *bytes) {
                        let mut slot = failure.lock().expect("restore mutex was poisoned");
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        break;
                    }

                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }));
        }

        // Report from this thread, so the caller's closure never has to be `Sync`.
        while handles.iter().any(|handle| !handle.is_finished()) {
            progress(done.load(std::sync::atomic::Ordering::Relaxed), blobs.len());
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });

    if let Some(error) = failure.into_inner().expect("restore mutex was poisoned") {
        return Err(error);
    }

    let directories: std::collections::HashSet<_> = blobs
        .iter()
        .filter_map(|(hash, _)| library.blob_path(hash).parent().map(Path::to_path_buf))
        .collect();
    for directory in directories {
        guard(library, &directory)?;
        durability::sync_directories(&directory, library.root()).map_err(|source| {
            SnapshotError::Io {
                action: "flushing restored file directories",
                path: directory,
                source,
            }
        })?;
    }

    Ok(done.load(std::sync::atomic::Ordering::Relaxed))
}

/// Installs one blob without giving up the snapshot's recovery link.
///
/// `snapshot_dir` must already have passed [`validate_snapshot_dir`].
fn restore_one(
    library: &Library,
    snapshot_dir: &Path,
    hash: &str,
    bytes: u64,
) -> Result<(), SnapshotError> {
    let destination = blob_source(library, hash)?;
    let source = snapshot_dir.join(BLOBS_DIR).join(hash);
    guard(library, &source)?;

    let source_size = file_len(&source)?;
    let destination_size = file_len(&destination)?;
    if source_size.is_none() {
        if destination_size == Some(bytes) {
            // Version 1.0 moved files before its database transaction. When retrying such a
            // restore, the content hash is the only remaining way to verify those bytes.
            if matches_hash(&destination, hash).map_err(|source| SnapshotError::Io {
                action: "verifying a previously restored file",
                path: destination.clone(),
                source,
            })? {
                return durability::sync_file(&destination).map_err(|source| SnapshotError::Io {
                    action: "flushing a previously restored file",
                    path: destination,
                    source,
                });
            }
            return Err(SnapshotError::BlobMismatch { path: destination });
        }
        return Err(SnapshotError::Io {
            action: "restoring a missing blob",
            path: source,
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
    }

    if source_size != Some(bytes) {
        return Err(SnapshotError::BlobMismatch { path: source });
    }

    if let Some(size) = destination_size {
        if size != bytes
            || !files_match(&source, &destination).map_err(|source| SnapshotError::Io {
                action: "comparing a restored file",
                path: destination.clone(),
                source,
            })?
        {
            return Err(SnapshotError::BlobMismatch { path: destination });
        }
        return durability::sync_file(&destination).map_err(|source| SnapshotError::Io {
            action: "flushing a restored file",
            path: destination,
            source,
        });
    }

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|source_error| SnapshotError::Io {
            action: "recreating a blob directory",
            path: parent.to_path_buf(),
            source: source_error,
        })?;
    }

    if std::fs::hard_link(&source, &destination).is_err() {
        copy_blob(&source, &destination).map_err(|source| SnapshotError::Io {
            action: "copying a file back from the snapshot",
            path: destination.clone(),
            source,
        })?;
    }
    durability::sync_file(&destination).map_err(|source| SnapshotError::Io {
        action: "flushing a restored link",
        path: destination,
        source,
    })
}

/// Exact comparison for a pre-existing destination, without hashing normal restores.
fn files_match(left: &Path, right: &Path) -> std::io::Result<bool> {
    use std::io::BufRead as _;
    let left = std::fs::File::open(left)?;
    let right = std::fs::File::open(right)?;
    if same_file(&left, &right)? {
        return Ok(true);
    }
    let mut left = std::io::BufReader::new(left);
    let mut right = std::io::BufReader::new(right);
    loop {
        let a = left.fill_buf()?;
        let b = right.fill_buf()?;
        if a.is_empty() || b.is_empty() {
            return Ok(a.is_empty() && b.is_empty());
        }
        let count = a.len().min(b.len());
        if a[..count] != b[..count] {
            return Ok(false);
        }
        left.consume(count);
        right.consume(count);
    }
}

fn same_file(left: &std::fs::File, right: &std::fs::File) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let a = left.metadata()?;
        let b = right.metadata()?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let identity = |file: &std::fs::File| -> std::io::Result<_> {
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: the handle belongs to a live file and the output buffer is ours.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
            ))
        };
        Ok(identity(left)? == identity(right)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Ok(false)
    }
}

fn matches_hash(path: &Path, hash: &str) -> std::io::Result<bool> {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let digest = digest.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing to a String cannot fail, so the result carries no information.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex.eq_ignore_ascii_case(hash))
}

/// Deletes a snapshot, reclaiming its space.
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if the snapshot is not directly inside the
/// library's snapshots directory, or [`SnapshotError::Io`] if removal fails.
pub fn delete(library: &Library, snapshot: &Snapshot) -> Result<(), SnapshotError> {
    let _lock = OperationLock::acquire(library)?;
    validate_snapshot_dir(library, &snapshot.dir)?;
    // Read the current manifest under the operation lock, not a cached interface summary.
    let current = reload(library, snapshot)?;
    for other in read_snapshots(library, true)? {
        if other.dir == current.dir {
            continue;
        }
        let needed: std::collections::HashSet<_> = other
            .manifest
            .detached
            .iter()
            .map(|c| c.hash.as_str())
            .collect();
        for (hash, size) in &current.manifest.blobs {
            if !needed.contains(hash.as_str()) {
                continue;
            }
            validate_hash(hash)?;
            let own_copy = other.dir.join(BLOBS_DIR).join(hash);
            guard(library, &own_copy)?;
            if file_len(&own_copy)? != Some(*size) {
                return Err(SnapshotError::SnapshotDependency {
                    snapshot: current.id(),
                    dependent: other.id(),
                });
            }
        }
    }
    remove_restored(library, &current)
}

/// Removes recovery links after restoration commits. The caller holds the operation lock.
pub(crate) fn remove_restored(library: &Library, snapshot: &Snapshot) -> Result<(), SnapshotError> {
    validate_directory(library, &snapshot.dir)?;
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

    let target = library.files_dir().join(format!(
        ".volume-probe-{}",
        jiff::Timestamp::now().as_nanosecond()
    ));
    guard(library, &target)?;
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

    /// Preserving then releasing is the whole clean, minus the database.
    fn clean_one(library: &Library, hash: &str, bytes: u64) -> PathBuf {
        let temp = begin(library).unwrap();
        assert_eq!(preserve_blob(library, &temp, hash).unwrap(), Some(bytes));
        let dir = finalise(&temp, &manifest(vec![(hash.to_owned(), bytes)])).unwrap();
        release_blob(library, &dir, hash, bytes).unwrap();
        dir
    }

    #[test]
    fn a_clean_takes_the_blob_out_of_the_library_and_keeps_the_bytes() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let snapshot_dir = clean_one(&library, &hash, 7);

        assert!(
            !library.blob_path(&hash).exists(),
            "the library must give up its link"
        );
        assert_eq!(
            std::fs::read(snapshot_dir.join(BLOBS_DIR).join(&hash)).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn preserving_keeps_the_library_copy_in_place() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let temp = begin(&library).unwrap();
        assert_eq!(preserve_blob(&library, &temp, &hash).unwrap(), Some(7));

        assert!(
            library.blob_path(&hash).is_file(),
            "nothing may leave the library before the database commits"
        );
        assert_eq!(
            std::fs::read(temp.join(BLOBS_DIR).join(&hash)).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn preserving_a_missing_blob_is_not_an_error() {
        let hash = "b".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();

        std::fs::remove_file(library.blob_path(&hash)).unwrap();
        assert_eq!(preserve_blob(&library, &temp, &hash).unwrap(), None);
    }

    #[test]
    fn releasing_a_blob_the_game_already_swept_is_not_an_error() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let temp = begin(&library).unwrap();
        preserve_blob(&library, &temp, &hash).unwrap();
        let dir = finalise(&temp, &manifest(vec![(hash.clone(), 7)])).unwrap();

        // This is what `RealmFileStore.Cleanup` does once our transaction commits.
        std::fs::remove_file(library.blob_path(&hash)).unwrap();
        assert!(!release_blob(&library, &dir, &hash, 7).unwrap());

        let snapshot = list(&library).unwrap().remove(0);
        restore_blobs(&library, &snapshot, &mut |_, _| {}).unwrap();
        delete(&library, &snapshot).unwrap();
        assert_eq!(std::fs::read(library.blob_path(&hash)).unwrap(), b"payload");
    }

    #[test]
    fn a_blob_the_snapshot_does_not_hold_is_never_released() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();

        // Nothing was preserved, so nothing may be removed.
        assert!(matches!(
            release_blob(&library, &temp, &hash, 7),
            Err(SnapshotError::NotPreserved { .. })
        ));

        // A truncated copy does not count either.
        std::fs::write(temp.join(BLOBS_DIR).join(&hash), b"pay").unwrap();
        assert!(matches!(
            release_blob(&library, &temp, &hash, 7),
            Err(SnapshotError::NotPreserved { .. })
        ));

        assert_eq!(std::fs::read(library.blob_path(&hash)).unwrap(), b"payload");
    }

    #[test]
    fn a_clean_survives_the_game_deleting_the_library_copy() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        let snapshot_dir = clean_one(&library, &hash, 7);
        assert!(snapshot_dir.join(BLOBS_DIR).join(&hash).is_file());

        let snapshot = list(&library).unwrap().remove(0);
        restore_blobs(&library, &snapshot, &mut |_, _| {}).unwrap();
        delete(&library, &snapshot).unwrap();
        assert_eq!(std::fs::read(library.blob_path(&hash)).unwrap(), b"payload");
    }

    #[test]
    fn preserving_refuses_to_overwrite_an_existing_snapshot_entry() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();

        let destination = temp.join(BLOBS_DIR).join(&hash);
        std::fs::write(&destination, b"older").unwrap();

        assert!(preserve_blob(&library, &temp, &hash).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"older");
        assert_eq!(std::fs::read(library.blob_path(&hash)).unwrap(), b"payload");
    }

    #[test]
    fn copying_is_used_where_hard_links_are_not_available() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();

        let destination = temp.join(BLOBS_DIR).join(&hash);
        copy_blob(&library.blob_path(&hash), &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"payload");
        assert!(release_blob(&library, &temp, &hash, 7).unwrap());
        assert!(!library.blob_path(&hash).exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn preserving_rejects_a_substituted_blob_store() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();

        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        let shard = library.blob_path(&hash);
        let shard = shard.parent().unwrap();
        std::fs::remove_dir_all(shard).unwrap();
        std::os::unix::fs::symlink(outside.path(), shard).unwrap();

        assert!(matches!(
            preserve_blob(&library, &temp, &hash),
            Err(SnapshotError::OutsideLibrary { .. })
        ));
        assert!(matches!(
            release_blob(&library, &temp, &hash, 6),
            Err(SnapshotError::OutsideLibrary { .. })
        ));
        assert!(outside.path().join("secret").is_file());
    }

    #[test]
    fn a_snapshot_round_trips() {
        let hash = "c".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        clean_one(&library, &hash, 7);

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

        clean_one(&library, &hash, 7);

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
        assert!(matches!(
            restore_blobs(&library, &snapshot, &mut |_, _| {}),
            Err(SnapshotError::OutsideLibrary { .. })
        ));
    }

    #[test]
    fn malformed_hashes_are_refused_before_any_move() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();
        for invalid in ["../escape", "/absolute", "é", "a\\..\\escape"] {
            assert!(preserve_blob(&library, &temp, invalid).is_err());
            assert!(release_blob(&library, &temp, invalid, 1).is_err());
            let snapshot = Snapshot {
                dir: temp.clone(),
                manifest: manifest(vec![(invalid.to_owned(), 1)]),
            };
            assert!(restore_blobs(&library, &snapshot, &mut |_, _| {}).is_err());
        }
        assert!(library.blob_path(&hash).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn restore_rechecks_the_blob_store_ancestor() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = clean_one(&library, &hash, 7);
        let outside = tempfile::tempdir().unwrap();
        std::fs::remove_dir_all(library.files_dir()).unwrap();
        std::os::unix::fs::symlink(outside.path(), library.files_dir()).unwrap();
        assert!(restore_one(&library, &temp, &hash, 7).is_err());
        assert!(temp.join(BLOBS_DIR).join(hash).is_file());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn failed_workers_finish_with_unclaimed_blobs() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let temp = begin(&library).unwrap();
        std::fs::remove_file(library.blob_path(&hash)).unwrap();
        let count = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2 + 1;
        let snapshot = Snapshot {
            dir: temp,
            manifest: manifest(vec![(hash, 7); count]),
        };
        let started = std::time::Instant::now();
        let error = restore_blobs(&library, &snapshot, &mut |_, _| {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "restore workers did not finish"
            );
        });
        assert!(error.is_err());
    }

    #[test]
    fn retrying_a_moved_blob_preserves_the_library_copy() {
        let hash = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
        let (_dir, library) = library_with_blob(hash, b"payload");
        let temp = begin(&library).unwrap();
        assert!(restore_one(&library, &temp, hash, 7).is_ok());
        assert_eq!(std::fs::read(library.blob_path(hash)).unwrap(), b"payload");
    }

    #[test]
    fn unfinished_snapshots_are_not_listed() {
        let hash = "f".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");

        begin(&library).unwrap();
        assert!(list(&library).unwrap().is_empty());
    }

    #[test]
    fn restoring_a_blob_retains_the_same_file_in_the_snapshot() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let dir = clean_one(&library, &hash, 7);
        restore_one(&library, &dir, &hash, 7).unwrap();
        let recovery = std::fs::File::open(dir.join(BLOBS_DIR).join(&hash)).unwrap();
        let restored = std::fs::File::open(library.blob_path(&hash)).unwrap();
        assert!(
            same_file(&recovery, &restored).unwrap(),
            "hard-link restores need no second copy"
        );
    }

    #[test]
    fn a_truncated_snapshot_blob_is_not_restored() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let dir = clean_one(&library, &hash, 7);
        std::fs::write(dir.join(BLOBS_DIR).join(&hash), b"pay").unwrap();
        assert!(matches!(
            restore_one(&library, &dir, &hash, 7),
            Err(SnapshotError::BlobMismatch { .. })
        ));
        assert!(!library.blob_path(&hash).exists());
    }

    #[test]
    fn a_same_size_corrupt_legacy_restore_is_refused() {
        let hash = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
        let (_dir, library) = library_with_blob(hash, b"PAYLOAD");
        let dir = begin(&library).unwrap();
        assert!(matches!(
            restore_one(&library, &dir, hash, 7),
            Err(SnapshotError::BlobMismatch { .. })
        ));
    }

    #[test]
    fn deletion_refuses_to_ignore_an_unreadable_dependency_manifest() {
        let hash = "a".repeat(64);
        let (_dir, library) = library_with_blob(&hash, b"payload");
        let dir = clean_one(&library, &hash, 7);
        let snapshot = list(&library).unwrap().remove(0);
        let unreadable = library.snapshots_dir().join("older");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::write(unreadable.join(MANIFEST_FILE), "broken JSON").unwrap();
        assert!(delete(&library, &snapshot).is_err());
        assert!(dir.join(BLOBS_DIR).join(hash).is_file());
    }
}
