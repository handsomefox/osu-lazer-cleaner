//! What a scan found and what a clean would do.

use crate::catalog::Category;
use crate::format::human_bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One file a clean would remove from one beatmap set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Index of the owning beatmap set in the database.
    pub set_index: usize,
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
    /// Files in this category.
    pub candidates: Vec<Candidate>,
    /// Bytes that would actually be freed, counting each blob once.
    pub bytes: u64,
    /// Whether the user selected this category.
    pub selected: bool,
}

impl Group {
    /// Number of files in the group.
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

/// Everything a scan found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
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

    /// Number of file usages selected categories would detach.
    #[must_use]
    pub fn selected_files(&self) -> usize {
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
        .map(|g| (g.category, (g.len(), g.bytes)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(hash: &str, bytes: u64, category: Category, frees: bool) -> Candidate {
        Candidate {
            set_index: 0,
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
                bytes: 100,
                selected: true,
            },
            Group {
                category: Category::Backgrounds,
                candidates: vec![candidate("aa", 100, Category::Backgrounds, true)],
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
            bytes: 0,
            selected: true,
        }]);

        assert_eq!(plan.selected_bytes(), 0);
        assert_eq!(plan.selected_files(), 1);
    }

    #[test]
    fn unselected_groups_contribute_nothing() {
        let plan = plan_with(vec![Group {
            category: Category::Videos,
            candidates: vec![candidate("cc", 700, Category::Videos, true)],
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
