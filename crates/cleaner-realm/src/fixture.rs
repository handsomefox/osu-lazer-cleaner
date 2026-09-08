//! Building a scratch osu!lazer database, for tests.
//!
//! The real database is someone's personal library and cannot be committed, so the tests that
//! must run everywhere build one through the C API instead. The schema declared here is the
//! part of osu!lazer's schema 52 this tool reads, and nothing else.
//!
//! Available only under the `test-support` feature, which `cleaner-core` turns on for its own
//! tests. Nothing here is compiled into a release build.

use crate::{CString, Path, Realm, Restoration, last_error, make_noop_scheduler, sys, value};
use std::ffi::CStr;

/// A beatmap set to create, and the files it owns.
pub struct SetFixture {
    /// The set's `Guid` primary key.
    pub id: [u8; 16],
    /// Artist, written to the set's one difficulty.
    pub artist: String,
    /// Title, written to the set's one difficulty.
    pub title: String,
    /// Name of the audio track, which a scan must never treat as a candidate.
    pub audio: String,
    /// Name of the background image.
    pub background: String,
    /// Filename and content hash for each file the set owns.
    pub files: Vec<(String, String)>,
}

impl SetFixture {
    /// A set named `Artist - Title` owning `files`, with no audio or background.
    #[must_use]
    pub fn new(id: [u8; 16], title: &str, files: &[(&str, &str)]) -> Self {
        Self {
            id,
            artist: String::new(),
            title: title.to_owned(),
            audio: String::new(),
            background: String::new(),
            files: files
                .iter()
                .map(|(name, hash)| ((*name).to_owned(), (*hash).to_owned()))
                .collect(),
        }
    }
}

/// One property, with the fields realm-core always wants set.
fn property(name: &'static CStr, kind: sys::realm_property_type_e) -> sys::realm_property_info_t {
    sys::realm_property_info_t {
        name: name.as_ptr(),
        public_name: c"".as_ptr(),
        link_target: c"".as_ptr(),
        link_origin_property_name: c"".as_ptr(),
        type_: kind,
        ..Default::default()
    }
}

fn link(name: &'static CStr, target: &'static CStr, list: bool) -> sys::realm_property_info_t {
    sys::realm_property_info_t {
        link_target: target.as_ptr(),
        collection_type: if list {
            sys::realm_collection_type_RLM_COLLECTION_TYPE_LIST
        } else {
            sys::realm_collection_type_RLM_COLLECTION_TYPE_NONE
        },
        flags: i32::from(!list), // Single object links are nullable.
        ..property(name, sys::realm_property_type_RLM_PROPERTY_TYPE_OBJECT)
    }
}

