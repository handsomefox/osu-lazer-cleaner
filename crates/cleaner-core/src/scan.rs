//! Building a plan from a library.
//!
//! Scanning never writes to the library. It copies the database to a scratch directory first,
//! because realm-core creates `client.realm.lock` and `client.realm.management/` beside any
//! database it opens, even read-only.

use crate::catalog::{self, Category};
use crate::error::ScanError;
use crate::osu::{self, SourceKind};
use crate::plan::{Candidate, Group, Plan, Progress};
use crate::skin;
use crate::storage::Library;
use cleaner_realm::Realm;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Extension of a difficulty file. Never a removal candidate.
const DIFFICULTY_EXTENSION: &str = ".osu";

/// Extension of a storyboard script.
const STORYBOARD_EXTENSION: &str = ".osb";

/// Scans `library` and returns what could be removed.
///
/// `selected` decides which categories start selected; everything is classified either way, so
/// the interface can toggle categories without rescanning.
///
/// # Errors
///
/// Returns [`ScanError`] if the database cannot be read or the blob store cannot be measured.
pub fn build_plan(
    library: &Library,
    selected: &HashSet<Category, impl std::hash::BuildHasher>,
    mut progress: impl FnMut(Progress),
) -> Result<Plan, ScanError> {
    let sizes = measure_blobs(library, &mut progress);

    let scratch = tempfile::tempdir().map_err(|source| ScanError::Io {
        action: "creating a scratch directory",
        path: library.root().to_path_buf(),
        source,
    })?;

    let database = copy_database(library, scratch.path())?;
    let realm = Realm::open_read_only(&database)?;

    let contents = realm.read_library()?;
    let sets = contents.beatmap_sets;
    let usage_counts = contents.usage_counts;

    let total_sets = sets.len();
    let mut candidates = classify_sets(library, &sets, &sizes, &usage_counts, &mut progress);

    candidates.extend(orphan_blobs(&sizes, &usage_counts));
    progress(Progress::Done);

    Ok(assemble(candidates, selected, total_sets, &sizes))
}

/// Classifies every set, reading their difficulty files in parallel.
///
/// Classification is almost entirely waiting on the filesystem: each set means opening its
/// `.osu` files to see what they reference. A large library holds well over a hundred thousand
/// of them, and reading those one at a time dominates the scan. Realm access stays on this
/// thread, because realm objects cannot cross one; only the plain data comes along.
fn classify_sets(
    library: &Library,
    sets: &[cleaner_realm::BeatmapSet],
    sizes: &HashMap<String, u64>,
    usage_counts: &HashMap<String, u32>,
    progress: &mut impl FnMut(Progress),
) -> Vec<Candidate> {
    let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);

    let chunks: Vec<&[cleaner_realm::BeatmapSet]> = sets.chunks(64).collect();
    let collected = std::sync::Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(chunk) = chunks.get(index) else {
                        break;
                    };

                    for set in *chunk {
                        local.extend(classify_set(library, set, sizes, usage_counts));
                    }
                    done.fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed);
                }

                collected
                    .lock()
                    .expect("classification mutex was poisoned")
                    .push(local);
            });
        }

        // Report from this thread, so the caller's closure never has to be `Sync`.
        while next.load(std::sync::atomic::Ordering::Relaxed) < chunks.len() {
            progress(Progress::ReadingSets {
                done: done.load(std::sync::atomic::Ordering::Relaxed),
                total: sets.len(),
            });
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });

    collected
        .into_inner()
        .expect("classification mutex was poisoned")
        .into_iter()
        .flatten()
        .collect()
}

/// Classifies one beatmap set's files.
fn classify_set(
    library: &Library,
    set: &cleaner_realm::BeatmapSet,
    sizes: &HashMap<String, u64>,
    usage_counts: &HashMap<String, u32>,
) -> Vec<Candidate> {
    let owned: BTreeSet<String> = set.files.iter().map(|f| f.filename.clone()).collect();
    let references = read_references(library, set, &owned);

    // Files a beatmap cannot play without. These are never candidates, whatever category
    // else they might match.
    let mut protected: BTreeSet<String> = references.audio.clone();
    protected.extend(
        set.files
            .iter()
            .map(|f| f.filename.clone())
            .filter(|name| has_extension(name, DIFFICULTY_EXTENSION)),
    );

    set.files
        .iter()
        .filter(|file| !protected.contains(&file.filename))
        .filter_map(|file| {
            let category = categorise(&file.filename, &references)?;
            let remaining = usage_counts
                .get(&file.hash)
                .copied()
                .unwrap_or(0)
                .saturating_sub(1);

            Some(Candidate {
                set_index: set.index,
                file_index: file.index,
                filename: file.filename.clone(),
                hash: file.hash.clone(),
                bytes: sizes.get(&file.hash).copied().unwrap_or(0),
                category,
                frees_blob: remaining == 0,
            })
        })
        .collect()
}

