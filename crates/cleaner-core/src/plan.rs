//! What a scan found and what a clean would do.

use crate::catalog::Category;
use crate::format::human_bytes;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A beatmap set's `Guid` primary key, as 16 bytes.
pub type SetId = [u8; 16];

/// Set identity used by candidates that belong to no beatmap set.
///
/// Unreferenced blobs are owned by nothing, so they carry this instead of a real key and are
/// never excluded by the browse list. A beatmap set the database gives no key reads as this
/// too, which costs it a label and the ability to be excluded, and nothing else.
pub const NO_SET: SetId = [0; 16];

/// Position that means a candidate belongs to no beatmap set.
pub const NO_SET_INDEX: usize = usize::MAX;

/// One file a clean would remove from one beatmap set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Position of the owning beatmap set, used to detach the file during this clean.
    ///
    /// Positions shift when beatmaps are imported or deleted, so this is only meaningful
    /// between a scan and the clean that follows it. [`Candidate::set_id`] names the set
    /// durably. [`NO_SET_INDEX`] means the file belongs to no set.
    pub set_index: usize,
    /// The owning beatmap set's primary key, or [`NO_SET`] when it belongs to no set.
    ///
    /// A snapshot records this so that restoring reattaches files to the same beatmap even if
    /// the library changed in between.
    #[serde(with = "set_id")]
    pub set_id: SetId,
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
    /// Usages pointing at this blob at scan time, counting every owner.
    ///
    /// Beatmap sets, skins, replays, and cached online assets all own usages. The blob only
    /// leaves the library once a clean detaches every one of them, which is the rule
    /// `RealmFileStore.Cleanup` writes as `Usages.@count = 0`.
    pub usage_count: u32,
}

impl Candidate {
    /// Whether something other than this usage also points at the blob.
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        self.usage_count > 1
    }
}

/// A fast, non-cryptographic hasher for content hashes.
///
/// Recounting a large library hashes a quarter of a million 64-character keys, several times
/// per click. The standard hasher is built to resist collision attacks on untrusted keys, which
/// these are not: the keys come from a hash the library computed itself. Mixing eight bytes at
/// a time instead costs a rotate and a multiply per word.
///
/// This must stay a real hash of every byte. An earlier version took the first eight characters
/// on the grounds that a SHA-256 is already uniform, which was true of a real library and
/// catastrophic anywhere the prefixes repeat: a quarter of a million equal keys turned one
/// selection change into six minutes.
#[derive(Default, Clone, Copy)]
pub(crate) struct FastHash(u64);

/// Odd 64-bit constant from `rustc_hash`, chosen to spread every input bit.
pub(crate) const HASH_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FastHash {
    /// Folds one word into the running hash.
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(HASH_SEED);
    }
}

impl std::hash::Hasher for FastHash {
    fn write(&mut self, bytes: &[u8]) {
        let (words, tail) = bytes.as_chunks::<8>();
        for word in words {
            self.mix(u64::from_le_bytes(*word));
        }

        if !tail.is_empty() {
            let mut last = [0_u8; 8];
            last[..tail.len()].copy_from_slice(tail);
            self.mix(u64::from_le_bytes(last));
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl std::hash::BuildHasher for FastHash {
    type Hasher = Self;

    fn build_hasher(&self) -> Self {
        Self::default()
    }
}

/// What a set of candidates would free: distinct files, their bytes, and references detached.
///
/// A blob leaves the library only when the clean detaches its last usage. Counting each
/// reference on its own would keep a file that two selected beatmap sets share, and counting
/// none of them would delete a file a skin still needs.
pub(crate) fn freed_blobs<'a>(candidates: impl Iterator<Item = &'a Candidate>) -> Totals {
    let mut counts: HashMap<&str, (u32, u32, u64), FastHash> = HashMap::default();
    let mut references = 0;

    for candidate in candidates {
        references += 1;
        let entry = counts.entry(candidate.hash.as_str()).or_insert((
            0,
            candidate.usage_count,
            candidate.bytes,
        ));
        entry.0 += 1;
        // One scan fills in the same count for every reference to a hash. Taking the largest
        // is the conservative reading if that ever stops being true.
        entry.1 = entry.1.max(candidate.usage_count);
    }

    let mut totals = Totals {
        references,
        ..Totals::default()
    };
    for (detaching, owners, bytes) in counts.into_values() {
        if detaching >= owners {
            totals.files += 1;
            totals.bytes += bytes;
        }
    }
    totals
}

/// The distinct blobs a group of candidates would free, by hash.
///
/// Only the report needs the names; everything else needs the totals, which [`freed_blobs`]
/// produces without building this.
pub(crate) fn freed_hashes<'a>(
    candidates: impl Iterator<Item = &'a Candidate>,
) -> HashSet<&'a str, FastHash> {
    let mut counts: HashMap<&str, (u32, u32), FastHash> = HashMap::default();

