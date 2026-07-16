use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct LockedJsonFile<'a> {
    root: &'a Path,
    file_name: &'static str,
    lock_name: &'static str,
}

impl<'a> LockedJsonFile<'a> {
    pub(crate) fn new(root: &'a Path, file_name: &'static str, lock_name: &'static str) -> Self {
        Self {
            root,
            file_name,
            lock_name,
        }
    }

    pub(crate) fn lock(&self, exclusive: bool) -> Result<File, LockedJsonError> {
        let path = self.root.join(self.lock_name);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockedJsonError::OpenLock {
                path: path.clone(),
                source,
            })?;
        if exclusive {
            lock.lock()
        } else {
            lock.lock_shared()
        }
        .map_err(|source| LockedJsonError::Lock { path, source })?;
        Ok(lock)
    }

    pub(crate) fn read_or_default<T>(&self) -> Result<T, LockedJsonError>
    where
        T: DeserializeOwned + Default,
    {
        let path = self.root.join(self.file_name);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|source| LockedJsonError::Decode { path, source }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(T::default()),
            Err(source) => Err(LockedJsonError::Read { path, source }),
        }
    }

    pub(crate) fn persist<T>(&self, value: &T) -> Result<(), LockedJsonError>
    where
        T: Serialize,
    {
        let path = self.root.join(self.file_name);
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.root.join(format!(
            ".{}.{}.{}.tmp",
            self.file_name,
            std::process::id(),
            sequence
        ));
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| LockedJsonError::Write {
                path: temp_path.clone(),
                source,
            })?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut temp, value).map_err(|source| {
                LockedJsonError::Serialize {
                    path: temp_path.clone(),
                    source,
                }
            })?;
            temp.write_all(b"\n")
                .and_then(|()| temp.sync_all())
                .map_err(|source| LockedJsonError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
            fs::rename(&temp_path, &path).map_err(|source| LockedJsonError::Replace {
                from: temp_path.clone(),
                to: path,
                source,
            })?;
            sync_directory(self.root).map_err(|source| LockedJsonError::SyncParent {
                path: self.root.to_owned(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp_path);
        }
        result
    }
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum LockedJsonError {
    #[error("failed to open lock at {path}")]
    OpenLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to lock {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode {path}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize {path}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to atomically replace {to} from {from}")]
    Replace {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to sync parent directory {path}")]
    SyncParent {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