/// Decides which category a file belongs to, if any.
///
/// Order matters. Junk is checked first because nothing else should claim it. Videos are
/// matched by extension, which is how `BeatmapManager.DeleteVideos` classifies them. Skin
/// elements are checked before hitsounds so a file matching both lands in the more specific
/// category.
fn categorise(filename: &str, references: &osu::References) -> Option<Category> {
    if catalog::is_junk(filename) {
        return Some(Category::Junk);
    }

    if osu::VIDEO_EXTENSIONS
        .iter()
        .any(|extension| has_extension(filename, extension))
        || references.videos.contains(filename)
    {
        return Some(Category::Videos);
    }

    if has_extension(filename, STORYBOARD_EXTENSION) {
        return Some(Category::Storyboards);
    }

    // A background that a storyboard also draws stays a background, so that removing
    // storyboards does not take the backdrop with it.
    if references.backgrounds.contains(filename) {
        return Some(Category::Backgrounds);
    }

    if references.storyboard.contains(filename) {
        return Some(Category::Storyboards);
    }

    if skin::is_skin_element(filename) {
        return Some(Category::SkinElements);
    }

    if references.hitsounds.contains(filename) {
        return Some(Category::Hitsounds);
    }

    None
}

/// Reads and parses every difficulty and storyboard in a set.
fn read_references(
    library: &Library,
    set: &cleaner_realm::BeatmapSet,
    owned: &BTreeSet<String>,
) -> osu::References {
    let sources: Vec<(String, SourceKind)> = set
        .files
        .iter()
        .filter_map(|file| {
            let kind = if has_extension(&file.filename, DIFFICULTY_EXTENSION) {
                SourceKind::Difficulty
            } else if has_extension(&file.filename, STORYBOARD_EXTENSION) {
                SourceKind::Storyboard
            } else {
                return None;
            };

            let text = std::fs::read(library.blob_path(&file.hash)).ok()?;
            Some((String::from_utf8_lossy(&text).into_owned(), kind))
        })
        .collect();

    osu::parse_all(sources.iter().map(|(t, k)| (t.as_str(), *k)), owned)
}

/// Finds blobs on disk that no usage points at.
///
/// lazer sweeps these itself on every startup, so a healthy library has none. They appear when
/// a previous clean was interrupted, or when lazer crashed mid-import.
fn orphan_blobs(
    sizes: &HashMap<String, u64>,
    usage_counts: &HashMap<String, u32>,
) -> Vec<Candidate> {
    sizes
        .iter()
        .filter(|(hash, _)| !usage_counts.contains_key(*hash))
        .map(|(hash, bytes)| Candidate {
            set_index: usize::MAX,
            file_index: usize::MAX,
            filename: hash.clone(),
            hash: hash.clone(),
            bytes: *bytes,
            category: Category::Unreferenced,
            frees_blob: true,
        })
        .collect()
}

/// Groups candidates by category and computes totals.
fn assemble(
    candidates: Vec<Candidate>,
    selected: &HashSet<Category, impl std::hash::BuildHasher>,
    sets_scanned: usize,
    sizes: &HashMap<String, u64>,
) -> Plan {
    let mut by_category: HashMap<Category, Vec<Candidate>> = HashMap::new();
    for candidate in candidates {
        by_category
            .entry(candidate.category)
            .or_default()
            .push(candidate);
    }

    let groups = Category::ALL
        .iter()
        .map(|&category| {
            let candidates = by_category.remove(&category).unwrap_or_default();

            // Count each file once, so one shared between beatmap sets does not inflate the
            // totals. The candidate list keeps every reference, because each has to be
            // detached, but users should see how many files actually leave.
            let mut seen = HashSet::new();
            let freed: Vec<u64> = candidates
                .iter()
                .filter(|c| c.frees_blob && seen.insert(c.hash.as_str()))
                .map(|c| c.bytes)
                .collect();

            Group {
                category,
                candidates,
                files: freed.len(),
                bytes: freed.iter().sum(),
                selected: selected.contains(&category),
            }
        })
        .collect();

    Plan {
        groups,
        sets_scanned,
        blobs_total: sizes.len(),
        bytes_total: sizes.values().sum(),
    }
}

