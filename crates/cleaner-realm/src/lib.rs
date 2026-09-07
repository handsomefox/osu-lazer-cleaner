//! Safe bindings to realm-core's C API, scoped to what an osu!lazer library cleaner needs.
//!
//! The schema is discovered from the database file rather than declared in Rust. osu!lazer
//! bumps its schema version regularly, and a tool that hardcodes the model classes breaks on
//! every bump. Discovery via [`Realm::classes`] keeps us working: the sample library is at
//! schema 51, where `RealmOnlineAsset` does not yet exist, while current lazer is at 52.

pub mod sys;
mod value;

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::path::Path;

/// Every class that owns a list of `RealmNamedFileUsage`, with the property that holds it.
///
/// `RealmOnlineAsset` is deliberately absent: its `File` is a single embedded property rather
/// than a list, and the class only exists from schema 52. Cached online assets are handled
/// separately.
const OWNERS_WITH_FILE_LISTS: &[(&str, &str)] = &[
    ("BeatmapSet", "Files"),
    ("Score", "Files"),
    ("Skin", "Files"),
];

/// A beatmap set and the files it owns.
#[derive(Debug, Clone)]
pub struct BeatmapSet {
    /// Position in the class's natural order, used to address the set for writes.
    pub index: usize,
    /// Files the set owns, in list order.
    pub files: Vec<NamedFile>,
}

/// One entry in an owner's file list.
#[derive(Debug, Clone)]
pub struct NamedFile {
    /// Position within the owner's list.
    pub index: usize,
    /// Name the file had inside the beatmap archive, with forward slashes.
    pub filename: String,
    /// SHA-256 of the file's contents, which is also its name under `files/`.
    pub hash: String,
}

/// A usage to detach from a beatmap set.
#[derive(Debug, Clone, Copy)]
pub struct Removal {
    /// Index of the owning set, as reported by [`Realm::beatmap_sets`].
    pub set_index: usize,
    /// Index of the file within that set's list.
    pub file_index: usize,
}

/// Errors surfaced by realm-core or by our own preconditions.
#[derive(Debug, thiserror::Error)]
pub enum RealmError {
    /// realm-core reported a failure; the message is its own.
    #[error("realm error {code}: {message}")]
    Core {
        /// realm-core's `realm_errno_e` value.
        code: i32,
        /// Human-readable message from realm-core.
        message: String,
    },

    /// A path could not be represented as a C string.
    #[error("path is not valid for realm: {0}")]
    InvalidPath(String),
}

/// Fetches the last error realm-core recorded on this thread.
pub(crate) fn last_error() -> RealmError {
    let mut err = sys::realm_error_t::default();

    // SAFETY: `realm_get_last_error` fills the caller-provided struct and borrows nothing.
    let had_error = unsafe { sys::realm_get_last_error(&raw mut err) };

    if !had_error {
        return RealmError::Core {
            code: 0,
            message: "realm reported failure without an error".to_owned(),
        };
    }

    let message = if err.message.is_null() {
        String::new()
    } else {
        // SAFETY: realm-core guarantees `message` is a NUL-terminated string valid until the
        // next API call on this thread, and we copy it before returning.
        unsafe { CStr::from_ptr(err.message) }
            .to_string_lossy()
            .into_owned()
    };

    RealmError::Core {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "realm_errno_e is a C enum that fits in i32"
        )]
        code: err.error as i32,
        message,
    }
}

/// Reports whether the caller is on the scheduler's thread.
///
/// This tool does all realm work on one thread at a time, so the answer is always yes.
unsafe extern "C" fn scheduler_is_on_thread(_userdata: *mut std::ffi::c_void) -> bool {
    true
}

/// Reports whether two scheduler handles refer to the same scheduler.
///
/// Every handle we hand out is the same stateless no-op scheduler.
unsafe extern "C" fn scheduler_is_same_as(
    _a: *const std::ffi::c_void,
    _b: *const std::ffi::c_void,
) -> bool {
    true
}

/// Reports whether change notifications can be delivered. They cannot, by design.
unsafe extern "C" fn scheduler_can_deliver_notifications(_userdata: *mut std::ffi::c_void) -> bool {
    false
}

