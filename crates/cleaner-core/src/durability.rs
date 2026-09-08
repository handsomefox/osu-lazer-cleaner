//! Persist file contents and the directory entries that make recovery data reachable.

use std::path::Path;

pub(crate) fn sync_file(path: &Path) -> std::io::Result<()> {
    // Windows requires GENERIC_WRITE for FlushFileBuffers. Unix can flush a read-only handle,
    // which also lets it preserve read-only library files without changing their permissions.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if cfg!(windows) {
        options.write(true);
    }
    options.open(path)?.sync_all()
}

/// Flush every directory from `directory` up to the trusted library root, children first.
pub(crate) fn sync_directories(directory: &Path, root: &Path) -> std::io::Result<()> {
    if !directory.starts_with(root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory is outside the library",
        ));
    }
    for path in directory.ancestors() {
        sync_directory(path)?;
        if path == root {
            return Ok(());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "library root is not an ancestor",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the Unix version, where the flush can fail"
)]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // Windows has no Unix directory-fsync equivalent. Flush linked files through writable
    // handles and publish the snapshot with MOVEFILE_WRITE_THROUGH instead.
    Ok(())
}

/// Rename a complete file or directory. Callers flush Unix parent directories afterwards.
pub(crate) fn rename(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    return std::fs::rename(source, destination);

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        // Canonical parent paths carry the extended-length prefix needed by the Win32 API.
        let wide_path = |path: &Path| -> std::io::Result<Vec<u16>> {
            let parent = path.parent().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing parent")
            })?;
            let name = path.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing filename")
            })?;
            let absolute = parent.canonicalize()?.join(name);
            let mut wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains NUL",
                ));
            }
            wide.push(0);
            Ok(wide)
        };
        let source = wide_path(source)?;
        let destination = wide_path(destination)?;
        // SAFETY: both UTF-16 paths are NUL-terminated and live through the synchronous call.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_replaces_a_complete_file_and_flushes_its_parents() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("nested");
        std::fs::create_dir(&parent).unwrap();
        let source = parent.join("partial");
        let destination = parent.join("backup");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"old").unwrap();
        sync_file(&source).unwrap();
        rename(&source, &destination).unwrap();
        sync_directories(&parent, directory.path()).unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"new");
        assert!(!source.exists());
    }
}
