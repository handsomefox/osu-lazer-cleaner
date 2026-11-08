//! The user directories osu!lazer keeps its own data under.
//!
//! [`app_folders`] answers where they are: through `SHGetKnownFolderPath` on Windows, with the
//! environment as the fallback, and through the environment elsewhere. This module only says
//! which of them osu!lazer uses. On Linux that is the XDG rule .NET applies to
//! `LocalApplicationData`. This is the third place after `storage` and `running` where a
//! platform decides a path.

use std::path::PathBuf;

/// `%APPDATA%`, where osu!lazer keeps its own data.
#[must_use]
pub fn roaming_app_data() -> Option<PathBuf> {
    app_folders::roaming_app_data()
}

/// `$XDG_DATA_HOME`, or `~/.local/share` when it is unset, empty, or relative.
///
/// .NET resolves `LocalApplicationData` this way off Windows, and osu-framework's
/// `GameHost.UserStoragePaths` puts osu!lazer's data under it.
#[must_use]
pub fn data_home() -> Option<PathBuf> {
    app_folders::data_home()
}
