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
    let guard = lock.try_write().map_err(|_| StoreError::Locked)?;
    Ok(ProcessLock { _guard: guard })
}
