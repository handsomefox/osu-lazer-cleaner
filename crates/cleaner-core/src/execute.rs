//! Running a clean, and undoing one.
//!
//! The order is what makes a clean recoverable. Under the database write lock, the plan is
//! re-checked, every blob it would orphan is linked into the snapshot, and the manifest is
//! written. Only then does the transaction commit, and only then does the library give up its
//! own links. Stop the process at any point and either nothing happened or the snapshot holds
//! the bytes, so a clean the user interrupts never costs a file.
//!
//! A clean that fails before it commits takes its half-built snapshot with it, because those
//! links would otherwise keep blobs alive that the library still owns.

use crate::durability;
use crate::error::SnapshotError;
use crate::operation::OperationLock;
use crate::plan::{Options, Plan};
use crate::snapshot::{self, Manifest, Snapshot};
use crate::storage::Library;
use cleaner_realm::{Realm, Removal, Restoration};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Progress reported while a clean runs.
#[derive(Debug, Clone, Copy)]
pub enum CleanProgress {
    /// Updating the database.
    UpdatingDatabase,
    /// Putting files into the snapshot, before the database changes.
    Preserving {
        /// Files saved so far.
        done: usize,
        /// Files to save in total.
        total: usize,
    },
    /// Removing files from the library, after the database changed.
    Removing {
        /// Files removed so far.
        done: usize,
        /// Files to remove in total.
        total: usize,
    },
    /// Reattaching files to their beatmap sets.
    ///
    /// Counted in beatmap sets rather than files, because the whole thing commits at once and
    /// a per-file count would suggest work is being saved as it goes.
    Reattaching {
        /// Beatmap sets done so far.
        done: usize,
        /// Beatmap sets in total.
        total: usize,
    },
    /// Moving files back out of a snapshot.
    Restoring {
        /// Files moved so far.
        done: usize,
        /// Files to move in total.
        total: usize,
    },
}

/// What a clean did.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// Usages detached from the database.
    pub detached: usize,
    /// Blobs moved into the snapshot.
    pub stashed: usize,
    /// Bytes the snapshot now holds, which is what deleting it will reclaim.
    pub bytes: u64,
    /// Where the snapshot went, unless this was a preview.
    pub snapshot: Option<PathBuf>,
    /// Whether this was a preview.
    pub dry_run: bool,
}

/// Removes the selected files, moving their bytes into a snapshot.
///
/// Previewing changes nothing and still returns a filled-in [`Outcome`], so a caller that
/// forgets to branch on [`Options::dry_run`] cannot destroy anything.
///
/// # Errors
///
/// Returns [`SnapshotError`] if the database cannot be updated or a blob cannot be moved. The
/// database transaction is all-or-nothing, so a failure there leaves the library untouched.
pub fn run(
    library: &Library,
    plan: &Plan,
    options: &Options,
    mut progress: impl FnMut(CleanProgress),
) -> Result<Outcome, SnapshotError> {
    let candidates: Vec<_> = plan.selected_candidates().cloned().collect();

    if candidates.is_empty() || options.dry_run {
        return Ok(Outcome {
            detached: candidates.len(),
            stashed: 0,
            bytes: plan.selected_bytes(),
            snapshot: None,
            dry_run: options.dry_run,
        });
    }

    check_database_path(library)?;
    let _lock = OperationLock::acquire(library)?;
    let started = std::time::Instant::now();
    tracing::info!(
        candidates = candidates.len(),
        bytes = plan.selected_bytes(),
        library = %library.root().display(),
        "clean started"
    );
    let snapshot_dir = snapshot::begin(library)?;

    // `finalise` renames the directory before the commit, so a failure has to be able to find
    // it under either name.
    let finalised = std::cell::Cell::new(None);
    let prepared = prepare(
        library,
        &candidates,
        &snapshot_dir,
        &finalised,
        &mut progress,
    );

    let (removals, final_dir, blobs) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            // Nothing committed, so the links in there hold blobs the library still owns, and
            // the manifest describes detachments that never happened.
            snapshot::abandon(library, &snapshot_dir);
            if let Some(dir) = finalised.take() {
                snapshot::abandon(library, &dir);
            }
            tracing::error!(
                ms = elapsed_ms(started),
                "clean failed, snapshot abandoned: {}",
                crate::error::chain(&error)
            );
            return Err(error);
        }
    };

    release_unreferenced(library, &final_dir, &blobs, &mut progress)?;

    let bytes: u64 = blobs.iter().map(|(_, size)| size).sum();
    let ms = elapsed_ms(started);
    tracing::info!(
        snapshot = %final_dir.display(),
        rows = removals,
        blobs = blobs.len(),
        bytes,
        ms,
        blobs_per_s = crate::format::per_second(blobs.len() as u64, ms),
        mb_per_s = crate::format::megabytes_per_second(bytes, ms),
        "clean finished"
    );

    Ok(Outcome {
        detached: removals,
        stashed: blobs.len(),
        bytes,
        snapshot: Some(final_dir),
        dry_run: false,
    })
}

