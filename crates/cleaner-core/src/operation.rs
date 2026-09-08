//! One filesystem-changing operation per library, including across cleaner processes.

use crate::{Library, SnapshotError, safety};

/// The lock is released when the file closes, including after a panic or process exit.
pub(crate) struct OperationLock {
    _file: std::fs::File,
}

impl OperationLock {
    pub(crate) fn acquire(library: &Library) -> Result<Self, SnapshotError> {
        let directory = library.root().join(".osu-lazer-cleaner");
        let path = directory.join("operation.lock");
        if !safety::is_safe_path(&path, &[library.root()]) {
            return Err(SnapshotError::OutsideLibrary { path });
        }
        let file = (|| {
            std::fs::create_dir_all(&directory)?;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
        })()
        .map_err(|source| SnapshotError::Io {
            action: "opening the library operation lock",
            path: path.clone(),
            source,
        })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(SnapshotError::OperationInProgress),
            Err(std::fs::TryLockError::Error(source)) => Err(SnapshotError::Io {
                action: "locking the library",
                path,
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_operation_is_refused_until_the_first_finishes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("client.realm"), b"stub").unwrap();
        let library = Library::open(directory.path()).unwrap();
        let first = OperationLock::acquire(&library).unwrap();
        assert!(matches!(
            OperationLock::acquire(&library),
            Err(SnapshotError::OperationInProgress)
        ));
        drop(first);
        assert!(OperationLock::acquire(&library).is_ok());
    }
}
