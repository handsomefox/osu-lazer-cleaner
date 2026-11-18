//! Finding an osu!lazer data directory and addressing the files inside it.

use crate::error::StorageError;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Name of the Realm database lazer writes.
///
/// From `OsuGameBase.CLIENT_DATABASE_FILENAME`.
pub const DATABASE_FILENAME: &str = "client.realm";

/// Directory holding the content-addressed blobs.
pub const FILES_DIRECTORY: &str = "files";

/// File that redirects to a custom data directory, read by `StorageConfigManager`.
const STORAGE_CONFIG: &str = "storage.ini";

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

        tracing::info!(
            root = %resolved.display(),
            redirected = resolved != root,
            database_bytes = std::fs::metadata(resolved.join(DATABASE_FILENAME))
                .map_or_default(|meta| meta.len()),
            "library opened"
        );
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
            tracing::debug!(path = %candidate.display(), "no library here");
        }

        tracing::warn!(tried = candidates.len(), "no osu!lazer library found");
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

/// Reports whether a name is a 64-character lowercase hex SHA-256, the only name a blob can have.
///
/// osu!lazer writes hashes in lowercase (`ComputeSHA2Hash`). Uppercase is refused because NTFS
/// ignores case: a stray `files/a/ab/ABCD…` is the same file lazer opens for `abcd…`, and
/// treating it as a separate blob that nothing references would offer that file for removal.
pub(crate) fn is_blob_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reports whether a name is a 64-character hex digest in either case.
///
/// Only for snapshots that already exist. Versions before 1.8 accepted uppercase names, so a
/// snapshot can hold one, and it has to stay restorable and deletable. Nothing new is ever
/// planned under such a name: discovery and plan checks use [`is_blob_hash`].
pub(crate) fn is_hex_digest(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// How long a blob that no usage points at must sit unmodified before it counts as left over.
///
/// osu!lazer writes a blob into `files/` before the transaction that adds its usage commits, so
/// a blob caught between the two looks unreferenced. Ten minutes covers any import.
pub const UNREFERENCED_MIN_AGE: Duration = Duration::from_mins(10);

/// Reports whether the file at `path` was last modified at least [`UNREFERENCED_MIN_AGE`]
/// before `now`.
///
/// A file that cannot be read, or whose modification time is later than `now`, is not settled.
pub(crate) fn is_settled(path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= UNREFERENCED_MIN_AGE)
}

/// Builds the `files/`-relative path for a hash.
#[must_use]
pub fn blob_relative_path(hash: &str) -> PathBuf {
    if !hash.is_ascii() || hash.len() < 2 {
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
                return Some(PathBuf::from(value));
            }
        }
    }

    None
}

/// Directories lazer may keep its data in, most likely first.
fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if cfg!(windows) {
        if let Some(appdata) = crate::folders::roaming_app_data() {
            roots.push(appdata.join("osu"));
        }
    } else {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        roots.extend(unix_roots(
            home.as_deref(),
            crate::folders::data_home().as_deref(),
        ));
    }

    roots
}

/// Where lazer keeps its data on Linux and macOS, given `$HOME` and the XDG data directory.
///
/// osu-framework stores under `LocalApplicationData`, which .NET resolves to
/// `$XDG_DATA_HOME`, so that comes first, with the usual default after it in case the variable
/// was set differently when lazer ran. The Flathub build sees its own `XDG_DATA_HOME` inside
/// `~/.var/app/sh.ppy.osu`.
fn unix_roots(home: Option<&Path>, data_home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(data_home) = data_home {
        roots.push(data_home.join("osu"));
    }

    if let Some(home) = home {
        let default = home.join(".local/share/osu");
        if !roots.contains(&default) {
            roots.push(default);
        }
        roots.push(home.join(".var/app/sh.ppy.osu/data/osu"));
        roots.push(home.join("Library/Application Support/osu"));
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_names_are_full_hex_digests() {
        assert!(is_blob_hash(&"a".repeat(64)));
        assert!(!is_blob_hash(&"a".repeat(63)));
        assert!(!is_blob_hash("IMPORTANT READ ME.txt"));
    }

    #[test]
    fn a_blob_name_in_uppercase_is_not_a_blob() {
        // On NTFS this name is the same file osu!lazer opens for the lowercase hash.
        assert!(!is_blob_hash(&"A".repeat(64)));
        assert!(!is_blob_hash(&format!("{}F", "a".repeat(63))));
        assert!(is_blob_hash(&"0123456789abcdef".repeat(4)));
    }

    /// Writes a file and sets its modification time to `age` before now.
    fn file_aged(path: &Path, age: Duration) {
        std::fs::write(path, b"blob").unwrap();
        // Windows needs a handle opened for writing to change the time.
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn only_a_file_untouched_for_ten_minutes_is_settled() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();

        let old = dir.path().join("old");
        file_aged(&old, UNREFERENCED_MIN_AGE + Duration::from_secs(60));
        assert!(is_settled(&old, now));

        let young = dir.path().join("young");
        file_aged(&young, Duration::from_secs(60));
        assert!(
            !is_settled(&young, now),
            "a file being imported is not settled"
        );

        assert!(
            !is_settled(&old, now - 2 * UNREFERENCED_MIN_AGE),
            "a modification time later than now is not settled"
        );
        assert!(!is_settled(&dir.path().join("missing"), now));
    }

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

    #[test]
    fn the_xdg_data_directory_is_tried_before_the_default() {
        let home = Path::new("/home/me");
        assert_eq!(
            unix_roots(Some(home), Some(Path::new("/data"))),
            [
                PathBuf::from("/data/osu"),
                home.join(".local/share/osu"),
                home.join(".var/app/sh.ppy.osu/data/osu"),
                home.join("Library/Application Support/osu"),
            ]
        );
    }

    #[test]
    fn the_default_data_directory_is_tried_once() {
        let home = Path::new("/home/me");
        assert_eq!(
            unix_roots(Some(home), Some(&home.join(".local/share"))),
            [
                home.join(".local/share/osu"),
                home.join(".var/app/sh.ppy.osu/data/osu"),
                home.join("Library/Application Support/osu"),
            ]
        );
    }

    #[test]
    fn no_home_leaves_only_the_xdg_data_directory() {
        assert_eq!(
            unix_roots(None, Some(Path::new("/data"))),
            [PathBuf::from("/data/osu")]
        );
        let roots = unix_roots(None, None);
        assert!(roots.is_empty(), "nothing to look in: {roots:?}");
    }
}