/// Milliseconds since `started`, saturating rather than panicking on a huge value.
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// What a committed clean produced: usages detached, where the snapshot went, and what it holds.
type Prepared = (usize, PathBuf, Vec<(String, u64)>);

/// Re-checks the plan, fills the snapshot, and commits the database, in that order.
///
/// Returns how many usages were detached, where the snapshot ended up, and what it holds.
/// Every step runs under the database write lock, so nothing else can import or delete a
/// beatmap while the snapshot is being built.
fn prepare(
    library: &Library,
    candidates: &[crate::Candidate],
    snapshot_dir: &Path,
    finalised: &std::cell::Cell<Option<PathBuf>>,
    progress: &mut impl FnMut(CleanProgress),
) -> Result<Prepared, SnapshotError> {
    snapshot::validate_snapshot_dir(library, snapshot_dir)?;

    // Orphaned blobs have no usage to detach; they are already unreferenced.
    let removals: Vec<Removal> = candidates
        .iter()
        .filter(|c| c.set_index != crate::plan::NO_SET_INDEX)
        .map(|c| Removal {
            set_index: c.set_index,
            file_index: c.file_index,
        })
        .collect();

    progress(CleanProgress::UpdatingDatabase);
    let realm = Realm::open_for_write(&library.database())?;
    let (final_dir, blobs) = realm.erase_usages_with(|realm| {
        let current = realm.read_library()?;
        let freed = validate_candidates(candidates, &current)?;

        let blobs = each_blob(
            &freed,
            |hash, _| snapshot::preserve_blob(library, snapshot_dir, hash),
            &mut |done, total| progress(CleanProgress::Preserving { done, total }),
        )?;

        let manifest = Manifest {
            format_version: snapshot::FORMAT_VERSION,
            created: jiff::Timestamp::now(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            schema_version: realm.schema_version(),
            detached: candidates.to_vec(),
            blobs,
        };

        // Publish recovery data before committing. An interrupted clean stays restorable,
        // including blobs that never left the library.
        let dir = snapshot::finalise(snapshot_dir, &manifest)?;
        finalised.set(Some(dir.clone()));
        snapshot::sync_published(library, &dir)?;
        progress(CleanProgress::UpdatingDatabase);
        Ok::<_, SnapshotError>((removals.clone(), (dir, manifest.blobs)))
    })?;

    Ok((removals.len(), final_dir, blobs))
}

/// Rejects shifted indices and recomputes which blobs lose their last owner.
fn validate_candidates<'a>(
    candidates: &'a [crate::Candidate],
    current: &cleaner_realm::Library,
) -> Result<Vec<(&'a str, u64)>, SnapshotError> {
    let sets: HashMap<_, _> = current
        .beatmap_sets
        .iter()
        .map(|set| (set.index, set))
        .collect();
    let mut remaining = current.usage_counts.clone();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        if candidate.hash.len() != 64 || !candidate.hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SnapshotError::StalePlan);
        }
        if candidate.set_index == crate::plan::NO_SET_INDEX {
            if remaining.get(&candidate.hash).copied().unwrap_or(0) != 0 {
                return Err(SnapshotError::StalePlan);
            }
            continue;
        }
        let Some(set) = sets.get(&candidate.set_index) else {
            return Err(SnapshotError::StalePlan);
        };
        let Some(file) = set.files.get(candidate.file_index) else {
            return Err(SnapshotError::StalePlan);
        };
        if set.id != candidate.set_id
            || file.hash != candidate.hash
            || file.filename != candidate.filename
            || file.filename.to_ascii_lowercase().ends_with(".osu")
            || set.audio.iter().any(|audio| {
                crate::osu::filename_key(audio) == crate::osu::filename_key(&file.filename)
            })
            || !seen.insert((candidate.set_index, candidate.file_index))
        {
            return Err(SnapshotError::StalePlan);
        }
        let count = remaining
            .get_mut(&candidate.hash)
            .ok_or(SnapshotError::StalePlan)?;
        *count = count.checked_sub(1).ok_or(SnapshotError::StalePlan)?;
    }
    // A blob leaves only when every usage anywhere is being detached, which is what
    // `RealmFileStore.Cleanup` expresses as `Usages.@count = 0`. Counting each reference on
    // its own would keep a file two selected beatmap sets share.
    let mut freed: Vec<(&str, u64)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        if remaining.get(&candidate.hash).copied().unwrap_or(0) == 0
            && seen.insert(candidate.hash.as_str())
        {
            freed.push((candidate.hash.as_str(), candidate.bytes));
        }
    }
    Ok(freed)
}

/// What the copies of `client.realm` taken before a compaction are called, either side of the
/// time the copy was taken.
///
/// The time is in the name so that a run of compactions leaves a history instead of one file
/// each compaction overwrites.
const BACKUP_PREFIX: &str = "client.realm.";
/// The other half of a copy's name. Also keeps a half-written `.partial` from being mistaken
/// for a finished copy.
const BACKUP_SUFFIX: &str = ".backup";

