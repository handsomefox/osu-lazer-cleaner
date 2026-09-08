//! Whole-library tests: scan, clean, restore, and delete, against a real Realm database.
//!
//! Every other test in this crate stubs the database out, because opening one is slow. These
//! do not. They are the only place where the three pieces meet, and a bug that only appears
//! when they do is exactly the kind that costs somebody their beatmaps.
//!
//! The database comes from `cleaner_realm::fixture`, so nothing here needs a personal library.

#![cfg(test)]

use crate::catalog::Category;
use crate::plan::{Options, Plan};
use crate::storage::{DATABASE_FILENAME, Library};
use cleaner_realm::fixture::{SetFixture, synthetic_realm};
use std::collections::HashSet;

/// A library on disk, with its blobs written out.
struct Fixture {
    /// Kept so the directory outlives the library.
    _directory: tempfile::TempDir,
    library: Library,
}

impl Fixture {
    /// Builds a library holding `sets`, writing `contents` for each named hash.
    fn build(sets: &[SetFixture], contents: &[(&str, &[u8])]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        // The realm handle has to close before `Library::open` copies the database.
        drop(synthetic_realm(&root.join(DATABASE_FILENAME), sets));

        let library = Library::open(root).unwrap();
        for (hash, bytes) in contents {
            let path = library.blob_path(hash);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
        }

        Self {
            _directory: directory,
            library,
        }
    }

    /// Scans with the given categories selected.
    fn scan(&self, selected: &[Category]) -> Plan {
        let selected: HashSet<Category> = selected.iter().copied().collect();
        crate::build_plan(&self.library, &selected, |_| {}).unwrap()
    }

    /// Whether the library still holds a blob.
    fn holds(&self, hash: &str) -> bool {
        self.library.blob_path(hash).is_file()
    }
}

/// A 64-character hash made of one repeated hex digit.
fn hash(digit: char) -> String {
    std::iter::repeat_n(digit, 64).collect()
}

#[test]
fn a_clean_moves_a_video_out_and_a_restore_puts_it_back() {
    let video = hash('a');
    let difficulty = hash('b');
    let fixture = Fixture::build(
        &[SetFixture::new(
            [1; 16],
            "Nhelv",
            &[("Map [Insane].osu", &difficulty), ("intro.mp4", &video)],
        )],
        &[(&video, b"video bytes"), (&difficulty, b"osu file v14")],
    );

    let plan = fixture.scan(&[Category::Videos]);
    assert_eq!(plan.selected_totals().files, 1);
    assert_eq!(plan.selected_totals().bytes, 11);

    let outcome = crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {}).unwrap();
    assert_eq!(
        (outcome.detached, outcome.stashed, outcome.bytes),
        (1, 1, 11)
    );

    assert!(!fixture.holds(&video), "the video should have left");
    assert!(fixture.holds(&difficulty), "the difficulty must stay");

    // The database no longer names it, so a rescan finds nothing to do.
    assert_eq!(fixture.scan(&[Category::Videos]).selected_totals().files, 0);

    let snapshot = crate::snapshot::list(&fixture.library).unwrap().remove(0);
    crate::restore(&fixture.library, &snapshot, |_| {}).unwrap();

    assert_eq!(
        std::fs::read(fixture.library.blob_path(&video)).unwrap(),
        b"video bytes"
    );
    assert!(
        crate::snapshot::list(&fixture.library).unwrap().is_empty(),
        "a restored snapshot holds nothing and should be gone"
    );

    // Restoring reattached the usage, so the video is a candidate again.
    assert_eq!(fixture.scan(&[Category::Videos]).selected_totals().files, 1);
}