/// Creates a scheduler that runs no event loop and delivers no notifications.
///
/// realm-core ships no built-in scheduler for generic Linux, and
/// `realm_scheduler_get_frozen` cannot be used: in realm-core 20.1.2 it is stubbed to
/// `return nullptr` (see the `FIXME` in `c_api/scheduler.cpp:158-166`) while
/// `realm_config_set_scheduler` dereferences its argument unconditionally, so passing it
/// segfaults. Building a no-op scheduler through the supported `realm_scheduler_new` entry
/// point avoids that and costs us nothing, because this tool is a batch scanner that never
/// subscribes to changes.
fn make_noop_scheduler() -> *mut sys::realm_scheduler_t {
    // SAFETY: all callbacks are `extern "C"` with the signatures realm-core declares, and we
    // pass no userdata, so there is nothing to free.
    unsafe {
        sys::realm_scheduler_new(
            std::ptr::null_mut(),
            None,
            None,
            Some(scheduler_is_on_thread),
            Some(scheduler_is_same_as),
            Some(scheduler_can_deliver_notifications),
        )
    }
}

/// The `RLM_CLASS_EMBEDDED` bit, as a type that matches `realm_class_info_t::flags`.
///
/// bindgen maps realm-core's C enums to `u32` on Linux but `i32` on Windows, so the constant
/// cannot be used directly in a portable bitwise test.
fn embedded_flag() -> i32 {
    i32::try_from(sys::realm_class_flags_RLM_CLASS_EMBEDDED)
        .expect("RLM_CLASS_EMBEDDED is a small bit flag")
}

/// An open Realm database.
///
/// Dropping this releases the underlying handle.
pub struct Realm {
    ptr: *mut sys::realm_t,
}

impl Realm {
    /// Opens `path` read-only, without migrating it.
    ///
    /// Migration callbacks only fire for `RLM_SCHEMA_MODE_AUTOMATIC` and
    /// `RLM_SCHEMA_MODE_MANUAL`, so opening a database whose schema is newer than we
    /// understand cannot rewrite it. Passing a null schema means "use whatever is on disk".
    ///
    /// Note that "read-only" describes the database contents, not the directory: realm-core
    /// still creates `<name>.lock` and `<name>.management/` beside the file. Callers that must
    /// not touch a directory at all should copy the database out first.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::InvalidPath`] if `path` contains an interior NUL, or
    /// [`RealmError::Core`] if realm-core cannot open the file.
    pub fn open_read_only(path: &Path) -> Result<Self, RealmError> {
        Self::open(path, sys::realm_schema_mode_RLM_SCHEMA_MODE_READ_ONLY)
    }

    /// Opens `path` for writing, still without migrating it.
    ///
    /// `RLM_SCHEMA_MODE_READ_ONLY` cannot write, so cleaning needs a different mode — but it
    /// must be one that adopts the on-disk schema rather than reconciling it against a
    /// declared one. `ADDITIVE_DISCOVERED` is that mode: combined with a null schema it adds
    /// nothing, and the migration callback only fires for `AUTOMATIC` and `MANUAL`.
    ///
    /// `open_preserves_schema` asserts the property that matters — that a full
    /// open/write/commit cycle leaves the schema version and class list untouched.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::InvalidPath`] if `path` contains an interior NUL, or
    /// [`RealmError::Core`] if realm-core cannot open the file.
    pub fn open_for_write(path: &Path) -> Result<Self, RealmError> {
        Self::open(
            path,
            sys::realm_schema_mode_RLM_SCHEMA_MODE_ADDITIVE_DISCOVERED,
        )
    }

    /// Shared open path. `mode` decides whether writes are permitted.
    fn open(path: &Path, mode: sys::realm_schema_mode_e) -> Result<Self, RealmError> {
        let raw_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| RealmError::InvalidPath(path.display().to_string()))?;

