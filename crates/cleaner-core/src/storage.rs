//! Finding an osu!lazer data directory and addressing the files inside it.

use crate::error::StorageError;
use std::path::{Path, PathBuf};

/// Name of the Realm database lazer writes.
///
/// From `OsuGameBase.CLIENT_DATABASE_FILENAME`.
pub const DATABASE_FILENAME: &str = "client.realm";

/// Directory holding the content-addressed blobs.
pub const FILES_DIRECTORY: &str = "files";

/// File that redirects to a custom data directory, read by `StorageConfigManager`.
const STORAGE_CONFIG: &str = "storage.ini";

/// Suffixes realm-core owns beside the database. Never move, copy, or delete these.
///
/// The same list appears in lazer's `OsuStorage.IgnoreSuffixes`, where the comment reads
/// "Realm pipe files don't play well with copy operations".
pub const REALM_SIDECAR_SUFFIXES: &[&str] = &[".lock", ".note", ".management"];

/// A located osu!lazer data directory.
#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// Opens the library rooted at `root`, following `storage.ini` if it points elsewhere.
    ///
    /// lazer keeps `storage.ini` in the platform default directory even after the user moves
    /// their data, so the file acts as a permanent redirect.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotALibrary`] if no `client.realm` is found at the resolved
    /// location.
    pub fn open(root: &Path) -> Result<Self, StorageError> {
        let resolved = redirect_target(root).unwrap_or_else(|| root.to_path_buf());

        if !resolved.join(DATABASE_FILENAME).is_file() {
            return Err(StorageError::NotALibrary {
                path: resolved.clone(),
            });
        }

        Ok(Self { root: resolved })
    }

    /// Finds a library in the platform's default location.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] when no default directory holds a library.
    pub fn discover() -> Result<Self, StorageError> {
        let candidates = default_roots();

        for candidate in &candidates {
            if let Ok(library) = Self::open(candidate) {
                return Ok(library);
            }
        }

        Err(StorageError::NotFound { tried: candidates })
    }

    /// The resolved data directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the Realm database.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.root.join(DATABASE_FILENAME)
    }

    /// Path of the blob store.
    #[must_use]
    pub fn files_dir(&self) -> PathBuf {
        self.root.join(FILES_DIRECTORY)
    }

    /// Where this tool keeps its snapshots.
    #[must_use]
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join(".osu-lazer-cleaner").join("snapshots")
    }

    /// Where a blob with the given hash lives.
    ///
    /// The layout is `files/<h[0]>/<h[0..2]>/<hash>`, from
    /// `ModelExtensions.GetStoragePath`. Note the second directory includes the first
    /// character, so hash `abcd...` lands in `files/a/ab/abcd...`.
    #[must_use]
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.files_dir().join(blob_relative_path(hash))
    }
}

/// Builds the `files/`-relative path for a hash.
#[must_use]
pub fn blob_relative_path(hash: &str) -> PathBuf {
    if hash.len() < 2 {
        return PathBuf::from(hash);
    }

    PathBuf::from(&hash[..1]).join(&hash[..2]).join(hash)
}

/// Reads `storage.ini` and returns the directory it points at.
fn redirect_target(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(STORAGE_CONFIG)).ok()?;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        if key.trim().eq_ignore_ascii_case("FullPath") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(PathBuf::from(translate_path(value)));
            }
        }
    }

    None
}

/// Rewrites a Windows path so it resolves when running under WSL.
///
/// `storage.ini` always holds a Windows path, because lazer wrote it. A Linux build reading
/// the same file through `/mnt` needs `E:\Games\osu!lazer` to become `/mnt/e/Games/osu!lazer`.
/// On Windows the path is already correct and passes through unchanged.
fn translate_path(raw: &str) -> String {
    if cfg!(windows) {
        return raw.to_owned();
    }

    let bytes = raw.as_bytes();
    let looks_like_drive = bytes.len() > 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    if !looks_like_drive {
        return raw.to_owned();
    }

    let drive = raw[..1].to_ascii_lowercase();
    let rest = raw[2..].replace('\\', "/");
    format!("/mnt/{drive}{rest}")
}

/// Directories lazer may keep its data in, most likely first.
fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(appdata).join("osu"));
        }
    } else {
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            roots.push(home.join(".local/share/osu"));
            roots.push(home.join("Library/Application Support/osu"));
        }

        // Running under WSL against a Windows install is the common case for this project.
        roots.extend(windows_roots_from_wsl());
    }

    roots
}

/// Finds `%APPDATA%\osu` on mounted Windows drives, for WSL.
fn windows_roots_from_wsl() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let Ok(users) = std::fs::read_dir("/mnt/c/Users") else {
        return roots;
    };

    for entry in users.flatten() {
        let candidate = entry.path().join("AppData/Roaming/osu");
        if candidate.is_dir() {
            roots.push(candidate);
        }
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_path_nests_by_hash_prefix() {
        assert_eq!(
            blob_relative_path("abcdef0123"),
            PathBuf::from("a").join("ab").join("abcdef0123")
        );
    }

    #[test]
    fn opening_a_directory_without_a_database_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Library::open(dir.path()),
            Err(StorageError::NotALibrary { .. })
        ));
    }

    #[test]
    fn storage_ini_redirects_to_another_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("elsewhere");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join(DATABASE_FILENAME), b"not a real database").unwrap();
        std::fs::write(
            dir.path().join(STORAGE_CONFIG),
            format!("FullPath = {}\n", real.display()),
        )
        .unwrap();

        let library = Library::open(dir.path()).unwrap();
        assert_eq!(library.root(), real);
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_drive_paths_translate_for_wsl() {
        assert_eq!(
            translate_path(r"E:\Games\osu!lazer"),
            "/mnt/e/Games/osu!lazer"
        );
        assert_eq!(translate_path("/already/unix"), "/already/unix");
    }
}