/// The one name version 1.3 and earlier gave the single copy it kept. Still recognised, so that
/// a copy left by an older version can be listed and deleted.
const LEGACY_BACKUP: &str = "client.realm.backup";

/// How many copies to keep.
///
/// Each one is as large as the database it came from, so keeping every copy would cost a
/// gigabyte after a handful of compactions. The newest are the ones worth having: a database
/// osu!lazer refuses to open shows up the next time it starts, not several compactions later.
const MAX_DATABASE_BACKUPS: usize = 3;

/// How many names to try before giving up and replacing one. A minute of them is far more than
/// a compaction, which takes a second on a large database, can ever need.
const MAX_NAME_ATTEMPTS: u32 = 60;

/// What compacting the database did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// Whether realm-core actually rewrote the file.
    ///
    /// It declines rather than fails when the database is still in use, so this is the only way
    /// to tell a successful compaction from one that never happened.
    pub rewritten: bool,
    /// Size of the database before.
    pub before: u64,
    /// Size of the database after.
    pub after: u64,
    /// Where the copy taken beforehand is kept.
    pub backup: PathBuf,
}

impl Compaction {
    /// How many bytes the rewrite gave back.
    #[must_use]
    pub const fn freed(&self) -> u64 {
        self.before.saturating_sub(self.after)
    }
}

/// Rewrites the database without its free space, reporting the sizes before and after.
///
/// Realm never shrinks on its own: deleting rows returns their space to an internal free list
/// for reuse, so the file stays the same size however much a clean removes. This is the only
/// operation that makes it smaller, and the only one that rewrites the file the whole library
/// depends on. A copy is taken first and kept afterwards, so a rewrite that produces a database
/// osu!lazer will not open costs nothing but the time to put the copy back.
///
/// # Errors
///
/// Returns [`SnapshotError`] if the database cannot be copied, opened, or measured. Nothing is
/// rewritten unless the copy is complete and on disk.
pub fn compact(library: &Library) -> Result<Compaction, SnapshotError> {
    check_database_path(library)?;
    let _lock = OperationLock::acquire(library)?;
    let path = library.database();
    let measure = |when| {
        std::fs::metadata(&path)
            .map_err(|source| SnapshotError::Io {
                action: when,
                path: path.clone(),
                source,
            })
            .map(|meta| meta.len())
    };

    let started = std::time::Instant::now();
    let before = measure("measuring the database")?;
    let backup = back_up_database(library)?;
    let rewritten = Realm::open_for_write(&path)?.compact()?;
    let after = measure("measuring the database")?;
    tracing::info!(
        rewritten,
        before,
        after,
        saved = before.saturating_sub(after),
        ms = elapsed_ms(started),
        "compaction finished"
    );

    Ok(Compaction {
        rewritten,
        before,
        after,
        backup,
    })
}

/// Copies `client.realm` beside the snapshots, replacing the previous copy.
///
/// The copy lands under a temporary name and is renamed into place, so an interrupted copy
/// never replaces a good one with a truncated file.
fn back_up_database(library: &Library) -> Result<PathBuf, SnapshotError> {
    let directory = library.snapshots_dir();
    if !crate::safety::is_safe_path(&directory, &[library.root()]) {
        return Err(SnapshotError::OutsideLibrary { path: directory });
    }

    std::fs::create_dir_all(&directory).map_err(|source| SnapshotError::Io {
        action: "creating the snapshots directory",
        path: directory.clone(),
        source,
    })?;

    let name = unique_backup_name(&directory);
    let destination = directory.join(&name);
    let partial = directory.join(format!("{name}.partial"));
    let _ = std::fs::remove_file(&partial);

    let written = write_backup(library, &partial, &destination);
    if written.is_err() {
        // A half-written copy is worse than none: it would sit next to the snapshots looking
        // like something to fall back on.
        let _ = std::fs::remove_file(&partial);
        return written.map(|()| destination);
    }

    prune_database_backups(library, &name);
    Ok(destination)
}

/// Picks a name no copy already has, stepping forward a second at a time.
///
/// Two compactions inside one second would otherwise pick the same name, and the second would
/// replace the copy the first kept. Stepping the recorded time keeps every name one that
/// [`stamp_in_name`] can still read.
fn unique_backup_name(directory: &Path) -> String {
    let mut taken = jiff::Timestamp::now();
    for _ in 0..MAX_NAME_ATTEMPTS {
        let name = format!(
            "{BACKUP_PREFIX}{}{BACKUP_SUFFIX}",
            taken.strftime("%Y%m%d-%H%M%S")
        );
        if !directory.join(&name).exists() {
            return name;
        }
        taken += jiff::SignedDuration::from_secs(1);
    }

    // Every name in the minute after now is taken, which means something other than this is
    // writing them. Replacing one is better than failing the compaction over it.
    format!(
        "{BACKUP_PREFIX}{}{BACKUP_SUFFIX}",
        taken.strftime("%Y%m%d-%H%M%S")
    )
}

