//! What a scan found and what a clean would do.

use crate::catalog::Category;
use crate::format::human_bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One file a clean would remove from one beatmap set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Position of the owning beatmap set, used to detach the file during this clean.
    ///
    /// Positions shift when beatmaps are imported or deleted, so this is only meaningful
    /// between a scan and the clean that follows it. [`Candidate::set_id`] names the set
    /// durably.
    pub set_index: usize,
    /// The owning beatmap set's primary key, as 16 bytes.
    ///
    /// A snapshot records this so that restoring reattaches files to the same beatmap even if
    /// the library changed in between.
    #[serde(default, with = "set_id")]
    pub set_id: [u8; 16],
    /// Index of the usage within that set's file list.
    pub file_index: usize,
    /// Name the file has inside the set.
    pub filename: String,
    /// SHA-256 of the contents, which is also the blob's name on disk.
    pub hash: String,
    /// Size of the blob in bytes, or zero when it is missing from disk.
    pub bytes: u64,
    /// Role that put this file in the plan.
    pub category: Category,
    /// Whether removing this usage leaves the blob with no usages at all.
    ///
    /// Only a blob that reaches zero usages can be moved into a snapshot. A file shared with
    /// another beatmap set, a skin, or a replay is detached here but its bytes stay put.
    pub frees_blob: bool,
}

/// Candidates for one category, with totals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    /// Which category these belong to.
    pub category: Category,
    /// References to remove. One file shared by several beatmap sets appears once per set.
    pub candidates: Vec<Candidate>,
    /// Distinct files that would leave the library, which is what `bytes` measures.
    pub files: usize,
    /// Bytes that would actually be freed, counting each file once.
    pub bytes: u64,
    /// Whether the user selected this category.
    pub selected: bool,
}

impl Group {
    /// Number of references in the group.
    ///
    /// This counts one entry per beatmap set that refers to a file, so it is larger than
    /// [`Group::files`] whenever osu!lazer has shared a file between sets. Report
    /// [`Group::files`] to users: it is the number that pairs with [`Group::bytes`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Whether the group found nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}

/// How long each phase of a scan took.
///
/// Scanning a large library is dominated by the filesystem, and which part dominates depends
/// on the machine. Reporting the split makes that visible instead of guessable.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Timings {
    /// Measuring every blob in the store.
    pub measure_ms: u64,
    /// Reading beatmap sets and reference counts out of the database.
    pub database_ms: u64,
    /// Reading and parsing difficulty and storyboard files.
    pub classify_ms: u64,
    /// Beatmap sets whose difficulty files had to be opened.
    ///
    /// The rest were settled from the database alone. A high number here explains a slow scan.
    pub sets_parsed: usize,
}

/// Everything a scan found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// How long each phase took.
    pub timings: Timings,
    /// Groups, ordered as [`Category::ALL`].
    pub groups: Vec<Group>,
    /// Beatmap sets examined.
    pub sets_scanned: usize,
    /// Blobs present in the library.
    pub blobs_total: usize,
    /// Bytes held by the library's blob store.
    pub bytes_total: u64,
}

impl Plan {
    /// Bytes that selected categories would free.
    ///
    /// A blob shared by two selected categories is counted once, which is why this walks the
    /// candidates rather than summing the group totals.
    #[must_use]
    pub fn selected_bytes(&self) -> u64 {
        let mut seen = std::collections::HashSet::new();
        let mut bytes = 0;

        for group in self.groups.iter().filter(|g| g.selected) {
            for candidate in &group.candidates {
                if candidate.frees_blob && seen.insert(candidate.hash.as_str()) {
                    bytes += candidate.bytes;
                }
            }
        }

        bytes
    }

    /// Distinct files selected categories would remove from the library.
    ///
    /// Counts each file once however many beatmap sets refer to it, so this number pairs with
    /// [`Plan::selected_bytes`].
    #[must_use]
    pub fn selected_files(&self) -> usize {
        let mut seen = std::collections::HashSet::new();

        self.groups
            .iter()
            .filter(|g| g.selected)
            .flat_map(|g| g.candidates.iter())
            .filter(|c| c.frees_blob && seen.insert(c.hash.as_str()))
            .count()
    }

