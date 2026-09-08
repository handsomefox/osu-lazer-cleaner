//! Safe bindings to realm-core's C API, scoped to what an osu!lazer library cleaner needs.
//!
//! The schema is discovered from the database file rather than declared in Rust. osu!lazer
//! bumps its schema version regularly, and a tool that hardcodes the model classes breaks on
//! every bump. Discovery via [`Realm::classes`] keeps us working: the sample library is at
//! schema 51, where `RealmOnlineAsset` does not yet exist, while current lazer is at 52.

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;
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

/// Everything a scan needs from the database.
#[derive(Debug, Clone)]
pub struct Library {
    /// Beatmap sets eligible for cleaning.
    pub beatmap_sets: Vec<BeatmapSet>,
    /// How many usages across the whole database point at each file hash.
    ///
    /// This is the reference count that decides whether a blob can be removed.
    /// `RealmFileStore.Cleanup` expresses the same rule as `Usages.@count = 0`.
    pub usage_counts: HashMap<String, u32>,
}

/// A beatmap set and the files it owns.
#[derive(Debug, Clone)]
pub struct BeatmapSet {
    /// Position in the class's natural order.
    ///
    /// Only valid for as long as the database is unchanged, so it addresses a set during the
    /// clean that follows a scan and never afterwards. Use [`BeatmapSet::id`] to name a set
    /// across time.
    pub index: usize,
    /// The set's `Guid` primary key, as 16 bytes.
    ///
    /// Stable across imports and deletions, unlike the position.
    pub id: [u8; 16],
    /// Files the set owns, in list order.
    pub files: Vec<NamedFile>,
    /// Artist and title, from the first difficulty's `BeatmapMetadata`.
    ///
    /// Empty when the set has no difficulty or the metadata names neither. Used to label a set
    /// in the interface, never to identify one.
    pub title: String,
    /// Audio tracks its difficulties name, from `BeatmapMetadata.AudioFile`.
    ///
    /// Reading these from the database avoids opening the difficulty files, which is by far
    /// the slowest part of a scan.
    pub audio: Vec<String>,
    /// Background images its difficulties name, from `BeatmapMetadata.BackgroundFile`.
    pub backgrounds: Vec<String>,
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

/// Property keys needed to read a beatmap set and its metadata.
#[derive(Debug, Clone, Copy)]
struct BeatmapSetKeys {
    id: sys::realm_property_key_t,
    delete_pending: sys::realm_property_key_t,
    protected: sys::realm_property_key_t,
    beatmaps: sys::realm_property_key_t,
    metadata: sys::realm_property_key_t,
    audio_file: sys::realm_property_key_t,
    background_file: sys::realm_property_key_t,
    artist: sys::realm_property_key_t,
    title: sys::realm_property_key_t,
}

/// What one pass over a set's difficulties learned from `BeatmapMetadata`.
struct SetMetadata {
    /// Artist and title of the first difficulty that names either.
    title: String,
    /// Audio tracks the difficulties name.
    audio: Vec<String>,
    /// Background images the difficulties name.
    backgrounds: Vec<String>,
}

/// Joins an artist and a title the way osu!lazer shows them, skipping either when it is empty.
fn join_title(artist: &str, title: &str) -> String {
    match (artist.is_empty(), title.is_empty()) {
        (true, true) => String::new(),
        (true, false) => title.to_owned(),
        (false, true) => artist.to_owned(),
        (false, false) => format!("{artist} - {title}"),
    }
}

/// A usage to reattach to a beatmap set when restoring a snapshot.
#[derive(Debug, Clone)]
pub struct Restoration {
    /// The owning set's `Guid` primary key.
    ///
    /// Named by identity rather than position, because a restore can happen long after the
    /// clean that produced it, and importing or deleting a beatmap shifts every position.
    pub set_id: [u8; 16],
    /// Name the file had inside the set.
    pub filename: String,
    /// SHA-256 of the file's contents.
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
        ///
        /// Held as `i64` because bindgen maps C enums to `u32` on Linux and `i32` on Windows.
        /// Both convert into `i64` losslessly, so no cast is needed on either platform.
        code: i64,
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
        code: i64::from(err.error),
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

/// Reports whether a class's flags mark it as embedded.
///
/// `realm_class_info_t::flags` is a C `int`, but the flag constants come from an enum, and
/// bindgen maps C enums to `u32` on Linux and `i32` on Windows. Neither a cast nor a conversion
/// is portable, because whichever one is written is redundant on one of the two platforms.
/// Widening both sides to `i64` is lossless from either.
fn is_embedded(flags: i32) -> bool {
    i64::from(flags) & i64::from(sys::realm_class_flags_RLM_CLASS_EMBEDDED) != 0
}

/// An open Realm database.
///
/// Dropping this releases the underlying handle.
pub struct Realm {
    ptr: *mut sys::realm_t,
}

impl Realm {
    fn check_schema(&self) -> Result<(), RealmError> {
        let version = self.schema_version();
        if version > 52 {
            return Err(RealmError::Core {
                code: 0,
                message: format!(
                    "unsupported osu!lazer schema {version}; this build supports up to 52"
                ),
            });
        }
        Ok(())
    }

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