/// Deletes all but the newest [`MAX_DATABASE_BACKUPS`] copies, never the copy just written.
///
/// `keep` is held back whatever its name says, because a clock that has gone backwards would
/// otherwise make the copy taken a moment ago the oldest one there, and pruning it would leave
/// the rewrite that follows with nothing to fall back on.
///
/// A copy that will not delete is not worth failing a compaction over: the copy that matters is
/// on disk, so this reports and carries on.
fn prune_database_backups(library: &Library, keep: &str) {
    for stale in database_backups(library)
        .into_iter()
        .filter(|copy| copy.name != keep)
        .skip(MAX_DATABASE_BACKUPS.saturating_sub(1))
    {
        match std::fs::remove_file(&stale.path) {
            Ok(()) => tracing::info!(copy = %stale.name, "removed an old database copy"),
            Err(error) => {
                tracing::warn!(copy = %stale.name, %error, "could not remove an old database copy");
            }
        }
    }
}

/// Copies the database to `partial`, flushes it, and moves it to `destination`.
fn write_backup(
    library: &Library,
    partial: &Path,
    destination: &Path,
) -> Result<(), SnapshotError> {
    std::fs::copy(library.database(), partial).map_err(|source| SnapshotError::Io {
        action: "copying the database before compacting it",
        path: partial.to_path_buf(),
        source,
    })?;

    // Flushing matters here: the whole point is a copy that survives whatever the rewrite does.
    //
    // The handle has to be opened for writing. Windows refuses `FlushFileBuffers` on a
    // read-only handle, where Linux is happy to `fsync` one, so a read-only open passed every
    // test here and failed every one on Windows.
    std::fs::OpenOptions::new()
        .write(true)
        .open(partial)
        .and_then(|file| file.sync_all())
        .map_err(|source| SnapshotError::Io {
            action: "flushing the database copy",
            path: partial.to_path_buf(),
            source,
        })?;

    durability::rename(partial, destination).map_err(|source| SnapshotError::Io {
        action: "saving the database copy",
        path: destination.to_path_buf(),
        source,
    })?;
    durability::sync_directories(&library.snapshots_dir(), library.root()).map_err(|source| {
        SnapshotError::Io {
            action: "publishing the database copy",
            path: destination.to_path_buf(),
            source,
        }
    })
}

/// One copy of the database, kept from before a compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseBackup {
    /// File name, which is how the copy is addressed.
    pub name: String,
    /// Where it is, for an interface to show and a user to rename.
    pub path: PathBuf,
    /// How many bytes deleting it would reclaim.
    pub bytes: u64,
    /// When it was written.
    pub taken: jiff::Timestamp,
}

/// Whether a file name is one this wrote, rather than something else sitting beside the
/// snapshots. Only these are ever listed, pruned, or offered for deletion.
///
/// The time has to parse, which is what keeps a name like `client.realm.notes.backup` from
/// being pruned as though this had written it, and a name carrying a path separator from
/// reaching anything nested.
fn is_backup_name(name: &str) -> bool {
    name == LEGACY_BACKUP || stamp_in_name(name).is_some()
}

/// The time a copy's name records, or `None` when the name does not record one.
fn stamp_in_name(name: &str) -> Option<jiff::Timestamp> {
    let stamp = name
        .strip_prefix(BACKUP_PREFIX)?
        .strip_suffix(BACKUP_SUFFIX)?;
    jiff::civil::DateTime::strptime("%Y%m%d-%H%M%S", stamp)
        .ok()?
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|zoned| zoned.timestamp())
}

/// When a copy was taken, which its name records.
///
/// Falls back to when the file was written, for the legacy name, which records nothing.
/// `written` is `None` only when the filesystem will not say, which leaves nothing to date the
/// copy by.
fn taken_from_name(name: &str, written: Option<jiff::Timestamp>) -> Option<jiff::Timestamp> {
    stamp_in_name(name).or(written)
}

/// The copies of the database that compactions left behind, newest first.
///
/// Returns an empty list when no compaction has run, or when every copy has been removed.
#[must_use]
pub fn database_backups(library: &Library) -> Vec<DatabaseBackup> {
    let directory = library.snapshots_dir();
    if !crate::safety::is_safe_path(&directory, &[library.root()]) {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };

    let mut found: Vec<DatabaseBackup> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !is_backup_name(&name) {
                return None;
            }
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            let written = meta.modified().ok().and_then(|at| at.try_into().ok());
            let taken = taken_from_name(&name, written)?;
            Some(DatabaseBackup {
                name,
                path: entry.path(),
                bytes: meta.len(),
                taken,
            })
        })
        .collect();

    found.sort_by(|left, right| {
        right
            .taken
            .cmp(&left.taken)
            .then_with(|| right.name.cmp(&left.name))
    });
    found
}

