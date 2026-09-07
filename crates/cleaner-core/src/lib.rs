//! Portable logic for removing unwanted beatmap content from an osu!lazer library.
//!
//! Nothing here depends on a platform or a user interface, so it can be tested on any machine.
//!
//! The safety rules all come from osu!lazer's own source, and each is stated where it is
//! enforced. Three matter most:
//!
//! - A blob may only be removed once no usage anywhere points at it. osu!lazer applies the same
//!   rule in `RealmFileStore.Cleanup` as the query `Usages.@count = 0`. Beatmap sets, skins,
//!   and replays all own usages, so counting only beatmap sets would delete files other things
//!   still need.
//! - Difficulty files and the audio track are never removed. A difficulty's hash identifies it
//!   online, so changing or deleting one breaks score submission.
//! - Removing any other file leaves `BeatmapSetInfo.Hash` alone. osu!lazer does the same in
//!   `BeatmapManager.DeleteVideos`, which detaches a video without recomputing the set hash.

pub mod catalog;
pub mod error;
pub mod execute;
pub mod format;
pub mod osu;
pub mod plan;
pub mod report;
pub mod safety;
pub mod scan;
pub mod skin;
pub mod snapshot;
pub mod storage;

pub use catalog::Category;
pub use error::{ScanError, SnapshotError, StorageError};
pub use execute::{Outcome, restore, run};
pub use format::human_bytes;
pub use plan::{Candidate, Group, Options, Plan, Progress, Timings};
pub use report::Report;
pub use safety::is_safe_path;
pub use scan::build_plan;
pub use snapshot::{Manifest, Snapshot};
pub use storage::Library;
