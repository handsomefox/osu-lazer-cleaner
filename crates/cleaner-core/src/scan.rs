//! Building a plan from a library.
//!
//! Scanning never writes to the library. It copies the database to a scratch directory first,
//! because realm-core creates `client.realm.lock` and `client.realm.management/` beside any
//! database it opens, even read-only.

use crate::catalog::{self, Category};
use crate::error::ScanError;
use crate::osu::{self, SourceKind};
use crate::plan::{Candidate, Group, NO_SET, NO_SET_INDEX, Plan, Progress, SetId, Timings};
use crate::skin;
use crate::storage::Library;
use cleaner_realm::Realm;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Extension of a difficulty file. Never a removal candidate.
const DIFFICULTY_EXTENSION: &str = ".osu";

/// Extension of a storyboard script.
const STORYBOARD_EXTENSION: &str = ".osb";

/// Case-folded names for classification. Keep the original spellings in the parser and plan.
struct ReferenceIndex {
    audio: HashSet<String>,
    backgrounds: HashSet<String>,
    videos: HashSet<String>,
    storyboard: HashSet<String>,
    hitsounds: HashSet<String>,
}

impl From<&osu::References> for ReferenceIndex {
    fn from(references: &osu::References) -> Self {
        let keys =
            |names: &BTreeSet<String>| names.iter().map(|name| osu::filename_key(name)).collect();
        Self {
            audio: keys(&references.audio),
            backgrounds: keys(&references.backgrounds),
            videos: keys(&references.videos),
            storyboard: keys(&references.storyboard),
            hitsounds: keys(&references.hitsounds),
        }
    }
}

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
    let started = std::time::Instant::now();
    let sizes = measure_blobs(library, &mut progress);
    let measure_ms = elapsed_ms(started);

    let database_started = std::time::Instant::now();
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

    let database_ms = elapsed_ms(database_started);

    let classify_started = std::time::Instant::now();
    let total_sets = sets.len();
    let (mut candidates, sets_parsed) =
        classify_sets(library, &sets, &sizes, &usage_counts, &mut progress);
    let classify_ms = elapsed_ms(classify_started);

    candidates.extend(orphan_blobs(&sizes, &usage_counts));
    progress(Progress::Done);

    let titles = set_titles(&sets, &candidates);
    let mut plan = assemble(candidates, titles, selected, total_sets, &sizes);
    plan.timings = Timings {
        measure_ms,
        database_ms,
        classify_ms,
        sets_parsed,
    };
    Ok(plan)
}