    /// References selected categories would detach from beatmap sets.
    #[must_use]
    pub fn selected_references(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| g.selected)
            .map(Group::len)
            .sum()
    }

    /// Candidates from selected categories.
    pub fn selected_candidates(&self) -> impl Iterator<Item = &Candidate> {
        self.groups
            .iter()
            .filter(|g| g.selected)
            .flat_map(|g| g.candidates.iter())
    }

    /// Sets the selection state of one category.
    pub fn select(&mut self, category: Category, selected: bool) {
        if let Some(group) = self.groups.iter_mut().find(|g| g.category == category) {
            group.selected = selected;
        }
    }

    /// A short human summary of what is selected.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} files, {}",
            self.selected_files(),
            human_bytes(self.selected_bytes())
        )
    }
}

/// Serialises a beatmap set's primary key as hex, so manifests stay readable.
mod set_id {
    use serde::{Deserialize as _, Deserializer, Serializer};

    /// Writes the key as a 32-character hex string.
    pub(super) fn serialize<S: Serializer>(id: &[u8; 16], out: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write as _;

        let mut text = String::with_capacity(32);
        for byte in id {
            let _ = write!(text, "{byte:02x}");
        }
        out.serialize_str(&text)
    }

    /// Reads the key back, treating anything malformed as absent.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<[u8; 16], D::Error> {
        let text = String::deserialize(input)?;
        let mut id = [0_u8; 16];

        if text.len() != 32 {
            return Ok(id);
        }

        for (index, slot) in id.iter_mut().enumerate() {
            let Ok(byte) = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16) else {
                return Ok([0_u8; 16]);
            };
            *slot = byte;
        }

        Ok(id)
    }
}

/// How a clean should behave.
#[derive(Debug, Clone)]
pub struct Options {
    /// When set, report what would happen and change nothing.
    ///
    /// The execution path checks this too, so a caller that forgets to branch still cannot
    /// destroy anything.
    pub dry_run: bool,
}

impl Default for Options {
    /// Previewing is the default, so a mistake costs nothing.
    fn default() -> Self {
        Self { dry_run: true }
    }
}

/// Progress reported while scanning.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Measuring the blob store.
    MeasuringFiles {
        /// Blobs measured so far.
        done: usize,
    },
    /// Reading beatmap sets from the database.
    ReadingSets {
        /// Sets read so far.
        done: usize,
        /// Sets in total.
        total: usize,
    },
    /// The scan finished.
    Done,
}

/// Totals per category, for reporting.
#[must_use]
pub fn totals(plan: &Plan) -> BTreeMap<Category, (usize, u64)> {
    plan.groups
        .iter()
        .map(|g| (g.category, (g.files, g.bytes)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(hash: &str, bytes: u64, category: Category, frees: bool) -> Candidate {
        Candidate {
            set_index: 0,
            set_id: [0; 16],
            file_index: 0,
            filename: format!("{hash}.png"),
            hash: hash.to_owned(),
            bytes,
            category,
            frees_blob: frees,
        }
    }

    fn plan_with(groups: Vec<Group>) -> Plan {
        Plan {
            groups,
            ..Plan::default()
        }
    }

    #[test]
    fn shared_blobs_are_counted_once() {
        let plan = plan_with(vec![
            Group {
                category: Category::Videos,
                candidates: vec![candidate("aa", 100, Category::Videos, true)],
                files: 1,
                bytes: 100,
                selected: true,
            },
            Group {
                category: Category::Backgrounds,
                candidates: vec![candidate("aa", 100, Category::Backgrounds, true)],
                files: 1,
                bytes: 100,
                selected: true,
            },
        ]);

        assert_eq!(plan.selected_bytes(), 100);
    }

    #[test]
    fn blobs_still_referenced_free_nothing() {
        let plan = plan_with(vec![Group {
            category: Category::Videos,
            candidates: vec![candidate("bb", 500, Category::Videos, false)],
            files: 0,
            bytes: 0,
            selected: true,
        }]);

        assert_eq!(plan.selected_bytes(), 0);
        assert_eq!(
            plan.selected_files(),
            0,
            "a file that stays put is not removed"
        );
        assert_eq!(
            plan.selected_references(),
            1,
            "but its reference is detached"
        );
    }

    #[test]
    fn unselected_groups_contribute_nothing() {
        let plan = plan_with(vec![Group {
            category: Category::Videos,
            candidates: vec![candidate("cc", 700, Category::Videos, true)],
            files: 1,
            bytes: 700,
            selected: false,
        }]);

        assert_eq!(plan.selected_bytes(), 0);
        assert_eq!(plan.selected_files(), 0);
    }

    #[test]
    fn previewing_is_the_default() {
        assert!(Options::default().dry_run);
    }
}