        let scheduler = make_noop_scheduler();
        if scheduler.is_null() {
            return Err(last_error());
        }

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "one indivisible config-and-open sequence; splitting it would leak the \
                      config on early return and obscure the ownership story below"
        )]
        // SAFETY: `realm_config_new` allocates a config we own and release below. Each setter
        // copies what it needs, so `raw_path` only has to outlive the `set_path` call.
        // `set_scheduler` copies the scheduler, so we release our handle afterwards.
        let ptr = unsafe {
            let config = sys::realm_config_new();
            sys::realm_config_set_path(config, raw_path.as_ptr());
            sys::realm_config_set_schema(config, std::ptr::null());
            sys::realm_config_set_schema_mode(config, mode);
            sys::realm_config_set_scheduler(config, scheduler);
            sys::realm_config_set_automatic_change_notifications(config, false);
            let realm = sys::realm_open(config);
            sys::realm_release(config.cast());
            sys::realm_release(scheduler.cast());
            realm
        };

        if ptr.is_null() {
            return Err(last_error());
        }

        Ok(Self { ptr })
    }

    /// Returns the schema version recorded in the file.
    ///
    /// Compare this against the `schema_version` constant in osu!lazer's `RealmAccess.cs`
    /// before writing anything.
    #[must_use]
    pub fn schema_version(&self) -> u64 {
        // SAFETY: `self.ptr` is a valid realm for our lifetime and this call cannot fail.
        unsafe { sys::realm_get_schema_version(self.ptr) }
    }

    /// Lists every class in the database, with its row count.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if realm-core rejects any of the schema queries.
    pub fn classes(&self) -> Result<Vec<ClassInfo>, RealmError> {
        let mut count = 0;

        // SAFETY: passing a null buffer with max 0 asks realm-core for the required size.
        if !unsafe { sys::realm_get_class_keys(self.ptr, std::ptr::null_mut(), 0, &raw mut count) }
        {
            return Err(last_error());
        }

        let mut keys = vec![0; count];
        // SAFETY: `keys` has room for `count` entries, which is the size realm-core just asked for.
        if !unsafe { sys::realm_get_class_keys(self.ptr, keys.as_mut_ptr(), count, &raw mut count) }
        {
            return Err(last_error());
        }

        keys.truncate(count);
        keys.into_iter().map(|key| self.class(key)).collect()
    }

    /// Reads every beatmap set, with the files each one owns.
    ///
    /// Sets that lazer would exclude from bulk operations are skipped: `DeletePending` ones
    /// are already on their way out, and `Protected` ones are the built-in tutorial and intro
    /// maps that `BeatmapManager.DeleteAllVideos` also filters out.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if the schema does not match what osu!lazer writes.
    pub fn beatmap_sets(&self) -> Result<Vec<BeatmapSet>, RealmError> {
        let class = self.class_key("BeatmapSet")?;
        let files_key = self.property_key(class, "Files")?;
        let delete_pending_key = self.property_key(class, "DeletePending")?;
        let protected_key = self.property_key(class, "Protected")?;

        let usage = self.class_key("RealmNamedFileUsage")?;
        let filename_key = self.property_key(usage, "Filename")?;
        let target_key = self.property_key(usage, "File")?;
        let hash_key = self.property_key(self.class_key("File")?, "Hash")?;

        let mut sets = Vec::new();

        for index in 0..self.count(class)? {
            let object = self.object_at(class, index)?;

            if object.boolean(delete_pending_key)? || object.boolean(protected_key)? {
                continue;
            }

            sets.push(BeatmapSet {
                index,
                files: Self::read_usages(&object, files_key, filename_key, target_key, hash_key)?,
            });
        }

        Ok(sets)
    }

    /// Counts how many usages across the whole database point at each file hash.
    ///
    /// This is the reference count that decides whether a blob can be removed. It spans every
    /// type that owns file usages, because a blob shared between a beatmap and a skin must not
    /// be removed when only the beatmap gives it up. `RealmFileStore.Cleanup` expresses the
    /// same rule as the query `Usages.@count = 0`.
    ///
    /// Owner classes absent from the schema are skipped rather than treated as an error:
    /// `RealmOnlineAsset` only exists from schema 52 onwards.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if a class that does exist has an unexpected shape.
    pub fn usage_counts(&self) -> Result<HashMap<String, u32>, RealmError> {
        let usage = self.class_key("RealmNamedFileUsage")?;
        let filename_key = self.property_key(usage, "Filename")?;
        let target_key = self.property_key(usage, "File")?;
        let hash_key = self.property_key(self.class_key("File")?, "Hash")?;

        let mut counts = HashMap::new();

        for (class_name, property) in OWNERS_WITH_FILE_LISTS {
            let Some(class) = self.optional_class_key(class_name)? else {
                continue;
            };
            let list_key = self.property_key(class, property)?;

            for index in 0..self.count(class)? {
                let object = self.object_at(class, index)?;
                for file in
                    Self::read_usages(&object, list_key, filename_key, target_key, hash_key)?
                {
                    *counts.entry(file.hash).or_insert(0) += 1;
                }
            }
        }

        Ok(counts)
    }

    /// Removes the named usages from their beatmap sets, in one transaction.
    ///
    /// This is `ModelManager.DeleteFile`, which is only `item.Files.Remove(file)`. Blobs are
    /// never touched here. lazer sweeps them separately once their backlink count reaches
    /// zero, and this tool moves them into a snapshot for the same reason: detaching the usage
    /// and disposing of the bytes are two different steps.
    ///
    /// Indices are resolved per set and erased from the highest index down, so that earlier
    /// erasures cannot shift the positions of later ones.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if the transaction cannot be committed. The transaction is
    /// cancelled on any failure, leaving the database untouched.
    pub fn erase_usages(&self, removals: &[Removal]) -> Result<(), RealmError> {
        if removals.is_empty() {
            return Ok(());
        }

        let class = self.class_key("BeatmapSet")?;
        let files_key = self.property_key(class, "Files")?;

        let mut by_set: HashMap<usize, Vec<usize>> = HashMap::new();
        for removal in removals {
            by_set
                .entry(removal.set_index)
                .or_default()
                .push(removal.file_index);
        }

        // SAFETY: begins a transaction we either commit or cancel on every path below.
        if !unsafe { sys::realm_begin_write(self.ptr) } {
            return Err(last_error());
        }

        for (set_index, mut file_indices) in by_set {
            file_indices.sort_unstable();
            file_indices.reverse();

            if let Err(error) = self.erase_from_set(class, files_key, set_index, &file_indices) {
                // SAFETY: a transaction is open; rolling back discards every edit above.
                unsafe { sys::realm_rollback(self.ptr) };
                return Err(error);
            }
        }

        // SAFETY: the transaction opened above is still current.
        if !unsafe { sys::realm_commit(self.ptr) } {
            return Err(last_error());
        }

        Ok(())
    }

    /// Erases the given indices from one set's `Files` list. Callers hold the transaction.
    fn erase_from_set(
        &self,
        class: sys::realm_class_key_t,
        files_key: sys::realm_property_key_t,
        set_index: usize,
        file_indices: &[usize],
    ) -> Result<(), RealmError> {
        let object = self.object_at(class, set_index)?;

        // SAFETY: `object` is live and `files_key` names a list property on its class.
        let list = unsafe { sys::realm_get_list(object.ptr, files_key) };
        if list.is_null() {
            return Err(last_error());
        }

        for &index in file_indices {
            // SAFETY: `list` is live for this loop and released immediately after it.
            if !unsafe { sys::realm_list_erase(list, index) } {
                // SAFETY: released before propagating the error.
                unsafe { sys::realm_release(list.cast()) };
                return Err(last_error());
            }
        }

        // SAFETY: released exactly once, after the last use above.
        unsafe { sys::realm_release(list.cast()) };
        Ok(())
    }

    /// Reads a list of `RealmNamedFileUsage` entries off an owner object.
    fn read_usages(
        owner: &value::Object,
        list_key: sys::realm_property_key_t,
        filename_key: sys::realm_property_key_t,
        target_key: sys::realm_property_key_t,
        hash_key: sys::realm_property_key_t,
    ) -> Result<Vec<NamedFile>, RealmError> {
        // SAFETY: `owner` is live and `list_key` names a list property on its class.
        let list = unsafe { sys::realm_get_list(owner.ptr, list_key) };
        if list.is_null() {
            return Err(last_error());
        }

        let mut size = 0;
        // SAFETY: `list` is live; released on every path below.
        if !unsafe { sys::realm_list_size(list, &raw mut size) } {
            // SAFETY: released before propagating.
            unsafe { sys::realm_release(list.cast()) };
            return Err(last_error());
        }

        let mut files = Vec::with_capacity(size);
        for index in 0..size {
            // SAFETY: `index` is below the size realm-core just reported.
            let entry = unsafe { sys::realm_list_get_linked_object(list, index) };
            let entry = match value::Object::from_raw(entry) {
                Ok(entry) => entry,
                Err(error) => {
                    // SAFETY: released before propagating.
                    unsafe { sys::realm_release(list.cast()) };
                    return Err(error);
                }
            };

            let read = (|| {
                let filename = entry.string(filename_key)?;
                // SAFETY: `target_key` is an object link on `RealmNamedFileUsage`.
                let file = unsafe { sys::realm_get_linked_object(entry.ptr, target_key) };
                let hash = value::Object::from_raw(file)?.string(hash_key)?;
                Ok::<_, RealmError>(NamedFile {
                    index,
                    filename,
                    hash,
                })
            })();

            match read {
                Ok(file) => files.push(file),
                Err(error) => {
                    // SAFETY: released before propagating.
                    unsafe { sys::realm_release(list.cast()) };
                    return Err(error);
                }
            }
        }

        // SAFETY: released exactly once, after the loop.
        unsafe { sys::realm_release(list.cast()) };
        Ok(files)
    }

    /// Counts rows in a class.
    fn count(&self, class: sys::realm_class_key_t) -> Result<usize, RealmError> {
        let mut rows = 0;
        // SAFETY: `class` came from this realm's schema.
        if !unsafe { sys::realm_get_num_objects(self.ptr, class, &raw mut rows) } {
            return Err(last_error());
        }
        Ok(rows)
    }

    /// Fetches the object at `index` in a class's natural order.
    fn object_at(
        &self,
        class: sys::realm_class_key_t,
        index: usize,
    ) -> Result<value::Object, RealmError> {
        // SAFETY: `class` came from this realm's schema.
        let results = unsafe { sys::realm_object_find_all(self.ptr, class) };
        if results.is_null() {
            return Err(last_error());
        }

        // SAFETY: `results` is live until released on the next line.
        let object = unsafe { sys::realm_results_get_object(results, index) };
        // SAFETY: released exactly once; `object` does not borrow from it.
        unsafe { sys::realm_release(results.cast()) };

        value::Object::from_raw(object)
    }

    /// Looks up a class key, returning `None` when the class is absent from the schema.
    fn optional_class_key(&self, name: &str) -> Result<Option<sys::realm_class_key_t>, RealmError> {
        let raw = value::c_string(name)?;
        let mut found = false;
        let mut info = sys::realm_class_info_t::default();

        // SAFETY: `raw` outlives the call and the out-params are ours.
        if !unsafe { sys::realm_find_class(self.ptr, raw.as_ptr(), &raw mut found, &raw mut info) }
        {
            return Err(last_error());
        }

        Ok(found.then_some(info.key))
    }

    /// Looks up a class key by its on-disk name.
    fn class_key(&self, name: &str) -> Result<sys::realm_class_key_t, RealmError> {
        let raw = CString::new(name).map_err(|_| RealmError::InvalidPath(name.to_owned()))?;
        let mut found = false;
        let mut info = sys::realm_class_info_t::default();

        // SAFETY: `raw` outlives the call, and the out-params are ours.
        if !unsafe { sys::realm_find_class(self.ptr, raw.as_ptr(), &raw mut found, &raw mut info) }
        {
            return Err(last_error());
        }

        if !found {
            return Err(RealmError::Core {
                code: 0,
                message: format!("class {name} not present in this schema"),
            });
        }

        Ok(info.key)
    }

    /// Looks up a property key by name within a class.
    fn property_key(
        &self,
        class: sys::realm_class_key_t,
        name: &str,
    ) -> Result<sys::realm_property_key_t, RealmError> {
        let raw = CString::new(name).map_err(|_| RealmError::InvalidPath(name.to_owned()))?;
        let mut found = false;
        let mut info = sys::realm_property_info_t::default();

        // SAFETY: `raw` outlives the call, and the out-params are ours.
        if !unsafe {
            sys::realm_find_property(self.ptr, class, raw.as_ptr(), &raw mut found, &raw mut info)
        } {
            return Err(last_error());
        }

        if !found {
            return Err(RealmError::Core {
                code: 0,
                message: format!("property {name} not present"),
            });
        }

        Ok(info.key)
    }

    /// Describes a single class.
    fn class(&self, key: sys::realm_class_key_t) -> Result<ClassInfo, RealmError> {
        let mut info = sys::realm_class_info_t::default();

        // SAFETY: `realm_get_class` fills the struct with pointers owned by the realm, which
        // outlives the copy we make below.
        if !unsafe { sys::realm_get_class(self.ptr, key, &raw mut info) } {
            return Err(last_error());
        }

        let mut rows = 0;
        // SAFETY: `key` came from `realm_get_class_keys` on this same realm.
        if !unsafe { sys::realm_get_num_objects(self.ptr, key, &raw mut rows) } {
            return Err(last_error());
        }

        let name = if info.name.is_null() {
            String::new()
        } else {
            // SAFETY: realm-core owns this string and keeps it alive for the realm's lifetime.
            unsafe { CStr::from_ptr(info.name) }
                .to_string_lossy()
                .into_owned()
        };

        Ok(ClassInfo {
            name,
            rows,
            embedded: info.flags & embedded_flag() != 0,
        })
    }
}