/// Walks the blob store once, recording every blob's size.
///
/// `RealmFile` has no size column, so sizes have to come from the filesystem. One walk is much
/// cheaper than a `stat` per database row: a large library holds hundreds of thousands of
/// blobs.
///
/// The walk runs in parallel across the store's top-level shards. osu!lazer names each blob
/// after its own hash and files it under `files/<first character>/`, so the tree is already
/// split sixteen ways and each shard can be walked independently. This matters most on
/// Windows, where directory traversal is the slowest part of a scan by a wide margin.
fn measure_blobs(library: &Library, progress: &mut impl FnMut(Progress)) -> HashMap<String, u64> {
    let files_dir = library.files_dir();

    let Ok(entries) = std::fs::read_dir(&files_dir) else {
        return HashMap::new();
    };

    let shards: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .collect();

    let next = std::sync::atomic::AtomicUsize::new(0);
    let counted = std::sync::atomic::AtomicUsize::new(0);
    let collected = std::sync::Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        let workers = shards
            .len()
            .min(std::thread::available_parallelism().map_or(4, std::num::NonZero::get) * 2);

        for _ in 0..workers {
            scope.spawn(|| {
                let mut local = HashMap::new();
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(shard) = shards.get(index) else {
                        break;
                    };

                    measure_shard(shard, &mut local, &counted);
                }

                collected
                    .lock()
                    .expect("blob measurement mutex was poisoned")
                    .push(local);
            });
        }

        // Report from this thread, so the caller's closure never has to be `Sync`.
        while next.load(std::sync::atomic::Ordering::Relaxed) < shards.len() {
            progress(Progress::MeasuringFiles {
                done: counted.load(std::sync::atomic::Ordering::Relaxed),
            });
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    collected
        .into_inner()
        .expect("blob measurement mutex was poisoned")
        .into_iter()
        .flatten()
        .collect()
}

/// Measures every blob under one shard directory.
fn measure_shard(
    shard: &Path,
    sizes: &mut HashMap<String, u64>,
    counted: &std::sync::atomic::AtomicUsize,
) {
    for entry in walkdir::WalkDir::new(shard).follow_links(false) {
        // A blob that vanished mid-walk is not an error: lazer may be tidying up.
        let Ok(entry) = entry else {
            continue;
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let Some(name) = entry.file_name().to_str() else {
            continue;
        };

        // Blob names are hex SHA-256. Anything else is lazer's own README.
        if !is_blob_name(name) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        sizes.insert(name.to_owned(), metadata.len());
        counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Copies the database out of the library so opening it cannot touch the original directory.
fn copy_database(library: &Library, scratch: &Path) -> Result<std::path::PathBuf, ScanError> {
    let source = library.database();
    let destination = scratch.join(crate::storage::DATABASE_FILENAME);

    std::fs::copy(&source, &destination).map_err(|source_error| ScanError::Io {
        action: "copying the database",
        path: source,
        source: source_error,
    })?;

    Ok(destination)
}

/// Reports whether a name is a 64-character lowercase hex digest.
fn is_blob_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Case-insensitive extension test, matching how lazer compares filenames.
fn has_extension(filename: &str, extension: &str) -> bool {
    filename.len() > extension.len()
        && filename[filename.len() - extension.len()..].eq_ignore_ascii_case(extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs_with_background(name: &str) -> osu::References {
        let mut references = osu::References::default();
        references.backgrounds.insert(name.to_owned());
        references
    }

    #[test]
    fn videos_are_matched_by_extension() {
        let empty = osu::References::default();
        assert_eq!(categorise("intro.mp4", &empty), Some(Category::Videos));
        assert_eq!(categorise("INTRO.AVI", &empty), Some(Category::Videos));
    }

    #[test]
    fn a_background_used_by_a_storyboard_stays_a_background() {
        let mut references = refs_with_background("bg.jpg");
        references.storyboard.insert("bg.jpg".to_owned());

        assert_eq!(
            categorise("bg.jpg", &references),
            Some(Category::Backgrounds)
        );
    }

    #[test]
    fn junk_wins_over_every_other_role() {
        let mut references = osu::References::default();
        references.storyboard.insert("Thumbs.db".to_owned());

        assert_eq!(categorise("Thumbs.db", &references), Some(Category::Junk));
    }

    #[test]
    fn unclassified_files_are_left_alone() {
        assert_eq!(categorise("readme.txt", &osu::References::default()), None);
    }

    #[test]
    fn storyboard_scripts_are_storyboards() {
        assert_eq!(
            categorise("map.osb", &osu::References::default()),
            Some(Category::Storyboards)
        );
    }

    #[test]
    fn blob_names_are_full_hex_digests() {
        assert!(is_blob_name(&"a".repeat(64)));
        assert!(!is_blob_name(&"a".repeat(63)));
        assert!(!is_blob_name("IMPORTANT READ ME.txt"));
    }

    #[test]
    fn extension_matching_ignores_case_and_needs_a_stem() {
        assert!(has_extension("map.OSU", ".osu"));
        assert!(!has_extension(".osu", ".osu"));
    }
}
