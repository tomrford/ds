//! File locks: a blocking per-remote lock, a non-blocking store/project
//! mutation lock so a second concurrent invocation fails fast instead of
//! silently queueing.

use std::fs::{self, OpenOptions};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub struct FileLock {
    file: fs::File,
}

pub struct MutationLock {
    _lock: FileLock,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = open_lock_file(path)?;
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(Self { file })
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Best-effort; there is no meaningful recovery path in Drop.
        let _ = self.file.unlock();
    }
}

impl MutationLock {
    /// Serialize project mutations on a lock file kept outside the project
    /// (see `Store::lock_project_mutation`), so checkouts never carry lock
    /// artifacts.
    pub fn acquire(lock_path: &Path, project_dir: &Path) -> Result<Self> {
        let Some(lock) = FileLock::try_acquire(lock_path)? else {
            bail!(
                "another mutation is in progress in {}",
                project_dir.display()
            );
        };
        Ok(Self { _lock: lock })
    }
}

fn open_lock_file(path: &Path) -> Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))
}