impl Drop for Realm {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was produced by `realm_open` and is released exactly once.
        unsafe { sys::realm_release(self.ptr.cast()) };
    }
}

/// A class in the database's schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    /// On-disk class name, e.g. `BeatmapSet` rather than lazer's `BeatmapSetInfo`.
    pub name: String,
    /// Number of rows.
    pub rows: usize,
    /// Whether this is an embedded class, which cannot be queried independently.
    pub embedded: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copies the sample database into a scratch directory and returns the copy's path.
    ///
    /// Opening a realm creates `client.realm.lock` and `client.realm.management/` beside it,
    /// even in read-only mode — read-only refers to the database contents, not the directory.
    /// The sample library is reference data we must not alter, so every test works on a copy.
    /// Production code has the same obligation when scanning a live library, and solves it the
    /// same way.
    fn sample_realm(scratch: &tempfile::TempDir) -> Option<std::path::PathBuf> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ref/client.realm");
        if !source.is_file() {
            return None;
        }

        let copy = scratch.path().join("client.realm");
        std::fs::copy(&source, &copy).expect("failed to copy sample database");
        Some(copy)
    }

    /// Wraps a test body that needs the sample database, skipping it when absent.
    ///
    /// `ref/client.realm` is a real personal osu!lazer library, so it is gitignored and never
    /// present in CI. These tests therefore cover what only a developer with a real library
    /// can reach. Building a committable synthetic fixture through realm-core would let CI
    /// run them too, and is worth doing once the schema we depend on has settled.
    fn with_sample(body: impl FnOnce(&Path)) {
        let scratch = tempfile::tempdir().expect("failed to create scratch dir");
        let Some(path) = sample_realm(&scratch) else {
            tracing::warn!("skipping: ref/client.realm not present");
            return;
        };
        body(&path);
    }

    /// The spike gate: realm-core builds, binds, and reads a real lazer library.
    #[test]
    fn reads_real_lazer_database() {
        with_sample(|path| {
            let realm = Realm::open_read_only(path).expect("failed to open sample database");
            let classes = realm.classes().expect("failed to enumerate schema");

            let named = |name: &str| {
                classes
                    .iter()
                    .find(|c| c.name == name)
                    .unwrap_or_else(|| panic!("class {name} missing; found {classes:#?}"))
                    .rows
            };

            // Names are the on-disk `[MapTo]` names from ppy/osu, not the C# class names.
            assert!(named("BeatmapSet") > 0, "expected beatmap sets");
            assert!(named("Beatmap") > 0, "expected beatmaps");
            assert!(named("File") > 0, "expected file rows");

            // osu!lazer was at schema 52 when this was written; older files are still readable.
            assert!(realm.schema_version() > 0, "expected a real schema version");
        });
    }

    /// The write-path gate: a real transaction must not migrate the database.
    ///
    /// `RLM_SCHEMA_MODE_READ_ONLY` cannot write, so cleaning uses `ADDITIVE_DISCOVERED`. That
    /// mode is only safe if it adopts the on-disk schema rather than reconciling it against a
    /// declared one, so this asserts the three things that would reveal a migration: the
    /// schema version is unchanged, the class list is unchanged, and the edit actually landed.
    #[test]
    fn writing_does_not_migrate_the_database() {
        with_sample(|path| {
            // Row counts legitimately change when we edit, so compare schema shape only.
            let shape = |realm: &Realm| -> Vec<(String, bool)> {
                realm
                    .classes()
                    .expect("failed to enumerate schema")
                    .into_iter()
                    .map(|c| (c.name, c.embedded))
                    .collect()
            };

            let (version_before, shape_before) = {
                let realm = Realm::open_read_only(path).expect("failed to open before");
                (realm.schema_version(), shape(&realm))
            };

            let usages_before = {
                let realm = Realm::open_read_only(path).expect("failed to open before");
                realm
                    .classes()
                    .expect("failed to enumerate schema")
                    .into_iter()
                    .find(|c| c.name == "RealmNamedFileUsage")
                    .expect("RealmNamedFileUsage missing")
                    .rows
            };

            // Detach the first two usages of one set through the production write path.
            let (set_index, erased, files_before) = {
                let realm = Realm::open_for_write(path).expect("failed to open for write");
                let sets = realm.beatmap_sets().expect("failed to read beatmap sets");
                let set = sets
                    .iter()
                    .find(|s| s.files.len() >= 2)
                    .expect("no beatmap set with two files to erase");

                let removals: Vec<_> = set.files[..2]
                    .iter()
                    .map(|f| Removal {
                        set_index: set.index,
                        file_index: f.index,
                    })
                    .collect();

                realm
                    .erase_usages(&removals)
                    .expect("failed to erase usages");
                (set.index, removals.len(), set.files.len())
            };

            let realm = Realm::open_read_only(path).expect("failed to reopen");

            let after = realm
                .beatmap_sets()
                .expect("failed to reread beatmap sets")
                .into_iter()
                .find(|s| s.index == set_index)
                .expect("the edited set disappeared");
            assert_eq!(
                after.files.len(),
                files_before - erased,
                "the erase did not survive the commit"
            );
            assert_eq!(
                realm.schema_version(),
                version_before,
                "schema version changed: the database was migrated"
            );
            assert_eq!(
                shape(&realm),
                shape_before,
                "class list changed: the database was migrated"
            );

            // Erasing the list entry must also drop the embedded row, since an EmbeddedObject
            // cannot outlive its owner. This is what makes lazer's later zero-backlink sweep see
            // the blob as orphaned.
            let usages = |realm: &Realm| {
                realm
                    .classes()
                    .expect("failed to enumerate schema")
                    .into_iter()
                    .find(|c| c.name == "RealmNamedFileUsage")
                    .expect("RealmNamedFileUsage missing")
                    .rows
            };
            assert_eq!(
                usages(&realm),
                usages_before - erased,
                "erasing the list entry did not delete the embedded usage row"
            );
        });
    }

    /// Reference counts must span every owner, and dedup must be visible in the numbers.
    #[test]
    fn counts_usages_across_all_owners() {
        with_sample(|path| {
            let realm = Realm::open_read_only(path).expect("failed to open");

            let counts = realm.usage_counts().expect("failed to count usages");
            let sets = realm.beatmap_sets().expect("failed to read beatmap sets");

            let total: u32 = counts.values().sum();
            let from_sets: u32 = sets
                .iter()
                .map(|s| u32::try_from(s.files.len()).expect("set has a sane file count"))
                .sum();

            // Scores and skins own usages too, so the global total must exceed what beatmap
            // sets alone account for. Counting only sets is the bug this guards against.
            assert!(
                total > from_sets,
                "expected owners beyond beatmap sets: {total} total vs {from_sets} from sets"
            );

            // Deduplication is the whole reason refcounting exists: some blob must be shared.
            assert!(
                counts.values().any(|&n| n > 1),
                "expected at least one blob shared by more than one usage"
            );

            let files: usize = sets.iter().map(|s| s.files.len()).sum();
            assert!(files > 0, "expected beatmap sets to own files");
        });
    }

    /// Prints the discovered schema. Run with `--nocapture` to inspect a library.
    #[test]
    #[expect(
        clippy::print_stdout,
        reason = "diagnostic test, only visible under --nocapture"
    )]
    fn dumps_schema() {
        with_sample(|path| {
            let realm = Realm::open_read_only(path).expect("failed to open");
            println!("schema version: {}", realm.schema_version());

            let mut classes = realm.classes().expect("failed to enumerate schema");
            classes.sort_by_key(|c| std::cmp::Reverse(c.rows));

            for class in &classes {
                let kind = if class.embedded { "embedded" } else { "table" };
                println!("{:>10}  {:<24} {kind}", class.rows, class.name);
            }
        });
    }
}
