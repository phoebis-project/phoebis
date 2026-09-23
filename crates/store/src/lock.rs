//! Process-exclusive lock on the data directory, held for the life of the `Store`.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::StoreError;

pub struct ProcessLock {
    // Leaked to get a 'static guard rather than a self-referential struct: `Store::open` runs once
    // per process per data directory, so the one-time leak is bounded by the process lifetime.
    _guard: fd_lock::RwLockWriteGuard<'static, File>,
}

pub fn acquire(dir: &Path) -> Result<ProcessLock, StoreError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join("lock"))?;
    let lock: &'static mut fd_lock::RwLock<File> = Box::leak(Box::new(fd_lock::RwLock::new(file)));
    let guard = lock.try_write().map_err(|error| match error.kind() {
        std::io::ErrorKind::WouldBlock => StoreError::Locked,
        _ => StoreError::Io(error),
    })?;
    Ok(ProcessLock { _guard: guard })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquire_on_the_same_directory_answers_locked() {
        let dir = tempfile::tempdir().unwrap();
        let _first = acquire(dir.path()).unwrap();
        assert!(matches!(acquire(dir.path()), Err(StoreError::Locked)));
    }

    #[test]
    fn an_unwritable_directory_answers_the_io_error_not_locked() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = acquire(dir.path());
        let is_io_error = matches!(result, Err(StoreError::Io(_)));
        // Restored before the assertion so the temp directory can still be cleaned up on either outcome.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_io_error);
    }
}