#[test]
fn deleting_a_snapshot_is_what_finally_reclaims_the_space() {
    let video = hash('a');
    let fixture = Fixture::build(
        &[SetFixture::new(
            [1; 16],
            "Nhelv",
            &[("Map.osu", &hash('b')), ("intro.mp4", &video)],
        )],
        &[(&video, b"video bytes"), (&hash('b'), b"osu file v14")],
    );

    let plan = fixture.scan(&[Category::Videos]);
    crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {}).unwrap();

    let snapshot = crate::snapshot::list(&fixture.library).unwrap().remove(0);
    assert_eq!(snapshot.manifest.bytes(), 11);
    crate::snapshot::delete(&fixture.library, &snapshot).unwrap();

    assert!(crate::snapshot::list(&fixture.library).unwrap().is_empty());
    assert!(!fixture.holds(&video), "the bytes are gone for good now");
}

#[test]
fn a_video_two_beatmap_sets_share_survives_cleaning_one_of_them() {
    let video = hash('a');
    let fixture = Fixture::build(
        &[
            SetFixture::new(
                [1; 16],
                "First",
                &[("Map.osu", &hash('b')), ("intro.mp4", &video)],
            ),
            SetFixture::new(
                [2; 16],
                "Second",
                &[("Map.osu", &hash('c')), ("intro.mp4", &video)],
            ),
        ],
        &[
            (&video, b"video bytes"),
            (&hash('b'), b"osu file v14"),
            (&hash('c'), b"osu file v14"),
        ],
    );

    let mut plan = fixture.scan(&[Category::Videos]);
    assert_eq!(
        plan.selected_totals().references,
        2,
        "both sets refer to it"
    );
    assert_eq!(
        plan.selected_totals().files,
        1,
        "one file, counted once, leaves when both go"
    );

    plan.exclude([2; 16], true);
    assert_eq!(
        plan.selected_totals(),
        crate::Totals {
            files: 0,
            bytes: 0,
            references: 1,
        },
        "holding one set back keeps the file"
    );

    let outcome = crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {}).unwrap();

    assert_eq!((outcome.detached, outcome.stashed), (1, 0));
    assert!(
        fixture.holds(&video),
        "the second set still needs the video"
    );

    // The first set gave up its reference, so only the second one is left to give up.
    let plan = fixture.scan(&[Category::Videos]);
    assert_eq!(plan.selected_totals().references, 1);
    assert_eq!(plan.selected_totals().files, 1);
}

#[test]
fn cleaning_both_owners_of_a_shared_video_frees_it() {
    let video = hash('a');
    let fixture = Fixture::build(
        &[
            SetFixture::new(
                [1; 16],
                "First",
                &[("Map.osu", &hash('b')), ("intro.mp4", &video)],
            ),
            SetFixture::new(
                [2; 16],
                "Second",
                &[("Map.osu", &hash('c')), ("intro.mp4", &video)],
            ),
        ],
        &[
            (&video, b"video bytes"),
            (&hash('b'), b"osu file v14"),
            (&hash('c'), b"osu file v14"),
        ],
    );

    let plan = fixture.scan(&[Category::Videos]);
    let outcome = crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {}).unwrap();

    assert_eq!(
        (outcome.detached, outcome.stashed, outcome.bytes),
        (2, 1, 11)
    );
    assert!(!fixture.holds(&video));

    // Both usages come back, and so does the file.
    let snapshot = crate::snapshot::list(&fixture.library).unwrap().remove(0);
    crate::restore(&fixture.library, &snapshot, |_| {}).unwrap();
    assert!(fixture.holds(&video));
    assert_eq!(
        fixture
            .scan(&[Category::Videos])
            .selected_totals()
            .references,
        2
    );
}

