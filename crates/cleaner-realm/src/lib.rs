//! Safe bindings to realm-core's C API, scoped to what an osu!lazer library cleaner needs.
//!
//! The schema is discovered from the database file rather than declared in Rust. osu!lazer
//! bumps its schema version regularly, and a tool that hardcodes the model classes breaks on
//! every bump. Discovery via [`Realm::classes`] keeps us working: the sample library is at
//! schema 51, where `RealmOnlineAsset` does not yet exist, while current lazer is at 52.

pub mod sys;

use std::ffi::{CStr, CString};
use std::path::Path;

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
fn last_error() -> RealmError {
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

    /// Removes one `RealmNamedFileUsage` from a beatmap set's `Files` list, in a transaction.
    ///
    /// This mirrors `ModelManager.DeleteFile`, which is just `item.Files.Remove(file)` — the
    /// blob is swept later by the zero-backlink pass, never here. It exists at this layer so
    /// the spike can prove the write path round-trips without migrating anything.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if the class or property is missing, if there is no row at
    /// `set_index`, or if realm-core rejects the transaction.
    pub fn erase_beatmap_set_file(
        &self,
        set_index: usize,
        file_index: usize,
    ) -> Result<(), RealmError> {
        let class = self.class_key("BeatmapSet")?;
        let property = self.property_key(class, "Files")?;

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "a single transaction: each handle is released before the next step, so                       splitting the block would move releases away from their acquisitions"
        )]
        // SAFETY: every pointer below is owned by us and released on each exit path; the keys
        // came from this realm.
        unsafe {
            if !sys::realm_begin_write(self.ptr) {
                return Err(last_error());
            }

            let results = sys::realm_object_find_all(self.ptr, class);
            if results.is_null() {
                return Err(last_error());
            }

            let object = sys::realm_results_get_object(results, set_index);
            sys::realm_release(results.cast());
            if object.is_null() {
                return Err(last_error());
            }

            let list = sys::realm_get_list(object, property);
            sys::realm_release(object.cast());
            if list.is_null() {
                return Err(last_error());
            }

            let erased = sys::realm_list_erase(list, file_index);
            sys::realm_release(list.cast());
            if !erased {
                return Err(last_error());
            }

            if !sys::realm_commit(self.ptr) {
                return Err(last_error());
            }
        }

        Ok(())
    }

    /// Counts entries in a beatmap set's `Files` list.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if the class or property is missing, or there is no row at
    /// `set_index`.
    pub fn beatmap_set_file_count(&self, set_index: usize) -> Result<usize, RealmError> {
        let class = self.class_key("BeatmapSet")?;
        let property = self.property_key(class, "Files")?;
        let mut size = 0;

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "one lookup chain: each handle is released before the next step, so                       splitting the block would move releases away from their acquisitions"
        )]
        // SAFETY: as above; each handle is released before the next step.
        unsafe {
            let results = sys::realm_object_find_all(self.ptr, class);
            if results.is_null() {
                return Err(last_error());
            }

            let object = sys::realm_results_get_object(results, set_index);
            sys::realm_release(results.cast());
            if object.is_null() {
                return Err(last_error());
            }

            let list = sys::realm_get_list(object, property);
            sys::realm_release(object.cast());
            if list.is_null() {
                return Err(last_error());
            }

            let ok = sys::realm_list_size(list, &raw mut size);
            sys::realm_release(list.cast());
            if !ok {
                return Err(last_error());
            }
        }

        Ok(size)
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
    fn sample_realm(scratch: &tempfile::TempDir) -> std::path::PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ref/client.realm");
        assert!(
            source.is_file(),
            "missing sample database at {}",
            source.display()
        );

        let copy = scratch.path().join("client.realm");
        std::fs::copy(&source, &copy).expect("failed to copy sample database");
        copy
    }

    /// The spike gate: realm-core builds, binds, and reads a real lazer library.
    #[test]
    fn reads_real_lazer_database() {
        let scratch = tempfile::tempdir().expect("failed to create scratch dir");
        let path = sample_realm(&scratch);

        let realm = Realm::open_read_only(&path).expect("failed to open sample database");
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
    }

    /// The write-path gate: a real transaction must not migrate the database.
    ///
    /// `RLM_SCHEMA_MODE_READ_ONLY` cannot write, so cleaning uses `ADDITIVE_DISCOVERED`. That
    /// mode is only safe if it adopts the on-disk schema rather than reconciling it against a
    /// declared one, so this asserts the three things that would reveal a migration: the
    /// schema version is unchanged, the class list is unchanged, and the edit actually landed.
    #[test]
    fn writing_does_not_migrate_the_database() {
        let scratch = tempfile::tempdir().expect("failed to create scratch dir");
        let path = sample_realm(&scratch);

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
            let realm = Realm::open_read_only(&path).expect("failed to open before");
            (realm.schema_version(), shape(&realm))
        };

        let usages_before = {
            let realm = Realm::open_read_only(&path).expect("failed to open before");
            realm
                .classes()
                .expect("failed to enumerate schema")
                .into_iter()
                .find(|c| c.name == "RealmNamedFileUsage")
                .expect("RealmNamedFileUsage missing")
                .rows
        };

        let files_before = {
            let realm = Realm::open_for_write(&path).expect("failed to open for write");
            let before = realm
                .beatmap_set_file_count(0)
                .expect("failed to count files");
            assert!(before > 0, "first beatmap set has no files to erase");

            realm
                .erase_beatmap_set_file(0, 0)
                .expect("failed to erase file usage");
            before
        };

        let realm = Realm::open_read_only(&path).expect("failed to reopen");

        assert_eq!(
            realm.beatmap_set_file_count(0).expect("failed to recount"),
            files_before - 1,
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
            usages_before - 1,
            "erasing the list entry did not delete the embedded usage row"
        );
    }

    /// Prints the discovered schema. Run with `--nocapture` to inspect a library.
    #[test]
    #[expect(
        clippy::print_stdout,
        reason = "diagnostic test, only visible under --nocapture"
    )]
    fn dumps_schema() {
        let scratch = tempfile::tempdir().expect("failed to create scratch dir");
        let realm = Realm::open_read_only(&sample_realm(&scratch)).expect("failed to open");
        println!("schema version: {}", realm.schema_version());

        let mut classes = realm.classes().expect("failed to enumerate schema");
        classes.sort_by_key(|c| std::cmp::Reverse(c.rows));

        for class in &classes {
            let kind = if class.embedded { "embedded" } else { "table" };
            println!("{:>10}  {:<24} {kind}", class.rows, class.name);
        }
    }
}
