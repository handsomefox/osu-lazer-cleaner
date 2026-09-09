//! The user directories this tool reads and writes.
//!
//! Windows answers where `AppData` lives through `SHGetKnownFolderPath`, not through the
//! environment: `%APPDATA%` is absent from a service's environment and stale after a profile
//! redirection. The environment stays as the fallback, and every other platform has only the
//! environment, so this is the third place after `storage` and `running` where a `cfg` picks a
//! platform and a portable path picks up the rest.

use std::path::PathBuf;

/// `%APPDATA%`, where osu!lazer keeps its own data.
#[must_use]
pub fn roaming_app_data() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows::known_folder(&windows::FOLDERID_RoamingAppData).or_else(|| env_path("APPDATA"))
    }
    #[cfg(not(windows))]
    {
        env_path("APPDATA")
    }
}

/// `%LOCALAPPDATA%`, where this tool keeps its own data.
#[must_use]
pub fn local_app_data() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows::known_folder(&windows::FOLDERID_LocalAppData).or_else(|| env_path("LOCALAPPDATA"))
    }
    #[cfg(not(windows))]
    {
        env_path("LOCALAPPDATA")
    }
}

/// This tool's own data directory.
///
/// `%LOCALAPPDATA%\osu-lazer-cleaner` on Windows, `~/.local/share/osu-lazer-cleaner` elsewhere,
/// which is where the interface already writes its log while it is being developed on Linux.
#[must_use]
pub fn app_data_dir() -> Option<PathBuf> {
    if let Some(local) = local_app_data() {
        return Some(local.join("osu-lazer-cleaner"));
    }
    env_path("HOME").map(|home| home.join(".local/share/osu-lazer-cleaner"))
}

/// Directory the diagnostics log is written to.
#[must_use]
pub fn logs_dir() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join("logs"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    pub(super) use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_RoamingAppData,
    };
    use windows_sys::core::GUID;

    /// Asks Windows for a known folder, returning `None` when the lookup fails so the caller
    /// can fall back to the environment.
    pub(super) fn known_folder(id: &GUID) -> Option<PathBuf> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: `id` is a known-folder GUID, and `raw` receives an allocation the call owns
        // until `CoTaskMemFree` below releases it.
        let status = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &raw mut raw) };
        if status < 0 || raw.is_null() {
            return None;
        }

        let mut cursor = raw;
        let mut length = 0;
        // SAFETY: a successful call returns a NUL-terminated UTF-16 string, so the scan stops
        // inside the allocation.
        while unsafe { *cursor } != 0 {
            // SAFETY: the terminator has not been reached, so the next unit is still inside
            // the allocation.
            cursor = unsafe { cursor.add(1) };
            length += 1;
        }
        // SAFETY: `length` counts the units before the terminator, all within the allocation.
        let path = String::from_utf16(unsafe { std::slice::from_raw_parts(raw, length) })
            .ok()
            .map(PathBuf::from);
        // SAFETY: `raw` is the allocation returned above. It is no longer read, and this
        // releases it exactly once.
        unsafe { CoTaskMemFree(raw.cast()) };

        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_directory_sits_under_the_data_directory() {
        let Some(data) = app_data_dir() else {
            return;
        };
        assert_eq!(logs_dir().unwrap(), data.join("logs"));
        assert!(data.ends_with("osu-lazer-cleaner"));
    }

    #[test]
    fn an_empty_variable_is_not_a_path() {
        // SAFETY: single-threaded test, and no other test reads this variable.
        unsafe { std::env::set_var("CLEANER_EMPTY_TEST_VAR", "") };
        assert_eq!(env_path("CLEANER_EMPTY_TEST_VAR"), None);
        // SAFETY: as above.
        unsafe { std::env::remove_var("CLEANER_EMPTY_TEST_VAR") };
    }
}