/// Builds a small library through the C API. No personal data or schema migration is used.
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "test fixture setup owns and releases every C API handle in one sequence"
)]
fn empty_realm(path: &Path) -> Realm {
    let string = sys::realm_property_type_RLM_PROPERTY_TYPE_STRING;
    let boolean = sys::realm_property_type_RLM_PROPERTY_TYPE_BOOL;
    let uuid = sys::realm_property_type_RLM_PROPERTY_TYPE_UUID;
    let properties = [
        vec![sys::realm_property_info_t {
            flags: 2,
            ..property(c"Hash", string)
        }],
        vec![property(c"Filename", string), link(c"File", c"File", false)],
        vec![
            sys::realm_property_info_t {
                flags: 2,
                ..property(c"ID", uuid)
            },
            property(c"DeletePending", boolean),
            property(c"Protected", boolean),
            link(c"Files", c"RealmNamedFileUsage", true),
            link(c"Beatmaps", c"Beatmap", true),
        ],
        vec![link(c"Metadata", c"BeatmapMetadata", false)],
        vec![
            property(c"AudioFile", string),
            property(c"BackgroundFile", string),
            property(c"Artist", string),
            property(c"Title", string),
        ],
        vec![link(c"File", c"RealmNamedFileUsage", false)],
    ];
    let names = [
        c"File",
        c"RealmNamedFileUsage",
        c"BeatmapSet",
        c"Beatmap",
        c"BeatmapMetadata",
        c"RealmOnlineAsset",
    ];
    let classes: Vec<_> = names
        .iter()
        .zip(&properties)
        .enumerate()
        .map(|(index, (name, props))| sys::realm_class_info_t {
            name: name.as_ptr(),
            primary_key: match index {
                0 => c"Hash".as_ptr(),
                2 => c"ID".as_ptr(),
                _ => c"".as_ptr(),
            },
            num_properties: props.len(),
            flags: i32::from(index == 1),
            ..Default::default()
        })
        .collect();
    let mut pointers: Vec<_> = properties.iter().map(Vec::as_ptr).collect();
    let raw_path = CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: all schema names and arrays outlive these calls. This creates a new scratch
    // database with a declared schema. Production opens never use this schema mode.
    unsafe {
        let schema = sys::realm_schema_new(classes.as_ptr(), classes.len(), pointers.as_mut_ptr());
        assert!(!schema.is_null());
        let config = sys::realm_config_new();
        let scheduler = make_noop_scheduler();
        sys::realm_config_set_path(config, raw_path.as_ptr());
        sys::realm_config_set_schema(config, schema);
        sys::realm_config_set_schema_version(config, 52);
        sys::realm_config_set_scheduler(config, scheduler);
        let ptr = sys::realm_open(config);
        sys::realm_release(config.cast());
        sys::realm_release(schema.cast());
        sys::realm_release(scheduler.cast());
        assert!(!ptr.is_null(), "{}", last_error());
        Realm { ptr }
    }
}

/// Gives a set one difficulty carrying its metadata. The caller holds the transaction.
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "one create-and-link sequence; the objects are unusable until it finishes"
)]
fn add_difficulty(realm: &Realm, set: &value::Object, fixture: &SetFixture) {
    let metadata_class = realm.class_key("BeatmapMetadata").unwrap();
    let beatmap_class = realm.class_key("Beatmap").unwrap();
    let metadata_link = realm.property_key(beatmap_class, "Metadata").unwrap();
    let beatmaps = realm
        .property_key(realm.class_key("BeatmapSet").unwrap(), "Beatmaps")
        .unwrap();

    let fields = [
        ("Artist", fixture.artist.as_str()),
        ("Title", fixture.title.as_str()),
        ("AudioFile", fixture.audio.as_str()),
        ("BackgroundFile", fixture.background.as_str()),
    ];

    // SAFETY: every handle below comes from realm-core and is released before returning, and
    // the caller has an open transaction.
    unsafe {
        let metadata = sys::realm_object_create(realm.ptr, metadata_class);
        let metadata = value::Object::from_raw(metadata).unwrap();

        for (name, text) in fields {
            let key = realm.property_key(metadata_class, name).unwrap();
            let raw = value::c_string(text).unwrap();
            let string = sys::realm_value_t {
                __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                    string: sys::realm_string_t {
                        data: raw.as_ptr(),
                        size: text.len(),
                    },
                },
                type_: sys::realm_value_type_RLM_TYPE_STRING,
            };
            assert!(
                sys::realm_set_value(metadata.ptr, key, string, false),
                "{}",
                last_error()
            );
        }

        let beatmap = sys::realm_object_create(realm.ptr, beatmap_class);
        let beatmap = value::Object::from_raw(beatmap).unwrap();
        let link = sys::realm_value_t {
            __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                link: sys::realm_object_as_link(metadata.ptr),
            },
            type_: sys::realm_value_type_RLM_TYPE_LINK,
        };
        assert!(
            sys::realm_set_value(beatmap.ptr, metadata_link, link, false),
            "{}",
            last_error()
        );

        let list = sys::realm_get_list(set.ptr, beatmaps);
        assert!(!list.is_null(), "{}", last_error());
        let entry = sys::realm_value_t {
            __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                link: sys::realm_object_as_link(beatmap.ptr),
            },
            type_: sys::realm_value_type_RLM_TYPE_LINK,
        };
        let mut size = 0;
        assert!(sys::realm_list_size(list, &raw mut size));
        assert!(
            sys::realm_list_insert(list, size, entry),
            "{}",
            last_error()
        );
        sys::realm_release(list.cast());
    }
}
/// Builds a library holding `sets`, at schema 52.
///
/// The files are attached through [`Realm::restore_usages`], which is the same path a restore
/// takes, so the fixture exercises no code the product does not.
///
/// # Panics
///
/// Panics if realm-core refuses the schema or the database cannot be created, both of which
/// mean the fixture itself is wrong.
#[must_use]
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "one create-and-commit sequence per set; the object is unusable until it finishes"
)]
pub fn synthetic_realm(path: &Path, sets: &[SetFixture]) -> Realm {
    let realm = empty_realm(path);
    let class = realm.class_key("BeatmapSet").unwrap();

    for set in sets {
        let key = sys::realm_value_t {
            type_: sys::realm_value_type_RLM_TYPE_UUID,
            __bindgen_anon_1: sys::realm_value__bindgen_ty_1 {
                uuid: sys::realm_uuid_t { bytes: set.id },
            },
        };

        // SAFETY: the realm and class are live, and the object handle is released after commit.
        unsafe {
            assert!(sys::realm_begin_write(realm.ptr));
            let object = sys::realm_object_create_with_primary_key(realm.ptr, class, key);
            assert!(!object.is_null(), "{}", last_error());
            let object = value::Object::from_raw(object).unwrap();
            add_difficulty(&realm, &object, set);
            assert!(sys::realm_commit(realm.ptr));
        }

        let restorations: Vec<_> = set
            .files
            .iter()
            .map(|(filename, hash)| Restoration {
                set_id: set.id,
                filename: filename.clone(),
                hash: hash.clone(),
            })
            .collect();

        if !restorations.is_empty() {
            realm.restore_usages(&restorations, |_, _| {}).unwrap();
        }
    }

    realm
}