/// Milliseconds since `started`, saturating rather than panicking on a huge value.
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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
) -> (Vec<Candidate>, usize) {
    let parsed = std::sync::atomic::AtomicUsize::new(0);
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
                        let (candidates, was_parsed) =
                            classify_set(library, set, sizes, usage_counts);
                        local.extend(candidates);
                        if was_parsed {
                            parsed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
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

    let candidates = collected
        .into_inner()
        .expect("classification mutex was poisoned")
        .into_iter()
        .flatten()
        .collect();

    (
        candidates,
        parsed.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Classifies one beatmap set's files.
///
/// Also reports whether the set's difficulty files had to be opened.
fn classify_set(
    library: &Library,
    set: &cleaner_realm::BeatmapSet,
    sizes: &HashMap<String, u64>,
    usage_counts: &HashMap<String, u32>,
) -> (Vec<Candidate>, bool) {
    // Start from what the database already knows, which costs nothing to read.
    let mut references = osu::References {
        audio: set.audio.iter().cloned().collect(),
        backgrounds: set.backgrounds.iter().cloned().collect(),
        ..osu::References::default()
    };

    // Only open the difficulty files when the set owns something they might explain. Most
    // sets are just difficulties, one audio track, and one background, and reading those
    // files was over 99% of a scan's time on a large library. Building the name index costs
    // one allocation per file, so it waits until something is going to use it.
    let parsed = needs_parsing(set, &ReferenceIndex::from(&references));
    if parsed {
        let owned: BTreeSet<String> = set.files.iter().map(|f| f.filename.clone()).collect();
        references.absorb(read_references(library, set, &owned));
    }

    // Files a beatmap cannot play without. These are never candidates, whatever category else
    // they might match. Names go into a set folded the way lazer folds them, because comparing
    // each file against each protected name in turn is quadratic in a set of thirty
    // difficulties.
    let references = ReferenceIndex::from(&references);
    let mut protected = references.audio.clone();
    protected.extend(
        set.files
            .iter()
            .filter(|f| has_extension(&f.filename, DIFFICULTY_EXTENSION))
            .map(|f| osu::filename_key(&f.filename)),
    );

    let candidates = set
        .files
        .iter()
        .filter(|file| !protected.contains(&osu::filename_key(&file.filename)))
        .filter_map(|file| {
            let category = categorise(&file.filename, &references)?;

            Some(Candidate {
                set_index: set.index,
                set_id: set.id,
                file_index: file.index,
                filename: file.filename.clone(),
                hash: file.hash.clone(),
                bytes: sizes.get(&file.hash).copied().unwrap_or(0),
                category,
                usage_count: usage_counts.get(&file.hash).copied().unwrap_or(0),
            })
        })
        .collect();

    (candidates, parsed)
}

/// Reports whether a set owns anything that only its difficulty files can explain.
///
/// Storyboard art and custom hitsounds are named inside `.osu` and `.osb` files, so finding
/// them means reading those files. Everything else is settled by the database or by the
/// filename: audio and background come from `BeatmapMetadata`, videos and junk from the
/// extension, skin elements from the name.
///
/// A set therefore needs parsing only when it owns a file that none of those explain. When it
/// owns nothing but difficulties, its audio track, and its background, there is nothing left
/// for parsing to find.
fn needs_parsing(set: &cleaner_realm::BeatmapSet, known: &ReferenceIndex) -> bool {
    set.files.iter().any(|file| {
        let name = &file.filename;
        let key = osu::filename_key(name);

        if has_extension(name, STORYBOARD_EXTENSION) {
            return true;
        }

        let explained = has_extension(name, DIFFICULTY_EXTENSION)
            || known.audio.contains(&key)
            || known.backgrounds.contains(&key)
            || catalog::is_junk(name)
            || skin::is_skin_element(name)
            || osu::VIDEO_EXTENSIONS
                .iter()
                .any(|extension| has_extension(name, extension));

        !explained
    })
}

/// Decides which category a file belongs to, if any.
///
/// Order matters. Junk is checked first because nothing else should claim it. Videos are
/// matched by extension, which is how `BeatmapManager.DeleteVideos` classifies them. Skin
/// elements are checked before hitsounds so a file matching both lands in the more specific
/// category.
fn categorise(filename: &str, references: &ReferenceIndex) -> Option<Category> {
    let key = osu::filename_key(filename);
    if catalog::is_junk(filename) {
        return Some(Category::Junk);
    }

    if osu::VIDEO_EXTENSIONS
        .iter()
        .any(|extension| has_extension(filename, extension))
        || references.videos.contains(&key)
    {
        return Some(Category::Videos);
    }

    if has_extension(filename, STORYBOARD_EXTENSION) {
        return Some(Category::Storyboards);
    }

    // A background that a storyboard also draws stays a background, so that removing
    // storyboards does not take the backdrop with it.
    if references.backgrounds.contains(&key) {
        return Some(Category::Backgrounds);
    }

    if references.storyboard.contains(&key) {
        return Some(Category::Storyboards);
    }

    if skin::is_skin_element(filename) {
        return Some(Category::SkinElements);
    }

    if references.hitsounds.contains(&key) {
        return Some(Category::Hitsounds);
    }

    None
}

/// Reads and parses every difficulty and storyboard in a set.
///
/// Reading beatmaps is by far the slowest part of a scan: 145 of the 147 seconds a 185 GB
/// library took, against 0.4 seconds walking its 454,820 files. Almost all of those bytes are
/// `[HitObjects]`, and for most sets that section can name nothing the rest of the file does
/// not, so `sample_depth` decides whether to read it at all.
fn read_references(
    library: &Library,
    set: &cleaner_realm::BeatmapSet,
    owned: &BTreeSet<String>,
) -> osu::References {
    let depth = sample_depth(set);

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

            // A storyboard has no `[HitObjects]` and is small, so it is always read whole.
            let stop_at_hit_objects = depth == Depth::Events && kind == SourceKind::Difficulty;
            Some((
                read_source(&library.blob_path(&file.hash), stop_at_hit_objects)?,
                kind,
            ))
        })
        .collect();

    osu::parse_all(sources.iter().map(|(t, k)| (t.as_str(), *k)), owned)
}

/// How much of a difficulty has to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// Up to `[HitObjects]`, which is where the sections that name files end.
    Events,
    /// All of it, because the set owns a sample only a hit object can name.
    Samples,
}

