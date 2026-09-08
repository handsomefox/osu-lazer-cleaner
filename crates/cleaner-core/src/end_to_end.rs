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

/// Real hashes let these tests also exercise recovery of partially restored 1.0 snapshots.
fn video_fixture(two_owners: bool) -> (Fixture, String) {
    let video = "96b050b919f3fca2fc8b6923537136a197ad13c583beb1438d1a12ccbc999c42".to_owned();
    let mut sets = vec![SetFixture::new([1; 16], "First", &[("intro.mp4", &video)])];
    if two_owners {
        sets.push(SetFixture::new([2; 16], "Second", &[("intro.mp4", &video)]));
    }
    (Fixture::build(&sets, &[(&video, b"video bytes")]), video)
}

fn clean_video(fixture: &Fixture, exclude_second: bool) -> crate::Snapshot {
    let mut plan = fixture.scan(&[Category::Videos]);
    plan.exclude([2; 16], exclude_second);
    crate::run(&fixture.library, &plan, &Options { dry_run: false }, |_| {}).unwrap();
    crate::snapshot::list(&fixture.library).unwrap().remove(0)
}

#[test]
fn a_missing_set_fails_before_restore_touches_any_blob() {
    let (fixture, video) = video_fixture(false);
    let snapshot = clean_video(&fixture, false);
    let realm = cleaner_realm::Realm::open_for_write(&fixture.library.database()).unwrap();
    cleaner_realm::fixture::keep_first_sets(&realm, 0).unwrap();
    drop(realm);

    assert!(crate::restore(&fixture.library, &snapshot, |_| {}).is_err());
    assert!(!fixture.holds(&video));
    assert_eq!(
        std::fs::read(snapshot.dir.join("blobs").join(&video)).unwrap(),
        b"video bytes"
    );
}

#[test]
fn corrupt_restore_destinations_keep_the_snapshot_and_roll_back_rows() {
    for contents in [b"truncated".as_slice(), b"wrong bytes".as_slice()] {
        let (fixture, video) = video_fixture(false);
        let snapshot = clean_video(&fixture, false);
        std::fs::write(fixture.library.blob_path(&video), contents).unwrap();

        assert!(matches!(
            crate::restore(&fixture.library, &snapshot, |_| {}),
            Err(crate::SnapshotError::BlobMismatch { .. })
        ));
        assert_eq!(
            std::fs::read(snapshot.dir.join("blobs").join(&video)).unwrap(),
            b"video bytes"
        );
        let contents = cleaner_realm::Realm::open_read_only(&fixture.library.database())
            .unwrap()
            .read_library()
            .unwrap();
        assert!(!contents.usage_counts.contains_key(&video));

        std::fs::remove_file(fixture.library.blob_path(&video)).unwrap();
        crate::restore(&fixture.library, &snapshot, |_| {}).unwrap();
        assert_eq!(
            std::fs::read(fixture.library.blob_path(&video)).unwrap(),
            b"video bytes"
        );
        assert!(!snapshot.dir.exists());
    }
}

#[test]
fn partial_blob_restoration_retains_every_recovery_link() {
    let (fixture, video) = video_fixture(false);
    let snapshot = clean_video(&fixture, false);
    // Simulate interruption after file publication but before the database transaction commits.
    crate::snapshot::restore_blobs(&fixture.library, &snapshot, &mut |_, _| {}).unwrap();
    assert!(fixture.holds(&video));
    assert!(snapshot.dir.join("blobs").join(&video).is_file());

    // A subsequent game cleanup can remove the unreferenced library link without losing data.
    std::fs::remove_file(fixture.library.blob_path(&video)).unwrap();
    crate::restore(&fixture.library, &snapshot, |_| {}).unwrap();
    assert_eq!(
        std::fs::read(fixture.library.blob_path(&video)).unwrap(),
        b"video bytes"
    );
}

#[test]
fn a_snapshot_needed_by_an_older_clean_cannot_be_deleted() {
    let (fixture, video) = video_fixture(true);
    let older = clean_video(&fixture, true);
    let newer = clean_video(&fixture, false);
    assert!(older.manifest.blobs.is_empty());
    assert!(matches!(
        crate::snapshot::delete(&fixture.library, &newer),
        Err(crate::SnapshotError::SnapshotDependency { .. })
    ));
    assert!(newer.dir.join("blobs").join(&video).is_file());

    crate::restore(&fixture.library, &newer, |_| {}).unwrap();
    crate::restore(&fixture.library, &older, |_| {}).unwrap();
    let contents = cleaner_realm::Realm::open_read_only(&fixture.library.database())
        .unwrap()
        .read_library()
        .unwrap();
    assert_eq!(contents.usage_counts.get(&video), Some(&2));
    assert!(fixture.holds(&video));
}

#[test]
fn deleting_dependent_snapshots_first_allows_reclaiming_the_blob() {
    let (fixture, video) = video_fixture(true);
    let older = clean_video(&fixture, true);
    let newer = clean_video(&fixture, false);
    crate::snapshot::delete(&fixture.library, &older).unwrap();
    crate::snapshot::delete(&fixture.library, &newer).unwrap();
    assert!(!fixture.holds(&video));
    assert!(crate::snapshot::list(&fixture.library).unwrap().is_empty());
}

#[test]
fn an_old_snapshot_cannot_reattach_a_missing_shared_blob() {
    let (fixture, video) = video_fixture(true);
    let older = clean_video(&fixture, true);
    let newer = clean_video(&fixture, false);
    // Reproduce a dependency already deleted by version 1.0 or outside the cleaner.
    std::fs::remove_dir_all(newer.dir).unwrap();
    assert!(crate::restore(&fixture.library, &older, |_| {}).is_err());
    assert!(older.dir.is_dir());
    let contents = cleaner_realm::Realm::open_read_only(&fixture.library.database())
        .unwrap()
        .read_library()
        .unwrap();
    assert!(!contents.usage_counts.contains_key(&video));
}

#[test]
fn a_version_one_restore_can_resume_after_its_blobs_were_moved() {
    let (fixture, video) = video_fixture(false);
    let snapshot = clean_video(&fixture, false);
    std::fs::rename(
        snapshot.dir.join("blobs").join(&video),
        fixture.library.blob_path(&video),
    )
    .unwrap();
    crate::restore(&fixture.library, &snapshot, |_| {}).unwrap();
    assert!(fixture.holds(&video));
    assert!(!snapshot.dir.exists());
    assert_eq!(fixture.scan(&[Category::Videos]).selected_references(), 1);
}