/// Deletes one copy of the database, named as [`DatabaseBackup::name`] gives it.
///
/// Deleting a copy that is already gone is not an error, so a second click cannot fail.
///
/// # Errors
///
/// Returns [`SnapshotError::OutsideLibrary`] if `name` is not a name this wrote, and
/// [`SnapshotError::Io`] if the file exists and cannot be removed.
pub fn remove_database_backup(library: &Library, name: &str) -> Result<(), SnapshotError> {
    let _lock = OperationLock::acquire(library)?;
    let path = library.snapshots_dir().join(name);
    // `name` arrives from an interface. Anything that is not a copy this wrote, and any name
    // that walks out of the snapshots directory, is refused rather than deleted.
    if !is_backup_name(name) || !crate::safety::is_safe_path(&path, &[library.root()]) {
        return Err(SnapshotError::OutsideLibrary { path });
    }

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SnapshotError::Io {
            action: "deleting the database copy",
            path,
            source,
        }),
    }
}

/// A writer may have acquired a reference after the clean committed. Recheck under a new
/// transaction and keep that lock until every unlink finishes, as `RealmFileStore.Cleanup` does.
fn release_unreferenced(
    library: &Library,
    snapshot_dir: &Path,
    blobs: &[(String, u64)],
    progress: &mut impl FnMut(CleanProgress),
) -> Result<(), SnapshotError> {
    check_database_path(library)?;
    let realm = Realm::open_for_write(&library.database())?;
    realm.with_write_transaction(|realm| {
        let current = realm.read_library()?;
        let unreferenced: Vec<_> = blobs
            .iter()
            .filter(|(hash, _)| current.usage_counts.get(hash).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        release_all(library, snapshot_dir, &unreferenced, progress)
    })
}

/// Removes every preserved blob's library link, in parallel.
///
/// Each unlink is an independent filesystem operation, and on Windows they are slow enough
/// individually that doing them one at a time dominates a large clean.
fn release_all(
    library: &Library,
    snapshot_dir: &Path,
    blobs: &[(String, u64)],
    progress: &mut impl FnMut(CleanProgress),
) -> Result<(), SnapshotError> {
    let work: Vec<(&str, u64)> = blobs
        .iter()
        .map(|(hash, size)| (hash.as_str(), *size))
        .collect();

    each_blob(
        &work,
        |hash, size| snapshot::release_blob(library, snapshot_dir, hash, size).map(|_| Some(size)),
        &mut |done, total| progress(CleanProgress::Removing { done, total }),
    )?;
    Ok(())
}

/// Runs one filesystem operation over every blob, across every core.
///
/// Reports progress from this thread, so the caller's closure never has to be `Sync`. The
/// first error stops the other workers rather than letting them finish a queue that can hold
/// hundreds of thousands of entries.
fn each_blob(
    blobs: &[(&str, u64)],
    operation: impl Fn(&str, u64) -> Result<Option<u64>, SnapshotError> + Sync,
    progress: &mut impl FnMut(usize, usize),
) -> Result<Vec<(String, u64)>, SnapshotError> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let collected = std::sync::Mutex::new(Vec::new());
    let failure: std::sync::Mutex<Option<SnapshotError>> = std::sync::Mutex::new(None);

    std::thread::scope(|scope| {
        let workers = std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .min(blobs.len());
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            handles.push(scope.spawn(|| {
                let mut local = Vec::new();

                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(hash, size)) = blobs.get(index) else {
                        break;
                    };

                    match operation(hash, size) {
                        Ok(Some(size)) => local.push((hash.to_owned(), size)),
                        Ok(None) => {}
                        Err(error) => {
                            let mut slot = failure.lock().expect("blob mutex was poisoned");
                            if slot.is_none() {
                                *slot = Some(error);
                            }
                            stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                    }

                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                collected
                    .lock()
                    .expect("blob mutex was poisoned")
                    .push(local);
            }));
        }

        while handles.iter().any(|handle| !handle.is_finished()) {
            progress(done.load(std::sync::atomic::Ordering::Relaxed), blobs.len());
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });

    if let Some(error) = failure.into_inner().expect("blob mutex was poisoned") {
        return Err(error);
    }

    Ok(collected
        .into_inner()
        .expect("blob mutex was poisoned")
        .into_iter()
        .flatten()
        .collect())
}