/// Decides whether a set's `[HitObjects]` and `[TimingPoints]` are worth reading.
///
/// Those two sections only ever name sample files. `[TimingPoints]` resolves a custom sample
/// index against the files the set owns, and a hit object either does the same or names a file
/// outright. A set that owns no sample beyond its audio track therefore has nothing for them to
/// find, and skipping them skips most of every difficulty.
///
/// Getting this wrong can only leave a file in the library: an unread section means one fewer
/// reference, which means one fewer candidate.
fn sample_depth(set: &cleaner_realm::BeatmapSet) -> Depth {
    let owns_a_sample = set.files.iter().any(|file| {
        osu::AUDIO_EXTENSIONS
            .iter()
            .any(|extension| has_extension(&file.filename, extension))
            && !set
                .audio
                .iter()
                .any(|track| osu::filename_key(track) == osu::filename_key(&file.filename))
    });

    if owns_a_sample {
        Depth::Samples
    } else {
        Depth::Events
    }
}

/// Reads a source file, optionally stopping once `[HitObjects]` begins.
///
/// Returns `None` when the file cannot be read, which a scan treats as a set with nothing to
/// say rather than as an error.
fn read_source(path: &Path, stop_at_hit_objects: bool) -> Option<String> {
    if !stop_at_hit_objects {
        let bytes = std::fs::read(path).ok()?;
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }

    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut text = String::new();
    let mut line = Vec::new();

    loop {
        line.clear();
        if std::io::BufRead::read_until(&mut reader, b'\n', &mut line).ok()? == 0 {
            break;
        }

        let decoded = String::from_utf8_lossy(&line);
        if decoded.trim().eq_ignore_ascii_case("[HitObjects]") {
            break;
        }
        text.push_str(&decoded);
    }

    Some(text)
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
            set_index: NO_SET_INDEX,
            set_id: NO_SET,
            file_index: NO_SET_INDEX,
            filename: hash.clone(),
            hash: hash.clone(),
            bytes: *bytes,
            category: Category::Unreferenced,
            usage_count: 0,
        })
        .collect()
}

/// Labels every beatmap set that owns at least one candidate.
///
/// Sets with nothing to clean are left out: a large library holds tens of thousands of them,
/// and the browse list never shows one.
fn set_titles(
    sets: &[cleaner_realm::BeatmapSet],
    candidates: &[Candidate],
) -> BTreeMap<SetId, String> {
    let owners: HashSet<SetId> = candidates.iter().map(|c| c.set_id).collect();
    sets.iter()
        .filter(|set| owners.contains(&set.id) && !set.title.is_empty())
        .map(|set| (set.id, set.title.clone()))
        .collect()
}

