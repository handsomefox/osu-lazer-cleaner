//! Reading values out of realm objects.

use crate::{RealmError, last_error, sys};
use std::ffi::CString;

/// A borrowed realm object handle.
///
/// Releases the underlying handle on drop.
pub(crate) struct Object {
    pub(crate) ptr: *mut sys::realm_object_t,
}

impl Object {
    /// Wraps a raw pointer, returning the last realm error when it is null.
    pub(crate) fn from_raw(ptr: *mut sys::realm_object_t) -> Result<Self, RealmError> {
        if ptr.is_null() {
            return Err(last_error());
        }
        Ok(Self { ptr })
    }

    /// Reads a property as a string, treating null and non-string values as empty.
    pub(crate) fn string(&self, key: sys::realm_property_key_t) -> Result<String, RealmError> {
        let value = self.value(key)?;
        if value.type_ != sys::realm_value_type_RLM_TYPE_STRING {
            return Ok(String::new());
        }

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "reading the union member and the slice it points at is one operation"
        )]
        // SAFETY: realm-core guarantees the pointer and length describe a valid UTF-8 run
        // owned by the realm, and we copy out of it immediately.
        let bytes = unsafe {
            let string = value.__bindgen_anon_1.string;
            if string.data.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(string.data.cast::<u8>(), string.size)
            }
        };

        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Reads a property as a boolean, treating anything else as false.
    pub(crate) fn boolean(&self, key: sys::realm_property_key_t) -> Result<bool, RealmError> {
        let value = self.value(key)?;
        if value.type_ != sys::realm_value_type_RLM_TYPE_BOOL {
            return Ok(false);
        }

        // SAFETY: the tag says this variant holds a bool.
        Ok(unsafe { value.__bindgen_anon_1.boolean })
    }

    /// Reads a property as a UUID, which realm uses for `Guid` primary keys.
    ///
    /// Returns `None` when the property holds anything else.
    pub(crate) fn uuid(
        &self,
        key: sys::realm_property_key_t,
    ) -> Result<Option<[u8; 16]>, RealmError> {
        let value = self.value(key)?;
        if value.type_ != sys::realm_value_type_RLM_TYPE_UUID {
            return Ok(None);
        }

        // SAFETY: the tag says this variant holds a UUID.
        Ok(Some(unsafe { value.__bindgen_anon_1.uuid }.bytes))
    }

    /// Reads a raw property value.
    fn value(&self, key: sys::realm_property_key_t) -> Result<sys::realm_value_t, RealmError> {
        let mut value = sys::realm_value_t::default();

        // SAFETY: `self.ptr` is live and `key` belongs to its class.
        if !unsafe { sys::realm_get_value(self.ptr, key, &raw mut value) } {
            return Err(last_error());
        }

        Ok(value)
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: the pointer came from realm-core and is released exactly once.
        unsafe { sys::realm_release(self.ptr.cast()) };
    }
}

/// Builds a `CString`, mapping interior NULs to an error rather than panicking.
pub(crate) fn c_string(value: &str) -> Result<CString, RealmError> {
    CString::new(value).map_err(|_| RealmError::InvalidPath(value.to_owned()))
}