/// Puts a snapshot's files back, both the bytes and the database rows.
///
/// Rows are validated and staged before blobs are linked back. The write lock covers both,
/// and the transaction commits only once the restored files are durable. Snapshot links stay
/// in place until commit, so a failed or interrupted restore retains its recovery data.
///
/// # Errors
///
/// Returns [`SnapshotError`] if a blob cannot be moved back or the database cannot be updated.
pub fn restore(
    library: &Library,
    snapshot: &Snapshot,
    progress: impl FnMut(CleanProgress),
) -> Result<usize, SnapshotError> {
    check_database_path(library)?;
    let _lock = OperationLock::acquire(library)?;
    let started = std::time::Instant::now();
    let snapshot = snapshot::reload(library, snapshot)?;
    tracing::info!(snapshot = %snapshot.dir.display(), "restore started");

    let restorations: Vec<Restoration> = snapshot
        .manifest
        .detached
        .iter()
        // Orphaned blobs were never attached to anything, so there is nothing to reattach.
        .filter(|c| c.set_index != crate::plan::NO_SET_INDEX)
        .map(|c| Restoration {
            set_id: c.set_id,
            filename: c.filename.clone(),
            hash: c.hash.clone(),
        })
        .collect();

    let progress = std::cell::RefCell::new(progress);
    let (rows, blobs) = {
        let realm = Realm::open_for_write(&library.database())?;
        realm.restore_usages_with(
            &restorations,
            |done, total| progress.borrow_mut()(CleanProgress::Reattaching { done, total }),
            || {
                snapshot::restore_blobs(library, &snapshot, &mut |done, total| {
                    progress.borrow_mut()(CleanProgress::Restoring { done, total });
                })
            },
        )?
    };

    // Every restored usage now owns its library link. Only now may the recovery links go.
    snapshot::remove_restored(library, &snapshot)?;

    let ms = elapsed_ms(started);
    tracing::info!(
        snapshot = %snapshot.dir.display(),
        blobs,
        rows,
        ms,
        blobs_per_s = crate::format::per_second(blobs as u64, ms),
        "restore finished"
    );
    Ok(blobs)
}

