//! Running a clean, and undoing one.
//!
//! The order is deliberate. The database transaction commits first, then the blobs move. If
//! the process dies between the two, the library holds blobs nothing references, which lazer
//! sweeps on its next startup. A manifest is saved before either step so an interrupted clean
//! can be restored. The reverse order would leave the database pointing at files that are gone.

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

    check_database_path(library)?;
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
    let (final_dir, freed) = {
        let realm = Realm::open_for_write(&library.database())?;
        realm.erase_usages_with(|realm| {
            let current = realm.read_library()?;
            let freed = validate_candidates(&candidates, &current)?;
            let manifest = Manifest {
                format_version: snapshot::FORMAT_VERSION,
                created: jiff::Timestamp::now(),
                app_version: env!("CARGO_PKG_VERSION").to_owned(),
                schema_version: realm.schema_version(),
                detached: candidates.clone(),
                blobs: freed
                    .iter()
                    .filter(|(hash, _)| library.blob_path(hash).is_file())
                    .map(|(hash, size)| (hash.clone(), *size))
                    .collect(),
            };
            // Publish recovery data before committing or moving any bytes. An interrupted
            // clean remains restorable, including blobs that never left the library.
            let dir = snapshot::finalise(&snapshot_dir, &manifest)?;
            Ok::<_, SnapshotError>((removals.clone(), (dir, freed)))
        })?
    };

    // Only blobs that reach zero usages may move. A file shared with another set, a skin, or a
    // replay keeps its bytes where they are.
    //
    // Sizes go into a map first. Looking each one up by scanning the candidate list meant a
    // pass over every candidate for every blob, which on a large clean is tens of billions of
    // string comparisons before a single file moves.
    let freed = freed
        .iter()
        .map(|(hash, size)| (hash.as_str(), *size))
        .collect();
    let moved = stash_all(library, &final_dir, &freed, &mut progress)?;
    let blobs: Vec<(String, u64)> = moved;
    let bytes = blobs.iter().map(|(_, size)| size).sum();

    Ok(Outcome {
        detached: removals.len(),
        stashed: blobs.len(),
        bytes,
        snapshot: Some(final_dir),
        dry_run: false,
    })
}

/// Rejects shifted indices and recomputes which blobs lose their last owner.
fn validate_candidates(
    candidates: &[crate::Candidate],
    current: &cleaner_realm::Library,
) -> Result<HashMap<String, u64>, SnapshotError> {
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
        if candidate.set_index == usize::MAX {
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
            || set
                .audio
                .iter()
                .any(|audio| audio.eq_ignore_ascii_case(&file.filename))
            || !seen.insert((candidate.set_index, candidate.file_index))
        {
            return Err(SnapshotError::StalePlan);
        }
        let count = remaining
            .get_mut(&candidate.hash)
            .ok_or(SnapshotError::StalePlan)?;
        *count = count.checked_sub(1).ok_or(SnapshotError::StalePlan)?;
    }
    Ok(candidates
        .iter()
        .filter(|candidate| {
            candidate.frees_blob && remaining.get(&candidate.hash).copied().unwrap_or(0) == 0
        })
        .map(|candidate| (candidate.hash.clone(), candidate.bytes))
        .collect())
}

/// What compacting the database did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// operation that makes it smaller.
///
/// # Errors
///
/// Returns [`SnapshotError`] if the database cannot be opened or measured.
pub fn compact(library: &Library) -> Result<Compaction, SnapshotError> {
    check_database_path(library)?;
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
    let rewritten = Realm::open_for_write(&path)?.compact()?;
    let after = measure("measuring the database")?;

    Ok(Compaction {
        rewritten,
        before,
        after,
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
        let workers = std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .min(hashes.len());
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            handles.push(scope.spawn(|| {
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
            }));
        }

        // Report from this thread, so the caller's closure never has to be `Sync`.
        while handles.iter().any(|handle| !handle.is_finished()) {
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
    check_database_path(library)?;
    let blobs = snapshot::restore_blobs(library, snapshot, &mut |done, total| {
        progress(CleanProgress::Moving { done, total });
    })?;

    let restorations: Vec<Restoration> = snapshot
        .manifest
        .detached
        .iter()
        // Orphaned blobs were never attached to anything, so there is nothing to reattach.
        .filter(|c| c.set_index != usize::MAX)
        .map(|c| Restoration {
            set_id: c.set_id,
            set_index: c.set_index,
            filename: c.filename.clone(),
            hash: c.hash.clone(),
        })
        .collect();

    let rows = {
        let realm = Realm::open_for_write(&library.database())?;
        realm.restore_usages(&restorations, |done, total| {
            progress(CleanProgress::Reattaching { done, total });
        })?
    };

    // A restored snapshot holds nothing: its files are back in the library. Leaving the
    // directory behind would keep advertising space it no longer occupies, and offering to
    // restore it a second time.
    snapshot::delete(library, snapshot)?;

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
    fn stash_errors_do_not_leave_the_progress_loop_running() {
        let (_dir, library) = library();
        let count = std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2 + 1;
        let names: Vec<_> = (0..count).map(|index| format!("invalid-{index}")).collect();
        let freed = names.iter().map(|name| (name.as_str(), 1)).collect();
        let started = std::time::Instant::now();
        assert!(
            stash_all(&library, library.root(), &freed, &mut |_| {
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(10),
                    "stash workers did not finish"
                );
            })
            .is_err()
        );
    }
}