    for candidate in candidates {
        let entry = counts
            .entry(candidate.hash.as_str())
            .or_insert((0, candidate.usage_count));
        entry.0 += 1;
        entry.1 = entry.1.max(candidate.usage_count);
    }

    counts
        .into_iter()
        .filter(|(_, (detaching, owners))| detaching >= owners)
        .map(|(hash, _)| hash)
        .collect()
}

/// Candidates for one category.
#[derive(Debug, Clone)]
pub struct Group {
    /// Which category these belong to.
    pub category: Category,
    /// References to remove. One file shared by several beatmap sets appears once per set.
    pub candidates: Vec<Candidate>,
    /// Whether the user selected this category.
    pub selected: bool,
}

impl Group {
    /// Number of references in the group.
    ///
    /// This counts one entry per beatmap set that refers to a file, so it is larger than the
    /// file count whenever osu!lazer has shared a file between sets.
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

impl Timings {
    /// Total scan time in milliseconds.
    #[must_use]
    pub const fn total_ms(&self) -> u64 {
        self.measure_ms + self.database_ms + self.classify_ms
    }
}

/// Files and bytes one category would free, and how many references it would detach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Distinct files that would leave the library.
    pub files: usize,
    /// Bytes those files hold, which is what deleting the snapshot reclaims.
    pub bytes: u64,
    /// Usages that would be detached from beatmap sets.
    pub references: usize,
}

/// What a scan found, and what the current selection would remove.
///
/// Deliberately not serialisable. A snapshot records the [`Candidate`] list it detached and
/// nothing else, so a plan never has to survive the process that made it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// How long each phase took.
    pub timings: Timings,
    /// Groups, ordered as [`Category::ALL`].
    pub groups: Vec<Group>,
    /// Label for each beatmap set that owns at least one candidate.
    ///
    /// Only the interface reads these. Identity, not the label, names a set anywhere it
    /// matters.
    pub set_titles: BTreeMap<SetId, String>,
    /// Beatmap sets the user took out of the clean.
    pub excluded: HashSet<SetId>,
    /// Beatmap sets examined.
    pub sets_scanned: usize,
    /// Blobs present in the library.
    pub blobs_total: usize,
    /// Bytes held by the library's blob store.
    pub bytes_total: u64,
}

impl Plan {
    /// Candidates a clean would act on: selected categories, minus excluded beatmap sets.
    pub fn selected_candidates(&self) -> impl Iterator<Item = &Candidate> {
        self.groups
            .iter()
            .filter(|group| group.selected)
            .flat_map(|group| self.included(group))
    }

    /// Candidates in one group whose beatmap set the user kept.
    fn included<'a>(&'a self, group: &'a Group) -> impl Iterator<Item = &'a Candidate> {
        group
            .candidates
            .iter()
            .filter(|candidate| !self.excluded.contains(&candidate.set_id))
    }

    /// Bytes that selected categories would free.
    ///
    /// A blob shared by two selected categories is counted once, which is why this walks the
    /// candidates rather than summing the group totals.
    #[must_use]
    pub fn selected_bytes(&self) -> u64 {
        self.selected_totals().bytes
    }

    /// Distinct files selected categories would remove from the library.
    #[must_use]
    pub fn selected_files(&self) -> usize {
        self.selected_totals().files
    }

    /// References selected categories would detach from beatmap sets.
    #[must_use]
    pub fn selected_references(&self) -> usize {
        self.selected_totals().references
    }