fn check_database_path(library: &Library) -> Result<(), SnapshotError> {
    let path = library.database();
    if !crate::safety::is_safe_path(&path, &[library.root()]) {
        return Err(SnapshotError::OutsideLibrary { path });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Category;
    use crate::plan::{Candidate, Group};

    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::storage::DATABASE_FILENAME), b"stub").unwrap();
        let library = Library::open(dir.path()).unwrap();
        (dir, library)
    }

    fn plan_with_one_selected() -> Plan {
        Plan {
            groups: vec![Group {
                category: Category::Junk,
                candidates: vec![Candidate {
                    set_index: 0,
                    set_id: [0; 16],
                    file_index: 0,
                    filename: "Thumbs.db".to_owned(),
                    hash: "a".repeat(64),
                    bytes: 4096,
                    category: Category::Junk,
                    usage_count: 1,
                }],
                selected: true,
            }],
            ..Plan::default()
        }
    }

    #[test]
    fn previewing_touches_nothing_and_still_reports() {
        let (_dir, library) = library();
        let outcome = run(
            &library,
            &plan_with_one_selected(),
            &Options::default(),
            |_| {},
        )
        .unwrap();

        assert!(outcome.dry_run);
        assert_eq!(outcome.bytes, 4096);
        assert_eq!(outcome.stashed, 0);
        assert!(outcome.snapshot.is_none());
        assert!(
            !library.snapshots_dir().exists(),
            "no snapshot may be created"
        );
    }

    #[test]
    fn an_empty_selection_does_nothing() {
        let (_dir, library) = library();
        let options = Options { dry_run: false };

        let outcome = run(&library, &Plan::default(), &options, |_| {}).unwrap();

        assert_eq!(outcome.detached, 0);
        assert!(outcome.snapshot.is_none());
    }

    fn current_library(candidate: &Candidate, references: u32) -> cleaner_realm::Library {
        cleaner_realm::Library {
            beatmap_sets: vec![cleaner_realm::BeatmapSet {
                index: candidate.set_index,
                id: candidate.set_id,
                files: vec![cleaner_realm::NamedFile {
                    index: 0,
                    filename: candidate.filename.clone(),
                    hash: candidate.hash.clone(),
                }],
                title: "Artist - Title".to_owned(),
                audio: Vec::new(),
                backgrounds: Vec::new(),
            }],
            usage_counts: HashMap::from([(candidate.hash.clone(), references)]),
        }
    }

    #[test]
    fn clean_rechecks_shared_blobs_and_shifted_indices() {
        let plan = plan_with_one_selected();
        let candidate = &plan.groups[0].candidates[0];
        let mut current = current_library(candidate, 2);
        assert!(
            validate_candidates(std::slice::from_ref(candidate), &current)
                .unwrap()
                .is_empty()
        );
        current.beatmap_sets[0].id = [1; 16];
        assert!(validate_candidates(std::slice::from_ref(candidate), &current).is_err());
        current.beatmap_sets[0].id = candidate.set_id;
        current.beatmap_sets[0].files[0].hash = "b".repeat(64);
        assert!(validate_candidates(std::slice::from_ref(candidate), &current).is_err());
    }

    #[test]
    fn clean_rejects_duplicates_and_protected_audio() {
        let candidate = plan_with_one_selected().groups[0].candidates[0].clone();
        let mut current = current_library(&candidate, 1);
        assert!(validate_candidates(&[candidate.clone(), candidate.clone()], &current).is_err());
        current.beatmap_sets[0]
            .audio
            .push(candidate.filename.to_uppercase());
        assert!(validate_candidates(&[candidate], &current).is_err());
    }

    #[test]
    fn cleaning_every_reference_to_a_shared_blob_frees_it() {
        let mut first = plan_with_one_selected().groups[0].candidates[0].clone();
        first.usage_count = 2;
        let mut second = first.clone();
        second.set_index = 1;
        second.set_id = [1; 16];

        let mut current = current_library(&first, 2);
        current
            .beatmap_sets
            .extend(current_library(&second, 2).beatmap_sets);
        let candidates = [first.clone(), second];

        assert_eq!(
            validate_candidates(&candidates, &current).unwrap(),
            vec![(first.hash.as_str(), 4096)],
            "detaching both usages leaves the blob unreferenced"
        );

        // A skin holds the third reference, so the blob has to stay.
        current.usage_counts.insert(first.hash.clone(), 3);
        assert!(
            validate_candidates(&candidates, &current)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_failed_clean_leaves_no_half_built_snapshot() {
        let (_dir, library) = library();
        let plan = plan_with_one_selected();
        // The database is a stub, so opening it for write fails after `begin` created the
        // temporary directory.
        assert!(run(&library, &plan, &Options { dry_run: false }, |_| {}).is_err());

        let leftovers: Vec<_> = std::fs::read_dir(library.snapshots_dir())
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn the_database_copy_is_written_flushed_and_moved_into_place() {
        let (_dir, library) = library();

        // Called on its own, so that a failure here cannot be mistaken for the rewrite
        // failing. Flushing is the step that behaves differently on Windows.
        let path = back_up_database(&library).expect("could not write the copy");

        assert_eq!(std::fs::read(&path).unwrap(), b"stub");
        assert_eq!(
            std::fs::read_dir(library.snapshots_dir()).unwrap().count(),
            1,
            "no half-written copy may be left beside it"
        );
    }

    #[test]
    fn compacting_keeps_a_copy_of_the_database() {
        let (_dir, library) = library();
        assert!(
            database_backups(&library).is_empty(),
            "nothing to fall back to yet"
        );

        // The stub is not a real database, so the rewrite fails. The copy must already exist.
        assert!(compact(&library).is_err());

        let copies = database_backups(&library);
        let [copy] = copies.as_slice() else {
            panic!("expected exactly one copy, got {copies:?}");
        };
        assert_eq!(std::fs::read(&copy.path).unwrap(), b"stub");
        assert_eq!(copy.bytes, 4);
        assert!(
            copy.name.starts_with(BACKUP_PREFIX) && copy.name.ends_with(BACKUP_SUFFIX),
            "a copy carries the time it was taken: {}",
            copy.name
        );

        remove_database_backup(&library, &copy.name).unwrap();
        assert!(database_backups(&library).is_empty());
        remove_database_backup(&library, &copy.name)
            .expect("removing a missing copy is not an error");
    }

    /// Writes `count` copies by hand. Going through `compact` would need a second between each
    /// one to earn a different name.
    fn write_copies(library: &Library, count: usize) -> Vec<String> {
        std::fs::create_dir_all(library.snapshots_dir()).unwrap();
        (1..=count)
            .map(|index| {
                let name = format!("{BACKUP_PREFIX}2026010{index}-000000{BACKUP_SUFFIX}");
                std::fs::write(library.snapshots_dir().join(&name), b"copy").unwrap();
                name
            })
            .collect()
    }

    #[test]
    fn copies_are_listed_newest_first() {
        let (_dir, library) = library();
        let written = write_copies(&library, 3);

        let listed: Vec<String> = database_backups(&library)
            .into_iter()
            .map(|copy| copy.name)
            .collect();
        let mut newest_first = written;
        newest_first.reverse();
        assert_eq!(listed, newest_first);
    }

    #[test]
    fn a_copy_version_1_3_wrote_is_still_listed() {
        let (_dir, library) = library();
        std::fs::create_dir_all(library.snapshots_dir()).unwrap();
        std::fs::write(library.snapshots_dir().join(LEGACY_BACKUP), b"copy").unwrap();

        let copies = database_backups(&library);
        assert_eq!(copies.len(), 1, "an older copy must still be offered");
        assert_eq!(copies[0].name, LEGACY_BACKUP);
        remove_database_backup(&library, LEGACY_BACKUP).unwrap();
        assert!(database_backups(&library).is_empty());
    }

    #[test]
    fn a_file_that_only_looks_like_a_copy_is_left_alone() {
        let (_dir, library) = library();
        std::fs::create_dir_all(library.snapshots_dir()).unwrap();
        let nested = library.snapshots_dir().join("client.realm.held");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("important.backup"), b"keep me").unwrap();
        let handmade = library.snapshots_dir().join("client.realm.mine.backup");
        std::fs::write(&handmade, b"keep me too").unwrap();

        assert!(
            database_backups(&library).is_empty(),
            "only copies this wrote are listed, and so only they are pruned"
        );
        for name in [
            "client.realm.held/important.backup",
            "client.realm.mine.backup",
            "client.realm.2026-09-09.backup",
        ] {
            assert!(
                remove_database_backup(&library, name).is_err(),
                "{name} does not record a time this wrote"
            );
        }
        assert!(nested.join("important.backup").exists());
        assert!(handmade.exists());
    }

    #[test]
    fn two_copies_in_the_same_second_both_survive() {
        let (_dir, library) = library();
        let first = back_up_database(&library).expect("could not write the first copy");
        std::fs::write(library.database(), b"newer stub").unwrap();
        let second = back_up_database(&library).expect("could not write the second copy");

        assert_ne!(first, second, "the second copy must not replace the first");
        assert_eq!(std::fs::read(&first).unwrap(), b"stub");
        assert_eq!(std::fs::read(&second).unwrap(), b"newer stub");
        assert_eq!(database_backups(&library).len(), 2);
    }

    #[test]
    fn the_copy_just_written_is_never_the_one_pruned() {
        let (_dir, library) = library();
        // Every copy already there is dated later than the one about to be written, as a clock
        // that has gone backwards would leave them.
        let existing: Vec<String> = (1..=MAX_DATABASE_BACKUPS)
            .map(|index| {
                let name = format!("{BACKUP_PREFIX}2999010{index}-000000{BACKUP_SUFFIX}");
                std::fs::create_dir_all(library.snapshots_dir()).unwrap();
                std::fs::write(library.snapshots_dir().join(&name), b"later").unwrap();
                name
            })
            .collect();

        let fresh = back_up_database(&library).expect("could not write the copy");

        assert!(
            fresh.exists(),
            "the copy the rewrite depends on must survive"
        );
        let left = database_backups(&library);
        assert_eq!(left.len(), MAX_DATABASE_BACKUPS);
        assert!(
            !left.iter().any(|copy| copy.name == existing[0]),
            "the oldest of the others is the one to go"
        );
    }

    #[test]
    fn a_copy_is_dated_by_its_name_not_by_the_file() {
        let (_dir, library) = library();
        write_copies(&library, 1);

        let copies = database_backups(&library);
        assert_eq!(
            copies[0].taken.strftime("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-01-01 00:00:00",
            "the name is where the time is recorded"
        );
    }

    #[test]
    fn only_the_newest_copies_are_kept() {
        let (_dir, library) = library();
        let written = write_copies(&library, MAX_DATABASE_BACKUPS + 2);

        prune_database_backups(&library, written.last().unwrap());

        let left: Vec<String> = database_backups(&library)
            .into_iter()
            .map(|copy| copy.name)
            .collect();
        assert_eq!(left.len(), MAX_DATABASE_BACKUPS);
        assert!(
            !left.contains(&written[0]),
            "the oldest copy must be the one to go"
        );
        assert!(left.contains(written.last().unwrap()));
    }

    #[test]
    fn only_the_copies_this_wrote_can_be_deleted() {
        let (_dir, library) = library();
        std::fs::create_dir_all(library.snapshots_dir()).unwrap();
        let bystander = library.snapshots_dir().join("notes.txt");
        std::fs::write(&bystander, b"keep me").unwrap();

        for name in ["notes.txt", "client.realm", "../client.realm", ""] {
            assert!(
                remove_database_backup(&library, name).is_err(),
                "{name} is not a copy this wrote"
            );
        }
        assert!(bystander.exists(), "a bystander must survive");
        assert!(library.database().exists(), "the database must survive");
    }

    #[test]
    fn blob_errors_do_not_leave_the_progress_loop_running() {
        let (_dir, library) = library();
        let count = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2 + 1;
        let names: Vec<_> = (0..count).map(|index| format!("invalid-{index}")).collect();
        let blobs: Vec<_> = names.iter().map(|name| (name.clone(), 1)).collect();
        let started = std::time::Instant::now();

        assert!(
            release_all(&library, library.root(), &blobs, &mut |_| {
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(10),
                    "workers did not finish"
                );
            })
            .is_err()
        );
    }

    #[test]
    fn a_reference_acquired_after_commit_prevents_release() {
        use cleaner_realm::fixture::{SetFixture, synthetic_realm};
        let directory = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        drop(synthetic_realm(
            &directory.path().join("client.realm"),
            &[SetFixture::new([1; 16], "First", &[("intro.mp4", &hash)])],
        ));
        let library = Library::open(directory.path()).unwrap();
        let blob = library.blob_path(&hash);
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"video").unwrap();
        let plan = crate::build_plan(
            &library,
            &std::collections::HashSet::from([Category::Videos]),
            |_| {},
        )
        .unwrap();
        run(&library, &plan, &Options { dry_run: false }, |_| {}).unwrap();
        let snapshot = snapshot::list(&library).unwrap().remove(0);

        // Reimport the blob before a delayed release operation gets the write lock.
        std::fs::write(&blob, b"video").unwrap();
        let realm = Realm::open_for_write(&library.database()).unwrap();
        realm
            .restore_usages(
                &[Restoration {
                    set_id: [1; 16],
                    filename: "intro.mp4".to_owned(),
                    hash: hash.clone(),
                }],
                |_, _| {},
            )
            .unwrap();
        drop(realm);
        release_unreferenced(
            &library,
            &snapshot.dir,
            &snapshot.manifest.blobs,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), b"video");
        assert!(snapshot.dir.join("blobs").join(hash).is_file());
    }
}