    /// Rewrites the database without its free space, and reports whether it was rewritten.
    ///
    /// Realm never shrinks on its own. Deleting rows returns their space to an internal free
    /// list for reuse, so the file stays the same size however much is removed. Compacting
    /// rewrites it without that free space, which is the only way the file gets smaller.
    ///
    /// osu!lazer does the same thing in `RealmAccess.BlockAllOperations`. It is safe only when
    /// nothing else has the database open.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] on a database error. Returns `false` when another handle
    /// prevents compaction.
    pub fn compact(&self) -> Result<bool, RealmError> {
        let mut compacted = false;

        // SAFETY: `self.ptr` is a valid realm and the out-param is ours.
        if !unsafe { sys::realm_compact(self.ptr, &raw mut compacted) } {
            return Err(last_error());
        }

        Ok(compacted)
    }

    /// Reads every beatmap set and counts every file reference, in one pass.
    ///
    /// These two results are returned together because gathering them separately means walking
    /// every file usage in the database twice, and a large library holds hundreds of thousands
    /// of them. That second pass was a large part of the time a scan took.
    ///
    /// The returned sets exclude ones osu!lazer itself excludes from bulk operations:
    /// `DeletePending` sets are already on their way out, and `Protected` sets are the built-in
    /// tutorial and intro maps that `BeatmapManager.DeleteAllVideos` also filters out. The
    /// counts still include those sets, because a reference held by a protected set is a
    /// reference all the same.
    ///
    /// Owner classes absent from the schema are skipped rather than treated as an error:
    /// `RealmOnlineAsset` only exists from schema 52 onwards.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if the schema does not match what osu!lazer writes.
    pub fn read_library(&self) -> Result<Library, RealmError> {
        self.check_schema()?;
        let usage = self.class_key("RealmNamedFileUsage")?;
        let filename_key = self.property_key(usage, "Filename")?;
        let target_key = self.property_key(usage, "File")?;
        let hash_key = self.property_key(self.class_key("File")?, "Hash")?;

        let mut sets = Vec::new();
        let mut counts: HashMap<String, u32> = HashMap::new();

        for (class_name, property) in OWNERS_WITH_FILE_LISTS {
            let Some(class) = self.optional_class_key(class_name)? else {
                continue;
            };

            let list_key = self.property_key(class, property)?;
            let is_beatmap_set = *class_name == "BeatmapSet";

            let set_keys = if is_beatmap_set {
                Some(BeatmapSetKeys {
                    delete_pending: self.property_key(class, "DeletePending")?,
                    protected: self.property_key(class, "Protected")?,
                    id: self.property_key(class, "ID")?,
                    beatmaps: self.property_key(class, "Beatmaps")?,
                    metadata: self.property_key(self.class_key("Beatmap")?, "Metadata")?,
                    audio_file: self
                        .property_key(self.class_key("BeatmapMetadata")?, "AudioFile")?,
                    background_file: self
                        .property_key(self.class_key("BeatmapMetadata")?, "BackgroundFile")?,
                    artist: self.property_key(self.class_key("BeatmapMetadata")?, "Artist")?,
                    title: self.property_key(self.class_key("BeatmapMetadata")?, "Title")?,
                })
            } else {
                None
            };

            for index in 0..self.count(class)? {
                let object = self.object_at(class, index)?;
                let files =
                    Self::read_usages(&object, list_key, filename_key, target_key, hash_key)?;

                for file in &files {
                    *counts.entry(file.hash.clone()).or_insert(0) += 1;
                }

                if let Some(keys) = set_keys
                    && !object.boolean(keys.delete_pending)?
                    && !object.boolean(keys.protected)?
                {
                    let metadata = Self::read_metadata(&object, keys)?;
                    sets.push(BeatmapSet {
                        index,
                        id: object.uuid(keys.id)?.unwrap_or_default(),
                        files,
                        title: metadata.title,
                        audio: metadata.audio,
                        backgrounds: metadata.backgrounds,
                    });
                }
            }
        }

        if let Some(class) = self.optional_class_key("RealmOnlineAsset")? {
            let asset_file = self.property_key(class, "File")?;
            for index in 0..self.count(class)? {
                let asset = self.object_at(class, index)?;
                // SAFETY: `asset_file` names the embedded usage on this asset.
                let usage = unsafe { sys::realm_get_linked_object(asset.ptr, asset_file) };
                let usage = value::Object::from_raw(usage)?;
                // SAFETY: `target_key` names the File link on this usage.
                let file = unsafe { sys::realm_get_linked_object(usage.ptr, target_key) };
                let hash = value::Object::from_raw(file)?.string(hash_key)?;
                *counts.entry(hash).or_insert(0) += 1;
            }
        }

        Ok(Library {
            beatmap_sets: sets,
            usage_counts: counts,
        })
    }