    /// Everything the current selection would remove, counting each file once.
    #[must_use]
    pub fn selected_totals(&self) -> Totals {
        freed_blobs(self.selected_candidates())
    }

    /// What one category would free on its own, whether or not it is selected.
    #[must_use]
    pub fn category_totals(&self, category: Category) -> Totals {
        self.groups
            .iter()
            .find(|group| group.category == category)
            .map_or_else(Totals::default, |group| self.totals_of(group))
    }

    /// What every category would free on its own, in [`Category::ALL`] order.
    #[must_use]
    pub fn totals_by_category(&self) -> Vec<(Category, Totals)> {
        self.groups
            .iter()
            .map(|group| (group.category, self.totals_of(group)))
            .collect()
    }

    /// Files and bytes one group would free, ignoring the other groups.
    fn totals_of(&self, group: &Group) -> Totals {
        freed_blobs(self.included(group))
    }

    /// Sets the selection state of one category.
    pub fn select(&mut self, category: Category, selected: bool) {
        if let Some(group) = self.groups.iter_mut().find(|g| g.category == category) {
            group.selected = selected;
        }
    }

    /// Takes one beatmap set out of the clean, or puts it back.
    ///
    /// Excluding a set has no effect on unreferenced blobs, which belong to no set.
    pub fn exclude(&mut self, set: SetId, excluded: bool) {
        if set == NO_SET {
            return;
        }
        if excluded {
            self.excluded.insert(set);
        } else {
            self.excluded.remove(&set);
        }
    }

    /// Every beatmap set contributing to one category, largest first.
    ///
    /// Sizes count each file once per set. A file two sets share is counted in both, because
    /// either set on its own is the reason it is listed.
    #[must_use]
    pub fn sets_in(&self, category: Category) -> Vec<SetEntry> {
        let Some(group) = self.groups.iter().find(|g| g.category == category) else {
            return Vec::new();
        };

        let mut by_set: HashMap<SetId, Vec<&Candidate>> = HashMap::new();
        for candidate in &group.candidates {
            by_set.entry(candidate.set_id).or_default().push(candidate);
        }

        let mut entries: Vec<SetEntry> = by_set
            .into_iter()
            .map(|(set, candidates)| {
                let mut seen: HashSet<&str, FastHash> = HashSet::default();
                let mut bytes = 0;
                let mut files = 0;
                for candidate in &candidates {
                    if seen.insert(candidate.hash.as_str()) {
                        bytes += candidate.bytes;
                        files += 1;
                    }
                }
                SetEntry {
                    set,
                    title: self.label(set),
                    files,
                    bytes,
                    excluded: self.excluded.contains(&set),
                }
            })
            .collect();

        entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.title.cmp(&b.title)));
        entries
    }

    /// How a beatmap set is named in the interface.
    fn label(&self, set: SetId) -> String {
        if set == NO_SET {
            return "Files no beatmap set owns".to_owned();
        }
        match self.set_titles.get(&set) {
            Some(title) if !title.is_empty() => title.clone(),
            _ => "Untitled beatmap set".to_owned(),
        }
    }

    /// A short human summary of what is selected.
    #[must_use]
    pub fn summary(&self) -> String {
        let totals = self.selected_totals();
        format!("{} files, {}", totals.files, human_bytes(totals.bytes))
    }
}

/// One beatmap set's share of a category, for the browse list.
#[derive(Debug, Clone)]
pub struct SetEntry {
    /// The set's primary key, or [`NO_SET`] for files no set owns.
    pub set: SetId,
    /// Artist and title, or a placeholder when the database names neither.
    pub title: String,
    /// Distinct files this set contributes to the category.
    pub files: usize,
    /// Bytes those files hold.
    pub bytes: u64,
    /// Whether the user took this set out of the clean.
    pub excluded: bool,
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

    /// Reads the key back, rejecting malformed identities.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<[u8; 16], D::Error> {
        let text = String::deserialize(input)?;
        let mut id = [0_u8; 16];