/// Groups candidates by category.
fn assemble(
    candidates: Vec<Candidate>,
    set_titles: BTreeMap<SetId, String>,
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
        .map(|&category| Group {
            category,
            candidates: by_category.remove(&category).unwrap_or_default(),
            selected: selected.contains(&category),
        })
        .collect();

    Plan {
        timings: Timings::default(),
        groups,
        set_titles,
        excluded: HashSet::new(),
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
/// The walk runs in parallel across the store's directories. osu!lazer names each blob after
/// its own hash and files it under `files/<first character>/<first two characters>/`, so the
/// tree is already split 256 ways and each directory can be walked independently. Splitting at
/// the second level rather than the first matters because the work has to divide evenly: with
/// sixteen units and sixteen workers, one large directory holds up the whole scan. This is the
/// slowest part of a scan on Windows by a wide margin.
fn measure_blobs(library: &Library, progress: &mut impl FnMut(Progress)) -> HashMap<String, u64> {
    let shards = blob_shards(&library.files_dir());

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

/// Lists the directories a blob store's walk can be split across.
///
/// Prefers the second level, and falls back to the first for a store lazer has not filled in
/// yet, or to the store itself when neither can be read.
fn blob_shards(files_dir: &Path) -> Vec<std::path::PathBuf> {
    let subdirectories = |path: &Path| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(path).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|entry| entry.path())
                    .collect()
            },
        )
    };

    let first: Vec<_> = subdirectories(files_dir);
    if first.is_empty() {
        return Vec::new();
    }

    let second: Vec<_> = first.iter().flat_map(|dir| subdirectories(dir)).collect();
    if second.is_empty() { first } else { second }
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
        && filename.as_bytes()[filename.len() - extension.len()..]
            .eq_ignore_ascii_case(extension.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_filenames_do_not_panic_during_extension_checks() {
        assert!(!has_extension("aああ", ".mp4"));
        assert!(has_extension("あ.MP4", ".mp4"));
    }

    #[test]
    fn a_video_two_sets_share_counts_once_and_still_frees_its_bytes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(crate::storage::DATABASE_FILENAME),
            b"stub",
        )
        .unwrap();
        let library = Library::open(directory.path()).unwrap();

        let first = set_with(&["intro.mp4"], &[], &[]);
        let mut second = first.clone();
        second.index = 1;
        second.id = [8; 16];
        second.title = "Artist - Second".to_owned();

        let hash = first.files[0].hash.clone();
        let sizes = HashMap::from([(hash.clone(), 500)]);
        let usage_counts = HashMap::from([(hash, 2)]);

        let (mut candidates, _) = classify_set(&library, &first, &sizes, &usage_counts);
        candidates.extend(classify_set(&library, &second, &sizes, &usage_counts).0);
        let sets = [first, second];
        let titles = set_titles(&sets, &candidates);
        let plan = assemble(
            candidates,
            titles,
            &HashSet::from([Category::Videos]),
            2,
            &sizes,
        );

        let totals = plan.category_totals(Category::Videos);
        assert_eq!((totals.files, totals.bytes, totals.references), (1, 500, 2));
        assert_eq!(plan.selected_totals().files, 1);
        assert_eq!(plan.selected_totals().bytes, 500);

        let report = crate::report::summarise(&plan, "test");
        let category = report
            .categories
            .iter()
            .find(|group| group.category == "videos")
            .unwrap();
        assert_eq!(category.kept_shared, 0);
        assert_eq!(report.sharing.shared_references, 2);

        assert_eq!(plan.sets_in(Category::Videos).len(), 2);
        assert_eq!(plan.sets_in(Category::Videos)[0].title, "Artist - First");
    }

    #[test]
    fn sets_with_nothing_to_clean_are_not_labelled() {
        let with_video = set_with(&["intro.mp4"], &[], &[]);
        let mut without = with_video.clone();
        without.index = 1;
        without.id = [8; 16];
        without.files.clear();

        let candidates = vec![Candidate {
            set_index: 0,
            set_id: with_video.id,
            file_index: 0,
            filename: "intro.mp4".to_owned(),
            hash: "a".repeat(64),
            bytes: 1,
            category: Category::Videos,
            usage_count: 1,
        }];

        let titles = set_titles(&[with_video, without], &candidates);
        assert_eq!(titles.len(), 1);
        assert_eq!(titles.get(&[7; 16]).unwrap(), "Artist - First");
    }

    /// Classifies one set against a library with no blobs on disk.
    fn classify(set: &cleaner_realm::BeatmapSet) -> Vec<Candidate> {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(crate::storage::DATABASE_FILENAME),
            b"stub",
        )
        .unwrap();
        let library = Library::open(directory.path()).unwrap();

        let usage_counts = set.files.iter().map(|f| (f.hash.clone(), 1)).collect();
        classify_set(&library, set, &HashMap::new(), &usage_counts).0
    }

    #[test]
    fn difficulties_and_audio_are_never_candidates() {
        let set = set_with(
            &["Map [Easy].osu", "Map [Hard].OSU", "Audio.MP3", "intro.mp4"],
            &["audio.mp3"],
            &[],
        );

        let names: Vec<_> = classify(&set)
            .into_iter()
            .map(|candidate| candidate.filename)
            .collect();

        assert_eq!(
            names,
            vec!["intro.mp4"],
            "only the video may be removed; the case of the others must not matter"
        );
    }

    #[test]
    fn a_video_named_as_the_audio_track_is_protected() {
        // A set whose `AudioFilename` points at the video is unusual and legal. Removing it
        // would leave the beatmap silent.
        let set = set_with(&["Map.osu", "intro.mp4"], &["INTRO.MP4"], &[]);
        assert!(
            classify(&set).is_empty(),
            "the audio track is protected however it is spelled"
        );
    }

    #[test]
    fn a_second_level_blob_store_is_walked_directory_by_directory() {
        let directory = tempfile::tempdir().unwrap();
        let files = directory.path().join("files");
        for shard in ["a", "b"] {
            for nested in ["0", "1"] {
                std::fs::create_dir_all(files.join(shard).join(format!("{shard}{nested}")))
                    .unwrap();
            }
        }

        let shards = blob_shards(&files);
        assert_eq!(shards.len(), 4, "one unit per second-level directory");
        assert!(shards.iter().all(|path| path.starts_with(&files)));
    }

    #[test]
    fn a_blob_store_lazer_has_not_nested_yet_still_divides() {
        let directory = tempfile::tempdir().unwrap();
        let files = directory.path().join("files");
        std::fs::create_dir_all(files.join("a")).unwrap();

        assert_eq!(blob_shards(&files).len(), 1, "fall back to the first level");
        assert!(blob_shards(&directory.path().join("missing")).is_empty());
    }

    #[test]
    fn a_set_with_no_samples_stops_before_the_hit_objects() {
        let mut set = set_with(
            &["Map.osu", "audio.mp3", "bg.jpg"],
            &["audio.mp3"],
            &["bg.jpg"],
        );
        assert_eq!(sample_depth(&set), Depth::Events);

        // Its own audio track does not count, whatever case it is written in.
        set.audio = vec!["AUDIO.MP3".to_owned()];
        assert_eq!(sample_depth(&set), Depth::Events);
    }

    #[test]
    fn a_set_owning_a_sample_is_read_to_the_end() {
        for sample in ["soft-hitclap.wav", "clap.ogg", "voice.mp3"] {
            let set = set_with(&["Map.osu", "audio.mp3", sample], &["audio.mp3"], &[]);
            assert_eq!(
                sample_depth(&set),
                Depth::Samples,
                "{sample} can only be named by a hit object"
            );
        }
    }

    #[test]
    fn reading_stops_at_the_hit_objects_header() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Map.osu");
        std::fs::write(
            &path,
            "[General]\nAudioFilename: audio.mp3\n[Events]\n0,0,\"bg.jpg\",0,0\n\
             [HitObjects]\n256,192,1000,1,0,0:0:0:0:secret.wav\n",
        )
        .unwrap();

        let stopped = read_source(&path, true).unwrap();
        assert!(stopped.contains("bg.jpg"), "the events must survive");
        assert!(!stopped.contains("secret.wav"), "the hit objects must not");
        assert!(!stopped.contains("[HitObjects]"));

        let whole = read_source(&path, false).unwrap();
        assert!(whole.contains("secret.wav"));

        assert!(read_source(&directory.path().join("missing.osu"), true).is_none());
    }

    #[test]
    fn a_shorter_read_can_only_leave_files_alone() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(crate::storage::DATABASE_FILENAME),
            b"stub",
        )
        .unwrap();
        let library = Library::open(directory.path()).unwrap();

        let set = set_with(
            &["Map.osu", "audio.mp3", "bg.jpg"],
            &["audio.mp3"],
            &["bg.jpg"],
        );
        let difficulty = library.blob_path(&set.files[0].hash);
        std::fs::create_dir_all(difficulty.parent().unwrap()).unwrap();
        std::fs::write(
            &difficulty,
            "[General]\nAudioFilename: audio.mp3\n[Events]\n\
             Sprite,Background,Centre,\"bg.jpg\",320,240\n\
             [HitObjects]\n256,192,1000,1,0,0:0:0:0:\n",
        )
        .unwrap();

        let owned = set.files.iter().map(|f| f.filename.clone()).collect();
        let references = read_references(&library, &set, &owned);
        assert!(
            references.storyboard.contains("bg.jpg"),
            "everything before the hit objects is still read"
        );
    }

    fn refs_with_background(name: &str) -> osu::References {
        let mut references = osu::References::default();
        references.backgrounds.insert(name.to_owned());
        references
    }

    fn set_with(files: &[&str], audio: &[&str], backgrounds: &[&str]) -> cleaner_realm::BeatmapSet {
        cleaner_realm::BeatmapSet {
            index: 0,
            id: [7; 16],
            title: "Artist - First".to_owned(),
            files: files
                .iter()
                .enumerate()
                .map(|(index, name)| cleaner_realm::NamedFile {
                    index,
                    filename: (*name).to_owned(),
                    hash: format!("{index:064}"),
                })
                .collect(),
            audio: audio.iter().map(|s| (*s).to_owned()).collect(),
            backgrounds: backgrounds.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn known(set: &cleaner_realm::BeatmapSet) -> ReferenceIndex {
        ReferenceIndex::from(&osu::References {
            audio: set.audio.iter().cloned().collect(),
            backgrounds: set.backgrounds.iter().cloned().collect(),
            ..osu::References::default()
        })
    }

    #[test]
    fn an_ordinary_set_needs_no_parsing() {
        // Difficulties, one audio track, one background: the database explains all of it.
        let set = set_with(
            &["Map [Easy].osu", "Map [Hard].osu", "audio.mp3", "bg.jpg"],
            &["audio.mp3"],
            &["bg.jpg"],
        );
        assert!(!needs_parsing(&set, &known(&set)));
    }

    #[test]
    fn a_storyboard_always_needs_parsing() {
        let set = set_with(&["Map.osu", "audio.mp3", "Map.osb"], &["audio.mp3"], &[]);
        assert!(needs_parsing(&set, &known(&set)));
    }

    #[test]
    fn an_unexplained_file_needs_parsing() {
        // The extra sample could be a custom hitsound, which only the difficulty names.
        let set = set_with(
            &["Map.osu", "audio.mp3", "bg.jpg", "soft-hitwhistle2.wav"],
            &["audio.mp3"],
            &["bg.jpg"],
        );
        assert!(needs_parsing(&set, &known(&set)));
    }

    #[test]
    fn junk_and_skin_files_do_not_trigger_parsing() {
        // Both are recognised by name, so reading the difficulties would find nothing new.
        let set = set_with(
            &[
                "Map.osu",
                "audio.mp3",
                "bg.jpg",
                "Thumbs.db",
                "hitcircle.png",
            ],
            &["audio.mp3"],
            &["bg.jpg"],
        );
        assert!(!needs_parsing(&set, &known(&set)));
    }

    #[test]
    fn videos_are_matched_by_extension() {
        let empty = ReferenceIndex::from(&osu::References::default());
        assert_eq!(categorise("intro.mp4", &empty), Some(Category::Videos));
        assert_eq!(categorise("INTRO.AVI", &empty), Some(Category::Videos));
    }

    #[test]
    fn a_background_used_by_a_storyboard_stays_a_background() {
        let mut references = refs_with_background("bg.jpg");
        references.storyboard.insert("bg.jpg".to_owned());

        assert_eq!(
            categorise("bg.jpg", &ReferenceIndex::from(&references)),
            Some(Category::Backgrounds)
        );
    }

    #[test]
    fn mixed_case_backgrounds_never_become_storyboard_candidates() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(crate::storage::DATABASE_FILENAME),
            b"stub",
        )
        .unwrap();
        let library = Library::open(directory.path()).unwrap();
        let set = set_with(
            &["Map.osu", "SB/BG.JPG", "voice.WAV", "ÉCRAN.MP4"],
            &["écran.mp4"],
            &["sb/bg.jpg"],
        );
        let difficulty = library.blob_path(&set.files[0].hash);
        std::fs::create_dir_all(difficulty.parent().unwrap()).unwrap();
        std::fs::write(
            difficulty,
            "[Events]\nSprite,Background,Centre,\"sb\\bg.jpg\",320,240\n\
             [HitObjects]\n256,192,1000,1,0,0:0:0:0:VOICE.wav\n",
        )
        .unwrap();

        let (candidates, parsed) = classify_set(&library, &set, &HashMap::new(), &HashMap::new());
        assert!(parsed);
        let roles: Vec<_> = candidates
            .iter()
            .map(|c| (c.filename.as_str(), c.category))
            .collect();
        assert_eq!(
            roles,
            vec![
                ("SB/BG.JPG", Category::Backgrounds),
                ("voice.WAV", Category::Hitsounds)
            ]
        );
        assert!(
            candidates
                .iter()
                .all(|c| c.category != Category::Storyboards)
        );
    }

    #[test]
    fn known_names_skip_parsing_regardless_of_case() {
        let set = set_with(
            &["Map.osu", "AUDIO.MP3", "BG.JPG"],
            &["audio.mp3"],
            &["bg.jpg"],
        );
        assert!(!needs_parsing(&set, &known(&set)));
        assert_eq!(classify(&set)[0].category, Category::Backgrounds);
    }

    #[test]
    fn reference_categories_use_the_same_filename_keys() {
        let references = osu::References {
            videos: BTreeSet::from(["SB/VIDEO.BIN".to_owned()]),
            storyboard: BTreeSet::from(["SB/SPRITE.PNG".to_owned()]),
            hitsounds: BTreeSet::from(["SAMPLE.WAV".to_owned()]),
            ..osu::References::default()
        };
        let index = ReferenceIndex::from(&references);
        for (name, expected) in [
            ("sb/video.bin", Category::Videos),
            ("sb/sprite.png", Category::Storyboards),
            ("sample.wav", Category::Hitsounds),
        ] {
            assert_eq!(categorise(name, &index), Some(expected));
        }
    }

    #[test]
    fn junk_wins_over_every_other_role() {
        let mut references = osu::References::default();
        references.storyboard.insert("Thumbs.db".to_owned());

        assert_eq!(
            categorise("Thumbs.db", &ReferenceIndex::from(&references)),
            Some(Category::Junk)
        );
    }

    #[test]
    fn unclassified_files_are_left_alone() {
        assert_eq!(
            categorise(
                "readme.txt",
                &ReferenceIndex::from(&osu::References::default())
            ),
            None
        );
    }

    #[test]
    fn storyboard_scripts_are_storyboards() {
        assert_eq!(
            categorise(
                "map.osb",
                &ReferenceIndex::from(&osu::References::default())
            ),
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