    /// Reads the audio and background filenames every difficulty in a set names.
    ///
    /// `BeatmapMetadata` holds both, so a scan can learn what a set's audio track and
    /// background are without opening a single difficulty file. The same object carries the
    /// artist and title, which label the set in the interface.
    fn read_metadata(
        owner: &value::Object,
        keys: BeatmapSetKeys,
    ) -> Result<SetMetadata, RealmError> {
        let mut audio = Vec::new();
        let mut backgrounds = Vec::new();
        let mut title = String::new();

        // SAFETY: `owner` is live and `beatmaps` names a list property on its class.
        let list = unsafe { sys::realm_get_list(owner.ptr, keys.beatmaps) };
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

        for index in 0..size {
            // SAFETY: `index` is below the size realm-core just reported.
            let beatmap = unsafe { sys::realm_list_get_linked_object(list, index) };

            let read = (|| {
                let beatmap = value::Object::from_raw(beatmap)?;
                // SAFETY: `metadata` is an object link on `Beatmap`.
                let metadata = unsafe { sys::realm_get_linked_object(beatmap.ptr, keys.metadata) };
                let metadata = value::Object::from_raw(metadata)?;
                Ok::<_, RealmError>((
                    metadata.string(keys.audio_file)?,
                    metadata.string(keys.background_file)?,
                    metadata.string(keys.artist)?,
                    metadata.string(keys.title)?,
                ))
            })();

            match read {
                Ok((track, background, artist, name)) => {
                    if !track.is_empty() && !audio.contains(&track) {
                        audio.push(track);
                    }
                    if !background.is_empty() && !backgrounds.contains(&background) {
                        backgrounds.push(background);
                    }
                    if title.is_empty() {
                        title = join_title(&artist, &name);
                    }
                }
                Err(error) => {
                    // SAFETY: released before propagating.
                    unsafe { sys::realm_release(list.cast()) };
                    return Err(error);
                }
            }
        }

        // SAFETY: released exactly once, after the loop.
        unsafe { sys::realm_release(list.cast()) };
        Ok(SetMetadata {
            title,
            audio,
            backgrounds,
        })
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
        self.erase_usages_with(|_| Ok((removals.to_vec(), ())))
    }