/// Deletes every beatmap set past the first `keep`, so a real library can be cut down.
///
/// Deleting from the end keeps the positions of the sets being kept still. Realm removes the
/// `Beatmap` rows a set links to along with it, but leaves behind `File` rows nothing points at
/// any more, which is exactly the shape of a library osu!lazer has not swept yet.
///
/// # Errors
///
/// Returns [`RealmError::Core`] if a set cannot be deleted or the transaction cannot commit.
/// Any failure rolls the whole thing back.
pub fn keep_first_sets(realm: &Realm, keep: usize) -> Result<usize, crate::RealmError> {
    let class = realm.class_key("BeatmapSet")?;
    let total = realm.count(class)?;
    if total <= keep {
        return Ok(0);
    }

    // SAFETY: begins a transaction that every path below either commits or rolls back.
    if !unsafe { sys::realm_begin_write(realm.ptr) } {
        return Err(last_error());
    }

    for index in (keep..total).rev() {
        let object = match realm.object_at(class, index) {
            Ok(object) => object,
            Err(error) => {
                // SAFETY: the transaction opened above is still current.
                unsafe { sys::realm_rollback(realm.ptr) };
                return Err(error);
            }
        };

        // SAFETY: `object` is live for as long as the handle it wraps.
        if !unsafe { sys::realm_object_delete(object.ptr) } {
            let error = last_error();
            drop(object);
            // SAFETY: the transaction opened above is still current.
            unsafe { sys::realm_rollback(realm.ptr) };
            return Err(error);
        }
    }

    // SAFETY: the transaction opened above is still current.
    if !unsafe { sys::realm_commit(realm.ptr) } {
        return Err(last_error());
    }

    Ok(total - keep)
}
