//! The user directories this tool reads and writes.
//!
//! Windows answers where `AppData` lives through `SHGetKnownFolderPath`, not through the
//! environment: `%APPDATA%` is absent from a service's environment and stale after a profile
//! redirection. The environment stays as the fallback. Linux has only the environment, and
//! follows the XDG rule .NET applies to `LocalApplicationData`, which is where osu!lazer keeps
//! its data there. This is the third place after `storage` and `running` where a `cfg` picks a
//! platform and a portable path picks up the rest.

use std::path::{Path, PathBuf};

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

/// `$XDG_DATA_HOME`, or `~/.local/share` when it is unset, empty, or relative.
///
/// .NET resolves `LocalApplicationData` this way off Windows, and osu-framework's
/// `GameHost.UserStoragePaths` puts osu!lazer's data under it. The XDG specification says a
/// relative value is invalid and must be ignored.
#[must_use]
pub fn data_home() -> Option<PathBuf> {
    resolve_data_home(env_path("XDG_DATA_HOME"), env_path("HOME").as_deref())
}

fn resolve_data_home(xdg: Option<PathBuf>, home: Option<&Path>) -> Option<PathBuf> {
    xdg.filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".local/share")))
}

/// This tool's own data directory.
///
/// `%LOCALAPPDATA%\osu-lazer-cleaner` on Windows, `$XDG_DATA_HOME/osu-lazer-cleaner` elsewhere.
/// eframe keeps the window's saved state in the same directory, because `eframe::storage_dir`
/// follows the same rule.
#[must_use]
pub fn app_data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        local_app_data()
    } else {
        data_home()
    };
    base.map(|base| base.join("osu-lazer-cleaner"))
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

    // `/xdg` is absolute only on Unix, and the rule being tested is the XDG one.
    #[cfg(unix)]
    #[test]
    fn an_absolute_xdg_data_home_wins_over_home() {
        assert_eq!(
            resolve_data_home(Some(PathBuf::from("/xdg")), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/xdg"))
        );
        assert_eq!(
            resolve_data_home(None, Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.local/share"))
        );
        assert_eq!(resolve_data_home(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_xdg_data_home_is_ignored() {
        assert_eq!(
            resolve_data_home(Some(PathBuf::from("xdg")), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.local/share"))
        );
        assert_eq!(resolve_data_home(Some(PathBuf::from("xdg")), None), None);
    }
}
