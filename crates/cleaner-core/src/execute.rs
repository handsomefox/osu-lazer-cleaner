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
            return Err(error);
        }
    };

    release_unreferenced(library, &final_dir, &blobs, &mut progress)?;

    Ok(Outcome {
        detached: removals,
        stashed: blobs.len(),
        bytes: blobs.iter().map(|(_, size)| size).sum(),
        snapshot: Some(final_dir),
        dry_run: false,
    })
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

/// Name of the copy of `client.realm` taken before a compaction.
///
/// One fixed name, so a library holds at most one of these and the user always knows which
/// compaction it belongs to.
const DATABASE_BACKUP: &str = "client.realm.backup";

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

    let before = measure("measuring the database")?;
    let backup = back_up_database(library)?;
    let rewritten = Realm::open_for_write(&path)?.compact()?;
    let after = measure("measuring the database")?;

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

    let destination = directory.join(DATABASE_BACKUP);
    let partial = directory.join(format!("{DATABASE_BACKUP}.partial"));
    let _ = std::fs::remove_file(&partial);

    let written = write_backup(library, &partial, &destination);
    if written.is_err() {
        // A half-written copy is worse than none: it would sit next to the snapshots looking
        // like something to fall back on.
        let _ = std::fs::remove_file(&partial);
    }
    written.map(|()| destination)
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

/// The copy of the database a compaction left behind, and its size.
///
/// Returns `None` when no compaction has run, or when its copy has been removed.
#[must_use]
pub fn database_backup(library: &Library) -> Option<(PathBuf, u64)> {
    let path = library.snapshots_dir().join(DATABASE_BACKUP);
    if !crate::safety::is_safe_path(&path, &[library.root()]) {
        return None;
    }
    let bytes = std::fs::symlink_metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)?
        .len();
    Some((path, bytes))
}

/// Deletes the copy of the database a compaction left behind.
///
/// # Errors
///
/// Returns [`SnapshotError::Io`] if the file exists and cannot be removed.
pub fn remove_database_backup(library: &Library) -> Result<(), SnapshotError> {
    let _lock = OperationLock::acquire(library)?;
    let Some((path, _)) = database_backup(library) else {
        return Ok(());
    };

    std::fs::remove_file(&path).map_err(|source| SnapshotError::Io {
        action: "deleting the database copy",
        path,
        source,
    })
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
    let snapshot = snapshot::reload(library, snapshot)?;

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

    tracing::info!(blobs, rows, "restored a snapshot");
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
            database_backup(&library).is_none(),
            "nothing to fall back to yet"
        );

        // The stub is not a real database, so the rewrite fails. The copy must already exist.
        assert!(compact(&library).is_err());

        let (path, bytes) = database_backup(&library).expect("no copy was kept");
        assert_eq!(std::fs::read(&path).unwrap(), b"stub");
        assert_eq!(bytes, 4);

        remove_database_backup(&library).unwrap();
        assert!(database_backup(&library).is_none());
        remove_database_backup(&library).expect("removing a missing copy is not an error");
    }

    #[test]
    fn a_second_compaction_replaces_the_previous_copy() {
        let (_dir, library) = library();
        assert!(compact(&library).is_err());

        std::fs::write(library.database(), b"newer stub").unwrap();
        assert!(compact(&library).is_err());

        let (path, _) = database_backup(&library).expect("no copy was kept");
        assert_eq!(std::fs::read(&path).unwrap(), b"newer stub");
        assert_eq!(
            std::fs::read_dir(library.snapshots_dir()).unwrap().count(),
            1,
            "copies must not accumulate"
        );
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