#[test]
fn the_audio_track_and_difficulties_are_never_candidates() {
    let audio = hash('a');
    let difficulty = hash('b');
    let background = hash('c');

    let mut set = SetFixture::new(
        [1; 16],
        "Nhelv",
        &[
            ("Map [Insane].osu", &difficulty),
            ("audio.mp3", &audio),
            ("bg.jpg", &background),
        ],
    );
    set.audio = "audio.mp3".to_owned();
    set.background = "bg.jpg".to_owned();
    set.artist = "Sotarks".to_owned();

    let fixture = Fixture::build(
        &[set],
        &[
            (&audio, b"mp3"),
            (&difficulty, b"osu file v14"),
            (&background, b"jpeg"),
        ],
    );

    let plan = fixture.scan(Category::ALL);
    let names: Vec<_> = plan
        .selected_candidates()
        .map(|candidate| candidate.filename.as_str())
        .collect();

    assert_eq!(
        names,
        vec!["bg.jpg"],
        "only the background may go; the audio track and the difficulty are protected"
    );

    // The metadata the browse list shows comes from the same read.
    assert_eq!(
        plan.sets_in(Category::Backgrounds)[0].title,
        "Sotarks - Nhelv"
    );
}

#[test]
fn a_plan_that_no_longer_matches_the_library_is_refused() {
    let video = hash('a');
    let fixture = Fixture::build(
        &[SetFixture::new(
            [1; 16],
            "Nhelv",
            &[("Map.osu", &hash('b')), ("intro.mp4", &video)],
        )],
        &[(&video, b"video bytes"), (&hash('b'), b"osu file v14")],
    );

    let mut plan = fixture.scan(&[Category::Videos]);

    // Something else imported a beatmap between the scan and the clean, so the recorded
    // position now names a different set.
    for group in &mut plan.groups {
        for candidate in &mut group.candidates {
            candidate.set_id = [9; 16];
        }
    }

    let error = crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {})
        .expect_err("a stale plan must not run");
    assert!(matches!(error, crate::SnapshotError::StalePlan));

    assert!(fixture.holds(&video), "nothing may have been touched");
    assert!(
        crate::snapshot::list(&fixture.library).unwrap().is_empty(),
        "and no snapshot may be left behind"
    );
    assert_eq!(
        std::fs::read_dir(fixture.library.snapshots_dir()).map_or(0, Iterator::count),
        0,
        "including a half-built one"
    );
}

#[test]
fn unreferenced_blobs_are_swept_without_touching_the_database() {
    let orphan = hash('f');
    let difficulty = hash('b');
    let fixture = Fixture::build(
        &[SetFixture::new(
            [1; 16],
            "Nhelv",
            &[("Map.osu", &difficulty)],
        )],
        &[(&orphan, b"left over"), (&difficulty, b"osu file v14")],
    );

    let plan = fixture.scan(&[Category::Unreferenced]);
    assert_eq!(plan.selected_totals().files, 1);

    let outcome = crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {}).unwrap();

    assert_eq!(
        (outcome.detached, outcome.stashed),
        (0, 1),
        "nothing pointed at it, so there was no usage to detach"
    );
    assert!(!fixture.holds(&orphan));
    assert!(fixture.holds(&difficulty));

    // Restoring one puts the bytes back without inventing a database row for it.
    let snapshot = crate::snapshot::list(&fixture.library).unwrap().remove(0);
    crate::restore(&fixture.library, &snapshot, |_| {}).unwrap();
    assert!(fixture.holds(&orphan));
    assert_eq!(
        fixture
            .scan(&[Category::Unreferenced])
            .selected_totals()
            .files,
        1
    );
}

#[test]
fn previewing_a_clean_changes_nothing() {
    let video = hash('a');
    let fixture = Fixture::build(
        &[SetFixture::new(
            [1; 16],
            "Nhelv",
            &[("Map.osu", &hash('b')), ("intro.mp4", &video)],
        )],
        &[(&video, b"video bytes"), (&hash('b'), b"osu file v14")],
    );

    let plan = fixture.scan(&[Category::Videos]);
    let outcome = crate::run(&fixture.library, &plan, &Options::default(), |_| {}).unwrap();

    assert!(outcome.dry_run);
    assert_eq!(outcome.bytes, 11, "and still reports what it would free");
    assert!(fixture.holds(&video));
    assert!(!fixture.library.snapshots_dir().exists());
}