    /// Validates a plan and records its recovery data under the database write lock.
    ///
    /// `prepare` runs before any usages are removed. It returns the removals and a value
    /// that is returned after commit. A preparation or erase error rolls back the transaction.
    ///
    /// # Errors
    ///
    /// Returns the preparation error or a database transaction error.
    pub fn erase_usages_with<T, E: From<RealmError>>(
        &self,
        prepare: impl FnOnce(&Self) -> Result<(Vec<Removal>, T), E>,
    ) -> Result<T, E> {
        self.check_schema()?;
        // SAFETY: every path below commits or rolls back this transaction.
        if !unsafe { sys::realm_begin_write(self.ptr) } {
            return Err(last_error().into());
        }
        let result = (|| {
            let (removals, prepared) = prepare(self)?;

            let class = self.class_key("BeatmapSet")?;
            let files_key = self.property_key(class, "Files")?;

            let mut by_set: HashMap<usize, Vec<usize>> = HashMap::new();
            for removal in &removals {
                by_set
                    .entry(removal.set_index)
                    .or_default()
                    .push(removal.file_index);
            }

            for (set_index, mut file_indices) in by_set {
                file_indices.sort_unstable();
                file_indices.dedup();
                file_indices.reverse();

                self.erase_from_set(class, files_key, set_index, &file_indices)?;
            }
            Ok(prepared)
        })();

        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                // SAFETY: the transaction above is open.
                unsafe { sys::realm_rollback(self.ptr) };
                return Err(error);
            }
        };

        // SAFETY: the transaction opened above is still current.
        if !unsafe { sys::realm_commit(self.ptr) } {
            let error = last_error();
            // SAFETY: cancel the failed commit before releasing the handle.
            unsafe { sys::realm_rollback(self.ptr) };
            return Err(error.into());
        }

        Ok(prepared)
    }

    /// Reattaches usages to their beatmap sets, in one transaction.
    ///
    /// This is the inverse of [`Realm::erase_usages`], used when restoring a snapshot. Each
    /// entry recreates a `RealmNamedFileUsage` at the end of its set's list and points it at
    /// the `File` row for that hash, recreating the row if lazer removed it.
    ///
    /// Without this, restoring would put the bytes back but leave nothing referring to them,
    /// and osu!lazer would delete them again on its next startup.
    ///
    /// # Errors
    ///
    /// Returns [`RealmError::Core`] if a set is missing, a filename has different content,
    /// or the transaction cannot be committed. Any failure rolls back the transaction.
    pub fn restore_usages(
        &self,
        restorations: &[Restoration],
        mut progress: impl FnMut(usize, usize),
    ) -> Result<usize, RealmError> {
        self.check_schema()?;
        if restorations.is_empty() {
            return Ok(0);
        }

        let class = self.class_key("BeatmapSet")?;
        let files_key = self.property_key(class, "Files")?;
        let usage = self.class_key("RealmNamedFileUsage")?;
        let filename_key = self.property_key(usage, "Filename")?;
        let target_key = self.property_key(usage, "File")?;
        let file_class = self.class_key("File")?;

        // Group by set so each one is located once and its file list read once. Doing that per
        // usage meant a fresh query and a full list read for every file being restored.
        let mut by_set: HashMap<[u8; 16], Vec<&Restoration>> = HashMap::new();
        for restoration in restorations {
            by_set
                .entry(restoration.set_id)
                .or_default()
                .push(restoration);
        }

        // SAFETY: begins a transaction that every path below either commits or rolls back.
        if !unsafe { sys::realm_begin_write(self.ptr) } {
            return Err(last_error());
        }

        let total = by_set.len();
        let mut restored = 0;

        for (done, (set_id, entries)) in by_set.into_iter().enumerate() {
            if done % 64 == 0 {
                progress(done, total);
            }

            let outcome = self.restore_into_set(
                class,
                files_key,
                filename_key,
                target_key,
                file_class,
                set_id,
                &entries,
            );

            match outcome {
                Ok(count) => restored += count,
                Err(error) => {
                    // SAFETY: a transaction is open; rolling back discards every edit above.
                    unsafe { sys::realm_rollback(self.ptr) };
                    return Err(error);
                }
            }
        }

        progress(total, total);

        // SAFETY: the transaction opened above is still current.
        if !unsafe { sys::realm_commit(self.ptr) } {
            return Err(last_error());
        }

        Ok(restored)
    }