        if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom(
                "set_id must contain 32 hex digits",
            ));
        }

        for (index, slot) in id.iter_mut().enumerate() {
            let Ok(byte) = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16) else {
                return Err(serde::de::Error::custom("invalid set_id"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(hash: &str, bytes: u64, category: Category, usages: u32) -> Candidate {
        Candidate {
            set_index: 0,
            set_id: NO_SET,
            file_index: 0,
            filename: format!("{hash}.png"),
            hash: hash.to_owned(),
            bytes,
            category,
            usage_count: usages,
        }
    }

    fn group(category: Category, candidates: Vec<Candidate>, selected: bool) -> Group {
        Group {
            category,
            candidates,
            selected,
        }
    }

    fn plan_with(groups: Vec<Group>) -> Plan {
        Plan {
            groups,
            ..Plan::default()
        }
    }

    #[test]
    fn the_hasher_spreads_keys_that_share_a_prefix() {
        use std::hash::{BuildHasher as _, Hasher as _};

        let of = |key: &str| {
            let mut hasher = FastHash::default().build_hasher();
            hasher.write(key.as_bytes());
            hasher.finish()
        };

        // Every test in this crate names blobs by a repeated digit, and an earlier hasher
        // collapsed all of them onto one bucket.
        let mut seen = HashSet::new();
        for digit in "0123456789abcdef".chars() {
            let key: String = std::iter::repeat_n(digit, 64).collect();
            assert!(seen.insert(of(&key)), "collision on {digit}");
        }

        // Keys differing only in their last character must land apart too.
        assert_ne!(of(&"a".repeat(64)), of(&format!("{}b", "a".repeat(63))));
        assert_eq!(of("stable"), of("stable"), "hashing must be deterministic");
    }

    #[test]
    fn shared_blobs_are_counted_once() {
        let plan = plan_with(vec![
            group(
                Category::Videos,
                vec![candidate("aa", 100, Category::Videos, 1)],
                true,
            ),
            group(
                Category::Backgrounds,
                vec![candidate("aa", 100, Category::Backgrounds, 1)],
                true,
            ),
        ]);

        assert_eq!(plan.selected_bytes(), 100);
    }

    #[test]
    fn blobs_still_referenced_free_nothing() {
        let plan = plan_with(vec![group(
            Category::Videos,
            vec![candidate("bb", 500, Category::Videos, 2)],
            true,
        )]);

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
    fn shared_blobs_require_every_owner_to_be_selected() {
        let mut first = candidate("shared", 500, Category::Videos, 2);
        first.set_id = [1; 16];
        let mut second = first.clone();
        second.category = Category::Backgrounds;
        second.set_index = 1;
        second.set_id = [2; 16];

        let mut plan = plan_with(vec![
            group(Category::Videos, vec![first], true),
            group(Category::Backgrounds, vec![second], false),
        ]);

        assert_eq!(plan.selected_bytes(), 0, "one owner is not enough");

        plan.select(Category::Backgrounds, true);
        assert_eq!(plan.selected_totals().files, 1);
        assert_eq!(plan.selected_totals().bytes, 500);

        plan.select(Category::Videos, false);
        assert_eq!(plan.selected_bytes(), 0);

        plan.select(Category::Videos, true);
        for group in &mut plan.groups {
            // A skin owns the third reference, so neither beatmap set frees the file.
            group.candidates[0].usage_count = 3;
        }
        assert_eq!(
            plan.selected_totals(),
            Totals {
                files: 0,
                bytes: 0,
                references: 2,
            },
            "both usages are detached but the skin keeps the bytes"
        );
    }

    #[test]
    fn excluding_a_set_takes_its_files_out_of_the_clean() {
        let mut kept = candidate("aa", 100, Category::Videos, 1);
        kept.set_id = [1; 16];
        let mut dropped = candidate("bb", 700, Category::Videos, 1);
        dropped.set_id = [2; 16];
        dropped.set_index = 1;

        let mut plan = plan_with(vec![group(Category::Videos, vec![kept, dropped], true)]);
        assert_eq!(plan.selected_bytes(), 800);

        plan.exclude([2; 16], true);
        assert_eq!(plan.selected_bytes(), 100);
        assert_eq!(plan.selected_references(), 1);
        assert_eq!(plan.category_totals(Category::Videos).bytes, 100);

        plan.exclude([2; 16], false);
        assert_eq!(plan.selected_bytes(), 800);
    }

    #[test]
    fn excluding_one_owner_keeps_a_shared_file() {
        let mut first = candidate("shared", 500, Category::Videos, 2);
        first.set_id = [1; 16];
        let mut second = first.clone();
        second.set_id = [2; 16];
        second.set_index = 1;

        let mut plan = plan_with(vec![group(Category::Videos, vec![first, second], true)]);
        assert_eq!(plan.selected_bytes(), 500);

        plan.exclude([2; 16], true);
        assert_eq!(
            plan.selected_bytes(),
            0,
            "the kept set still refers to the file"
        );
    }

    #[test]
    fn files_no_set_owns_cannot_be_excluded() {
        let mut plan = plan_with(vec![group(
            Category::Unreferenced,
            vec![candidate("aa", 42, Category::Unreferenced, 0)],
            true,
        )]);

        plan.exclude(NO_SET, true);
        assert!(plan.excluded.is_empty());
        assert_eq!(plan.selected_bytes(), 42);
    }

    #[test]
    fn the_browse_list_orders_sets_by_size() {
        let mut small = candidate("aa", 100, Category::Videos, 1);
        small.set_id = [1; 16];
        let mut large = candidate("bb", 700, Category::Videos, 1);
        large.set_id = [2; 16];

        let mut plan = plan_with(vec![group(Category::Videos, vec![small, large], true)]);
        plan.set_titles.insert([1; 16], "Artist - Small".to_owned());
        plan.set_titles.insert([2; 16], "Artist - Large".to_owned());
        plan.exclude([1; 16], true);

        let entries = plan.sets_in(Category::Videos);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "Artist - Large");
        assert_eq!(entries[0].bytes, 700);
        assert!(!entries[0].excluded);
        assert_eq!(entries[1].title, "Artist - Small");
        assert!(entries[1].excluded);
    }

    #[test]
    fn a_set_the_database_does_not_name_still_has_a_label() {
        let mut orphan = candidate("aa", 1, Category::Unreferenced, 0);
        orphan.set_index = NO_SET_INDEX;
        let mut untitled = candidate("bb", 1, Category::Videos, 1);
        untitled.set_id = [9; 16];

        let plan = plan_with(vec![
            group(Category::Unreferenced, vec![orphan], true),
            group(Category::Videos, vec![untitled], true),
        ]);

        assert_eq!(
            plan.sets_in(Category::Videos)[0].title,
            "Untitled beatmap set"
        );
        assert_eq!(
            plan.sets_in(Category::Unreferenced)[0].title,
            "Files no beatmap set owns"
        );
    }

    #[test]
    fn unselected_groups_contribute_nothing() {
        let plan = plan_with(vec![group(
            Category::Videos,
            vec![candidate("cc", 700, Category::Videos, 1)],
            false,
        )]);

        assert_eq!(plan.selected_bytes(), 0);
        assert_eq!(plan.selected_files(), 0);
        assert_eq!(
            plan.category_totals(Category::Videos).bytes,
            700,
            "the row still reports what ticking it would free"
        );
    }

    #[test]
    fn previewing_is_the_default() {
        assert!(Options::default().dry_run);
    }

    #[test]
    fn a_manifest_without_a_set_id_is_rejected() {
        let mut value = serde_json::to_value(candidate("aa", 1, Category::Junk, 1)).unwrap();
        for malformed in [
            "x".repeat(32),
            format!("a{}a", "é".repeat(15)),
            String::new(),
        ] {
            value["set_id"] = serde_json::Value::String(malformed);
            assert!(serde_json::from_value::<Candidate>(value.clone()).is_err());
        }

        value.as_object_mut().unwrap().remove("set_id");
        assert!(
            serde_json::from_value::<Candidate>(value.clone()).is_err(),
            "a manifest that names no beatmap set cannot be restored"
        );
    }

    #[test]
    fn a_manifest_without_a_usage_count_is_rejected() {
        let mut value = serde_json::to_value(candidate("aa", 1, Category::Junk, 1)).unwrap();
        value.as_object_mut().unwrap().remove("usage_count");
        assert!(serde_json::from_value::<Candidate>(value).is_err());
    }
}
