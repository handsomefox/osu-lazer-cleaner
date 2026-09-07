//! Running a clean, and undoing one.
//!
//! The order is deliberate. The database transaction commits first, then the blobs move. If
//! the process dies between the two, the library holds blobs nothing references, which lazer
//! sweeps on its next startup and this tool reports as unreferenced. The reverse order would
//! leave the database pointing at files that are gone.

use crate::error::SnapshotError;
use crate::plan::{Options, Plan};
use crate::snapshot::{self, Manifest, Snapshot};
use crate::storage::Library;
use cleaner_realm::{Realm, Removal, Restoration};
use std::collections::HashMap;
use std::path::PathBuf;

/// Progress reported while a clean runs.
#[derive(Debug, Clone, Copy)]
pub enum CleanProgress {
    /// Updating the database.
    UpdatingDatabase,
    /// Moving files into the snapshot.
    Moving {
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

    let snapshot_dir = snapshot::begin(library)?;

    // Orphaned blobs have no usage to detach; they are already unreferenced.
    let removals: Vec<Removal> = candidates
        .iter()
        .filter(|c| c.set_index != usize::MAX)
        .map(|c| Removal {
            set_index: c.set_index,
            file_index: c.file_index,
        })
        .collect();

    progress(CleanProgress::UpdatingDatabase);
    let schema_version = {
        let realm = Realm::open_for_write(&library.database())?;
        realm.erase_usages(&removals)?;
        realm.schema_version()
    };

    // Only blobs that reach zero usages may move. A file shared with another set, a skin, or a
    // replay keeps its bytes where they are.
    //
    // Sizes go into a map first. Looking each one up by scanning the candidate list meant a
    // pass over every candidate for every blob, which on a large clean is tens of billions of
    // string comparisons before a single file moves.
    let mut freed: HashMap<&str, u64> = HashMap::new();
    for candidate in candidates.iter().filter(|c| c.frees_blob) {
        freed.insert(candidate.hash.as_str(), candidate.bytes);
    }

    let moved = stash_all(library, &snapshot_dir, &freed, &mut progress)?;
    let blobs: Vec<(String, u64)> = moved;
    let bytes = blobs.iter().map(|(_, size)| size).sum();

    let manifest = Manifest {
        format_version: snapshot::FORMAT_VERSION,
        created: jiff::Timestamp::now(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        schema_version,
        detached: candidates.clone(),
        blobs: blobs.clone(),
    };

    let final_dir = snapshot::finalise(&snapshot_dir, &manifest)?;

    Ok(Outcome {
        detached: removals.len(),
        stashed: blobs.len(),
        bytes,
        snapshot: Some(final_dir),
        dry_run: false,
    })
}

/// Moves every freed blob into the snapshot, in parallel.
///
/// Each rename is an independent filesystem operation, and on Windows they are slow enough
/// individually that doing them one at a time dominates a large clean.
fn stash_all(
    library: &Library,
    snapshot_dir: &std::path::Path,
    freed: &HashMap<&str, u64>,
    progress: &mut impl FnMut(CleanProgress),
) -> Result<Vec<(String, u64)>, SnapshotError> {
    let hashes: Vec<(&str, u64)> = freed.iter().map(|(hash, size)| (*hash, *size)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let collected = std::sync::Mutex::new(Vec::new());
    let failure: std::sync::Mutex<Option<SnapshotError>> = std::sync::Mutex::new(None);

    std::thread::scope(|scope| {
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);

        for _ in 0..workers {
            scope.spawn(|| {
                let mut local = Vec::new();

                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(hash, size)) = hashes.get(index) else {
                        break;
                    };

                    match snapshot::stash_blob(library, snapshot_dir, hash) {
                        Ok(true) => local.push((hash.to_owned(), size)),
                        Ok(false) => {}
                        Err(error) => {
                            let mut slot = failure.lock().expect("stash mutex was poisoned");
                            if slot.is_none() {
                                *slot = Some(error);
                            }
                            break;
                        }
                    }

                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }

                collected
                    .lock()
                    .expect("stash mutex was poisoned")
                    .push(local);
            });
        }

        // Report from this thread, so the caller's closure never has to be `Sync`.
        while next.load(std::sync::atomic::Ordering::Relaxed) < hashes.len() {
            progress(CleanProgress::Moving {
                done: done.load(std::sync::atomic::Ordering::Relaxed),
                total: hashes.len(),
            });
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });

    if let Some(error) = failure.into_inner().expect("stash mutex was poisoned") {
        return Err(error);
    }

    Ok(collected
        .into_inner()
        .expect("stash mutex was poisoned")
        .into_iter()
        .flatten()
        .collect())
}

/// Puts a snapshot's files back, both the bytes and the database rows.
///
/// Blobs move back first, so the database never points at a file that is missing. Reattaching
/// the rows matters as much as the bytes: without them nothing refers to the restored files,
/// and osu!lazer would sweep them again on its next startup.
///
/// # Errors
///
/// Returns [`SnapshotError`] if a blob cannot be moved back or the database cannot be updated.
pub fn restore(
    library: &Library,
    snapshot: &Snapshot,
    mut progress: impl FnMut(CleanProgress),
) -> Result<usize, SnapshotError> {
    let blobs = snapshot::restore_blobs(library, snapshot, &mut |done, total| {
        progress(CleanProgress::Moving { done, total });
    })?;

    progress(CleanProgress::UpdatingDatabase);

    let restorations: Vec<Restoration> = snapshot
        .manifest
        .detached
        .iter()
        // Orphaned blobs were never attached to anything, so there is nothing to reattach.
        .filter(|c| c.set_index != usize::MAX)
        .map(|c| Restoration {
            set_index: c.set_index,
            filename: c.filename.clone(),
            hash: c.hash.clone(),
        })
        .collect();

    let rows = {
        let realm = Realm::open_for_write(&library.database())?;
        realm.restore_usages(&restorations)?
    };

    // A restored snapshot holds nothing: its files are back in the library. Leaving the
    // directory behind would keep advertising space it no longer occupies, and offering to
    // restore it a second time.
    snapshot::delete(library, snapshot)?;

    tracing::info!(blobs, rows, "restored a snapshot");
    Ok(blobs)
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
                    file_index: 0,
                    filename: "Thumbs.db".to_owned(),
                    hash: "a".repeat(64),
                    bytes: 4096,
                    category: Category::Junk,
                    frees_blob: true,
                }],
                files: 1,
                bytes: 4096,
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
}