    /// Reattaches every usage belonging to one set. Callers hold the transaction.
    #[expect(
        clippy::too_many_arguments,
        reason = "property keys are looked up once by the caller and passed down"
    )]
    fn restore_into_set(
        &self,
        class: sys::realm_class_key_t,
        files_key: sys::realm_property_key_t,
        filename_key: sys::realm_property_key_t,
        target_key: sys::realm_property_key_t,
        file_class: sys::realm_class_key_t,
        set_id: [u8; 16],
        entries: &[&Restoration],
    ) -> Result<usize, RealmError> {
        let Some(object) = self.find_set_by_id(class, set_id)? else {
            return Err(RealmError::Core {
                code: 0,
                message: "the snapshot's beatmap set no longer exists".to_owned(),
            });
        };

        let hash_key = self.property_key(file_class, "Hash")?;
        let mut present: HashMap<_, _> =
            Self::read_usages(&object, files_key, filename_key, target_key, hash_key)?
                .into_iter()
                .map(|file| (file.filename, file.hash))
                .collect();

        for entry in entries {
            if present
                .get(&entry.filename)
                .is_some_and(|hash| hash != &entry.hash)
            {
                return Err(RealmError::Core {
                    code: 0,
                    message: format!(
                        "{} now refers to different content; keeping the snapshot",
                        entry.filename
                    ),
                });
            }
        }

        // SAFETY: `object` is live and `files_key` names a list property on its class. The
        // handle is acquired once and reused, rather than reacquired for every file.
        let list = unsafe { sys::realm_get_list(object.ptr, files_key) };
        if list.is_null() {
            return Err(last_error());
        }

        let mut restored = 0;
        for entry in entries {
            // Already listed, which happens when a restore runs twice.
            if present.contains_key(&entry.filename) {
                continue;
            }

            if let Err(error) = self.append_usage(list, filename_key, target_key, file_class, entry)
            {
                // SAFETY: released before propagating.
                unsafe { sys::realm_release(list.cast()) };
                return Err(error);
            }

            restored += 1;
            present.insert(entry.filename.clone(), entry.hash.clone());
        }

        // SAFETY: released exactly once, after the loop.
        unsafe { sys::realm_release(list.cast()) };
        Ok(restored)
    }

    /// Adds one `RealmNamedFileUsage` to the end of an already-acquired file list.
    fn append_usage(
        &self,
        list: *mut sys::realm_list_t,
        filename_key: sys::realm_property_key_t,
        target_key: sys::realm_property_key_t,
        file_class: sys::realm_class_key_t,
        entry: &Restoration,
    ) -> Result<(), RealmError> {
        let file = self.find_file_row(file_class, &entry.hash)?;
        let raw_name = value::c_string(&entry.filename)?;

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "one insert-and-populate sequence; the new object is unusable until both \
                      of its properties are set"
        )]
        // SAFETY: `list` and `file` are live, and the caller owns the list handle.
        unsafe {
            let mut size = 0;
            if !sys::realm_list_size(list, &raw mut size) {
                return Err(last_error());
            }

            let entry_object = sys::realm_list_insert_embedded(list, size);
            let entry_object = value::Object::from_raw(entry_object)?;

            let name = sys::realm_value_t {
                __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                    string: sys::realm_string_t {
                        data: raw_name.as_ptr(),
                        size: entry.filename.len(),
                    },
                },
                type_: sys::realm_value_type_RLM_TYPE_STRING,
            };
            if !sys::realm_set_value(entry_object.ptr, filename_key, name, false) {
                return Err(last_error());
            }

            let link = sys::realm_object_as_link(file.ptr);
            let target = sys::realm_value_t {
                __bindgen_anon_1: sys::realm_value__bindgen_ty_1 { link },
                type_: sys::realm_value_type_RLM_TYPE_LINK,
            };
            if !sys::realm_set_value(entry_object.ptr, target_key, target, false) {
                return Err(last_error());
            }
        }

        Ok(())
    }

    /// Finds a beatmap set by its `Guid` primary key.
    ///
    /// Returns `None` when no set matches, which means the beatmap was deleted since the
    /// snapshot was taken.
    fn find_set_by_id(
        &self,
        class: sys::realm_class_key_t,
        id: [u8; 16],
    ) -> Result<Option<value::Object>, RealmError> {
        let key = sys::realm_value_t {
            __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                uuid: sys::realm_uuid_t { bytes: id },
            },
            type_: sys::realm_value_type_RLM_TYPE_UUID,
        };

        let mut found = false;
        // SAFETY: `class` came from this realm's schema and the out-param is ours.
        let object = unsafe {
            sys::realm_object_find_with_primary_key(self.ptr, class, key, &raw mut found)
        };

        if !found || object.is_null() {
            return Ok(None);
        }

        value::Object::from_raw(object).map(Some)
    }

    /// Finds the `File` row for a hash, recreating a swept row in the caller's transaction.
    fn find_file_row(
        &self,
        file_class: sys::realm_class_key_t,
        hash: &str,
    ) -> Result<value::Object, RealmError> {
        let raw = value::c_string(hash)?;
        let key = sys::realm_value_t {
            __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                string: sys::realm_string_t {
                    data: raw.as_ptr(),
                    size: hash.len(),
                },
            },
            type_: sys::realm_value_type_RLM_TYPE_STRING,
        };

        let mut found = false;
        // SAFETY: `raw` outlives the call and `file_class` came from this realm.
        let object = unsafe {
            sys::realm_object_find_with_primary_key(self.ptr, file_class, key, &raw mut found)
        };

        if object.is_null() && !found {
            // osu!lazer removes zero-usage File rows on startup. The primary key is the
            // only stored property, so restoring it recreates the original row exactly.
            // SAFETY: the caller holds a write transaction and `key` owns no borrowed data
            // beyond `raw`, which remains live through this call.
            let created =
                unsafe { sys::realm_object_create_with_primary_key(self.ptr, file_class, key) };
            return value::Object::from_raw(created);
        }

        value::Object::from_raw(object)
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
            embedded: is_embedded(info.flags),
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
    use crate::fixture::{SetFixture, synthetic_realm};

    fn restoration() -> Restoration {
        Restoration {
            set_id: [1; 16],
            filename: "Thumbs.db".to_owned(),
            hash: "a".repeat(64),
        }
    }

    /// The one-set library the shared fixture builds for these tests.
    fn synthetic(path: &Path) -> Realm {
        synthetic_realm(
            path,
            &[SetFixture::new(
                [1; 16],
                "Title",
                &[("Thumbs.db", &"a".repeat(64))],
            )],
        )
    }

    #[test]
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "test creates an online asset and its embedded usage in one transaction"
    )]
    fn synthetic_online_asset_keeps_its_reference() {
        let dir = tempfile::tempdir().unwrap();
        let realm = synthetic(&dir.path().join("client.realm"));
        let asset_class = realm.class_key("RealmOnlineAsset").unwrap();
        let asset_file = realm.property_key(asset_class, "File").unwrap();
        let target = realm
            .property_key(realm.class_key("RealmNamedFileUsage").unwrap(), "File")
            .unwrap();
        let file = realm
            .find_file_row(realm.class_key("File").unwrap(), &restoration().hash)
            .unwrap();
        // SAFETY: every object and property belongs to this realm. The transaction commits
        // before the owned object handles are released.
        unsafe {
            assert!(sys::realm_begin_write(realm.ptr));
            let asset =
                value::Object::from_raw(sys::realm_object_create(realm.ptr, asset_class)).unwrap();
            let usage =
                value::Object::from_raw(sys::realm_set_embedded(asset.ptr, asset_file)).unwrap();
            let link = sys::realm_value_t {
                type_: sys::realm_value_type_RLM_TYPE_LINK,
                __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                    link: sys::realm_object_as_link(file.ptr),
                },
            };
            assert!(sys::realm_set_value(usage.ptr, target, link, false));
            assert!(sys::realm_commit(realm.ptr));
        }
        assert_eq!(
            realm.read_library().unwrap().usage_counts[&restoration().hash],
            2
        );
        realm
            .erase_usages(&[Removal {
                set_index: 0,
                file_index: 0,
            }])
            .unwrap();
        assert_eq!(
            realm.read_library().unwrap().usage_counts[&restoration().hash],
            1
        );
    }

    #[test]
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "test simulates lazer deleting an orphaned File row"
    )]
    fn synthetic_restore_recreates_a_swept_file_row() {
        let dir = tempfile::tempdir().unwrap();
        let realm = synthetic(&dir.path().join("client.realm"));
        realm
            .erase_usages(&[Removal {
                set_index: 0,
                file_index: 0,
            }])
            .unwrap();
        let file = realm
            .find_file_row(realm.class_key("File").unwrap(), &restoration().hash)
            .unwrap();
        // SAFETY: delete the known fixture row in a transaction, as lazer's cleanup does.
        unsafe {
            assert!(sys::realm_begin_write(realm.ptr));
            assert!(sys::realm_object_delete(file.ptr));
            assert!(sys::realm_commit(realm.ptr));
        }
        assert_eq!(
            realm.restore_usages(&[restoration()], |_, _| {}).unwrap(),
            1
        );
        assert_eq!(
            realm.restore_usages(&[restoration()], |_, _| {}).unwrap(),
            0
        );
        assert_eq!(
            realm.read_library().unwrap().usage_counts[&restoration().hash],
            1
        );
        assert_eq!(realm.schema_version(), 52);
    }

    #[test]
    fn synthetic_failed_preparation_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let realm = synthetic(&dir.path().join("client.realm"));
        let error = realm.erase_usages_with::<(), RealmError>(|_| {
            Err(RealmError::InvalidPath("test".to_owned()))
        });
        assert!(error.is_err());
        assert_eq!(
            realm.read_library().unwrap().usage_counts[&restoration().hash],
            1
        );
        realm
            .erase_usages(&[Removal {
                set_index: 0,
                file_index: 0,
            }])
            .unwrap();
        assert!(realm.read_library().unwrap().usage_counts.is_empty());
    }

    #[test]
    fn synthetic_restore_rejects_conflicting_content() {
        let dir = tempfile::tempdir().unwrap();
        let realm = synthetic(&dir.path().join("client.realm"));
        let mut entry = restoration();
        entry.hash = "b".repeat(64);
        assert!(realm.restore_usages(&[entry], |_, _| {}).is_err());
        assert_eq!(
            realm.read_library().unwrap().usage_counts[&restoration().hash],
            1
        );
        assert_eq!(
            realm.restore_usages(&[restoration()], |_, _| {}).unwrap(),
            0
        );
    }

    /// Copies the sample database into a scratch directory and returns the copy's path.
    ///
    /// Opening a realm creates `client.realm.lock` and `client.realm.management/` beside it,
    /// even in read-only mode — read-only refers to the database contents, not the directory.
    /// The sample library is reference data we must not alter, so every test works on a copy.
    /// Production code has the same obligation when scanning a live library, and solves it the
    /// same way.
    /// Copies whichever real library is available into `scratch`.
    ///
    /// `ref/client-slim.realm` comes first: every one of these tests copies the file before it
    /// opens it, and the slim one is a fraction of the size. Make it with
    /// `cargo run -p cleaner-realm --features test-support --example slim`.
    fn sample_realm(scratch: &tempfile::TempDir) -> Option<std::path::PathBuf> {
        let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ref");
        let source = ["client-slim.realm", "client.realm"]
            .iter()
            .map(|name| reference.join(name))
            .find(|path| path.is_file())?;

        let copy = scratch.path().join("client.realm");
        std::fs::copy(&source, &copy).expect("failed to copy sample database");
        Some(copy)
    }

    /// Wraps a test body that needs a real library, skipping it when there is none.
    ///
    /// A real library is someone's personal data, so `ref/` is gitignored and never present in
    /// CI. What must run everywhere is covered by the synthetic database in `crate::fixture`
    /// instead; these tests add what only a library osu!lazer itself wrote can show.
    fn with_sample(body: impl FnOnce(&Path)) {
        let scratch = tempfile::tempdir().expect("failed to create scratch dir");
        let Some(path) = sample_realm(&scratch) else {
            tracing::warn!("skipping: no library in ref/");
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
                let sets = realm
                    .read_library()
                    .expect("failed to read library")
                    .beatmap_sets;
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
                .read_library()
                .expect("failed to read library")
                .beatmap_sets
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

            let contents = realm.read_library().expect("failed to read library");
            let counts = contents.usage_counts;
            let sets = contents.beatmap_sets;

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

    /// Detaching and reattaching must leave the database exactly as it started.
    ///
    /// Restoring the bytes without the rows would be worse than useless: nothing would refer
    /// to the restored files, so osu!lazer would delete them again on its next startup.
    #[test]
    fn erasing_and_restoring_round_trips() {
        with_sample(|path| {
            let (set_index, set_id, before) = {
                let realm = Realm::open_read_only(path).expect("failed to open");
                let sets = realm
                    .read_library()
                    .expect("failed to read library")
                    .beatmap_sets;
                let set = sets
                    .iter()
                    .find(|s| s.files.len() >= 3)
                    .expect("no set with three files");
                (
                    set.index,
                    set.id,
                    set.files
                        .iter()
                        .map(|f| (f.filename.clone(), f.hash.clone()))
                        .collect::<Vec<_>>(),
                )
            };

            let removed: Vec<_> = before[..2].to_vec();

            {
                let realm = Realm::open_for_write(path).expect("failed to open for write");
                let sets = realm
                    .read_library()
                    .expect("failed to read library")
                    .beatmap_sets;
                let set = sets
                    .iter()
                    .find(|s| s.index == set_index)
                    .expect("set disappeared");

                let removals: Vec<_> = set.files[..2]
                    .iter()
                    .map(|f| Removal {
                        set_index,
                        file_index: f.index,
                    })
                    .collect();
                realm.erase_usages(&removals).expect("failed to erase");
            }

            {
                let realm = Realm::open_for_write(path).expect("failed to reopen for write");
                let restorations: Vec<_> = removed
                    .iter()
                    .map(|(filename, hash)| Restoration {
                        set_id,
                        filename: filename.clone(),
                        hash: hash.clone(),
                    })
                    .collect();

                assert_eq!(
                    realm
                        .restore_usages(&restorations, |_, _| {})
                        .expect("failed to restore"),
                    2,
                    "both usages should have been reattached"
                );
            }

            let realm = Realm::open_read_only(path).expect("failed to reopen");
            let after: Vec<_> = realm
                .read_library()
                .expect("failed to read library")
                .beatmap_sets
                .into_iter()
                .find(|s| s.index == set_index)
                .expect("set disappeared")
                .files
                .into_iter()
                .map(|f| (f.filename, f.hash))
                .collect();

            // Order within the list is not preserved, because restoring appends.
            let sorted = |mut v: Vec<(String, String)>| {
                v.sort();
                v
            };
            assert_eq!(sorted(after), sorted(before), "the set's files must return");
        });
    }

    /// Restoring twice must not duplicate a usage.
    #[test]
    fn restoring_twice_is_idempotent() {
        with_sample(|path| {
            let realm = Realm::open_for_write(path).expect("failed to open for write");
            let sets = realm
                .read_library()
                .expect("failed to read library")
                .beatmap_sets;
            let set = sets.iter().find(|s| !s.files.is_empty()).expect("no files");

            let restoration = Restoration {
                set_id: set.id,
                filename: set.files[0].filename.clone(),
                hash: set.files[0].hash.clone(),
            };

            // The usage is already present, so nothing should be added.
            assert_eq!(
                realm
                    .restore_usages(std::slice::from_ref(&restoration), |_, _| {})
                    .expect("failed to restore"),
                0
            );
        });
    }

    /// A restoration naming a set that is gone must fail rather than pick a different one.
    ///
    /// Identity is the only way a snapshot names a set. Falling back to a position would
    /// reattach the files to whichever beatmap happens to sit there now.
    #[test]
    fn refuses_to_restore_into_an_unknown_beatmap_set() {
        with_sample(|path| {
            let realm = Realm::open_for_write(path).expect("failed to open for write");
            let set = realm
                .read_library()
                .expect("failed to read library")
                .beatmap_sets
                .into_iter()
                .find(|s| !s.files.is_empty())
                .expect("no set with files");

            let restoration = Restoration {
                set_id: [0; 16],
                filename: set.files[0].filename.clone(),
                hash: set.files[0].hash.clone(),
            };

            let error = realm
                .restore_usages(std::slice::from_ref(&restoration), |_, _| {})
                .expect_err("an unknown set must not restore");
            assert!(
                error.to_string().contains("no longer exists"),
                "unexpected error: {error}"
            );
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
