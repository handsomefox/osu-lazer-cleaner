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
use std::collections::BTreeSet;
use std::path::PathBuf;

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
pub fn run(library: &Library, plan: &Plan, options: &Options) -> Result<Outcome, SnapshotError> {
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

    let schema_version = {
        let realm = Realm::open_for_write(&library.database())?;
        realm.erase_usages(&removals)?;
        realm.schema_version()
    };

    // Only blobs that reach zero usages may move. A file shared with another set, a skin, or a
    // replay keeps its bytes where they are.
    let freed: BTreeSet<&str> = candidates
        .iter()
        .filter(|c| c.frees_blob)
        .map(|c| c.hash.as_str())
        .collect();

    let mut blobs = Vec::new();
    let mut bytes = 0;

    for hash in freed {
        if snapshot::stash_blob(library, &snapshot_dir, hash)? {
            let size = candidates
                .iter()
                .find(|c| c.hash == hash)
                .map_or(0, |c| c.bytes);
            blobs.push((hash.to_owned(), size));
            bytes += size;
        }
    }

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

/// Puts a snapshot's files back, both the bytes and the database rows.
///
/// Blobs move back first, so the database never points at a file that is missing. Reattaching
/// the rows matters as much as the bytes: without them nothing refers to the restored files,
/// and osu!lazer would sweep them again on its next startup.
///
/// # Errors
///
/// Returns [`SnapshotError`] if a blob cannot be moved back or the database cannot be updated.
pub fn restore(library: &Library, snapshot: &Snapshot) -> Result<usize, SnapshotError> {
    let blobs = snapshot::restore_blobs(library, snapshot)?;

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

    let realm = Realm::open_for_write(&library.database())?;
    let rows = realm.restore_usages(&restorations)?;

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
                bytes: 4096,
                selected: true,
            }],
            ..Plan::default()
        }
    }

    #[test]
    fn previewing_touches_nothing_and_still_reports() {
        let (_dir, library) = library();
        let outcome = run(&library, &plan_with_one_selected(), &Options::default()).unwrap();

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

        let outcome = run(&library, &Plan::default(), &options).unwrap();

        assert_eq!(outcome.detached, 0);
        assert!(outcome.snapshot.is_none());
    }
}
