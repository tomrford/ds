use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use jj_lib::lock::FileLock;
use thiserror::Error;

use crate::ObjectId;

const STATE_MAGIC: &[u8; 4] = b"DSSS";
const OUTBOX_MAGIC: &[u8; 4] = b"DSOB";
const FORMAT_VERSION: u16 = 1;
const STATE_HEADER_BYTES: usize = 32;
const OUTBOX_HEADER_BYTES: usize = 96;
const MAX_SYNC_HEADS: usize = 4_096;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncState {
    pub cloud_cursor: u64,
    pub catalog_sequence: u64,
    pub accepted_heads: BTreeSet<ObjectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingHeadTransaction {
    pub idempotency_key: [u8; 16],
    pub new_head: ObjectId,
    pub observed_heads: BTreeSet<ObjectId>,
}

#[derive(Clone, Debug)]
pub struct MachineSyncStore {
    directory: PathBuf,
}

pub struct MachineSyncLock {
    _lock: FileLock,
}

impl MachineSyncStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, SyncStateError> {
        let directory = directory.as_ref().to_path_buf();
        if !directory.exists() {
            fs::create_dir(&directory).map_err(|source| SyncStateError::CreateDirectory {
                path: directory.clone(),
                source,
            })?;
        }
        if let Some(parent) = directory.parent() {
            sync_directory(parent)?;
        }
        sync_directory(&directory)?;
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn lock(&self) -> Result<MachineSyncLock, SyncStateError> {
        let path = self.directory.join("lock");
        let lock =
            FileLock::lock(path.clone()).map_err(|source| SyncStateError::Lock { path, source })?;
        Ok(MachineSyncLock { _lock: lock })
    }

    pub fn load_state(&self) -> Result<SyncState, SyncStateError> {
        let path = self.directory.join("state");
        match read_bounded(&path, STATE_HEADER_BYTES + MAX_SYNC_HEADS * 64) {
            Ok(bytes) => decode_state(&bytes),
            Err(SyncStateError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(SyncState::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_state(&self, state: &SyncState) -> Result<(), SyncStateError> {
        require_head_count(state.accepted_heads.len())?;
        let mut bytes = Vec::with_capacity(STATE_HEADER_BYTES + state.accepted_heads.len() * 64);
        bytes.extend_from_slice(STATE_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&state.cloud_cursor.to_le_bytes());
        bytes.extend_from_slice(&state.catalog_sequence.to_le_bytes());
        bytes.extend_from_slice(&(state.accepted_heads.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        for head in &state.accepted_heads {
            bytes.extend_from_slice(head);
        }
        atomic_write(&self.directory, "state", &bytes)
    }

    pub fn load_outbox(&self) -> Result<Option<PendingHeadTransaction>, SyncStateError> {
        let path = self.directory.join("outbox");
        match read_bounded(&path, OUTBOX_HEADER_BYTES + MAX_SYNC_HEADS * 64) {
            Ok(bytes) => decode_outbox(&bytes).map(Some),
            Err(SyncStateError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_outbox(&self, pending: &PendingHeadTransaction) -> Result<(), SyncStateError> {
        require_head_count(pending.observed_heads.len())?;
        let mut bytes = Vec::with_capacity(OUTBOX_HEADER_BYTES + pending.observed_heads.len() * 64);
        bytes.extend_from_slice(OUTBOX_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&pending.idempotency_key);
        bytes.extend_from_slice(&pending.new_head);
        bytes.extend_from_slice(&(pending.observed_heads.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        for head in &pending.observed_heads {
            bytes.extend_from_slice(head);
        }
        atomic_write(&self.directory, "outbox", &bytes)
    }

    pub fn clear_outbox(&self) -> Result<(), SyncStateError> {
        let path = self.directory.join("outbox");
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(&self.directory),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                sync_directory(&self.directory)
            }
            Err(source) => Err(SyncStateError::Remove { path, source }),
        }
    }
}

fn decode_state(bytes: &[u8]) -> Result<SyncState, SyncStateError> {
    require_header(bytes, STATE_MAGIC, STATE_HEADER_BYTES, "state")?;
    let count = read_u32(bytes, 24) as usize;
    require_zero(&bytes[28..32], "state reserved bytes")?;
    let heads = decode_heads(bytes, STATE_HEADER_BYTES, count, "state")?;
    Ok(SyncState {
        cloud_cursor: read_u64(bytes, 8),
        catalog_sequence: read_u64(bytes, 16),
        accepted_heads: heads,
    })
}

fn decode_outbox(bytes: &[u8]) -> Result<PendingHeadTransaction, SyncStateError> {
    require_header(bytes, OUTBOX_MAGIC, OUTBOX_HEADER_BYTES, "outbox")?;
    let count = read_u32(bytes, 88) as usize;
    require_zero(&bytes[92..96], "outbox reserved bytes")?;
    Ok(PendingHeadTransaction {
        idempotency_key: bytes[8..24].try_into().unwrap(),
        new_head: bytes[24..88].try_into().unwrap(),
        observed_heads: decode_heads(bytes, OUTBOX_HEADER_BYTES, count, "outbox")?,
    })
}

fn require_header(
    bytes: &[u8],
    magic: &[u8; 4],
    header_bytes: usize,
    object: &'static str,
) -> Result<(), SyncStateError> {
    if bytes.len() < header_bytes {
        return Err(SyncStateError::Invalid {
            object,
            reason: "header is truncated",
        });
    }
    if &bytes[..4] != magic {
        return Err(SyncStateError::Invalid {
            object,
            reason: "magic does not match",
        });
    }
    if u16::from_le_bytes(bytes[4..6].try_into().unwrap()) != FORMAT_VERSION {
        return Err(SyncStateError::Invalid {
            object,
            reason: "version is unsupported",
        });
    }
    require_zero(&bytes[6..8], "header reserved bytes")
}

fn decode_heads(
    bytes: &[u8],
    offset: usize,
    count: usize,
    object: &'static str,
) -> Result<BTreeSet<ObjectId>, SyncStateError> {
    require_head_count(count)?;
    let expected = offset
        .checked_add(count * 64)
        .ok_or(SyncStateError::Invalid {
            object,
            reason: "head count overflows",
        })?;
    if bytes.len() != expected {
        return Err(SyncStateError::Invalid {
            object,
            reason: "length does not match head count",
        });
    }
    let mut heads = BTreeSet::new();
    for chunk in bytes[offset..].chunks_exact(64) {
        let head = chunk.try_into().unwrap();
        if !heads.insert(head) {
            return Err(SyncStateError::Invalid {
                object,
                reason: "heads are not unique",
            });
        }
    }
    if !heads.iter().copied().eq(bytes[offset..]
        .chunks_exact(64)
        .map(|chunk| ObjectId::try_from(chunk).unwrap()))
    {
        return Err(SyncStateError::Invalid {
            object,
            reason: "heads are not sorted",
        });
    }
    Ok(heads)
}

fn require_head_count(count: usize) -> Result<(), SyncStateError> {
    if count <= MAX_SYNC_HEADS {
        Ok(())
    } else {
        Err(SyncStateError::TooManyHeads {
            count,
            maximum: MAX_SYNC_HEADS,
        })
    }
}

fn require_zero(bytes: &[u8], reason: &'static str) -> Result<(), SyncStateError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(SyncStateError::Invalid {
            object: "sync data",
            reason,
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, SyncStateError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|source| SyncStateError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| SyncStateError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > limit {
        return Err(SyncStateError::Invalid {
            object: "sync data",
            reason: "file exceeds the size limit",
        });
    }
    Ok(bytes)
}

fn atomic_write(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), SyncStateError> {
    let path = directory.join(name);
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|source| SyncStateError::Write {
            path: path.clone(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| SyncStateError::Write {
            path: path.clone(),
            source,
        })?;
    temporary
        .persist(&path)
        .map_err(|error| SyncStateError::Write {
            path,
            source: error.error,
        })?;
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<(), SyncStateError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| SyncStateError::Write {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug, Error)]
pub enum SyncStateError {
    #[error("failed to create sync directory {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read sync data at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write sync data at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove sync data at {path}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to lock sync state at {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: jj_lib::lock::FileLockError,
    },
    #[error("invalid {object}: {reason}")]
    Invalid {
        object: &'static str,
        reason: &'static str,
    },
    #[error("sync state has {count} heads, exceeding the {maximum}-head limit")]
    TooManyHeads { count: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_outbox_round_trip_and_clear_durably() {
        let temp = tempfile::tempdir().unwrap();
        let store = MachineSyncStore::open(temp.path().join("sync")).unwrap();
        assert_eq!(store.load_state().unwrap(), SyncState::default());
        assert_eq!(store.load_outbox().unwrap(), None);

        let state = SyncState {
            cloud_cursor: 9,
            catalog_sequence: 12,
            accepted_heads: BTreeSet::from([[1; 64], [2; 64]]),
        };
        let pending = PendingHeadTransaction {
            idempotency_key: [3; 16],
            new_head: [4; 64],
            observed_heads: BTreeSet::from([[1; 64], [2; 64]]),
        };
        store.save_state(&state).unwrap();
        store.save_outbox(&pending).unwrap();

        let reopened = MachineSyncStore::open(store.directory()).unwrap();
        assert_eq!(reopened.load_state().unwrap(), state);
        assert_eq!(reopened.load_outbox().unwrap(), Some(pending));
        reopened.clear_outbox().unwrap();
        reopened.clear_outbox().unwrap();
        assert_eq!(reopened.load_outbox().unwrap(), None);
    }

    #[test]
    fn malformed_or_noncanonical_files_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = MachineSyncStore::open(temp.path().join("sync")).unwrap();
        fs::write(store.directory().join("state"), b"DSSS").unwrap();
        assert!(matches!(
            store.load_state(),
            Err(SyncStateError::Invalid {
                object: "state",
                reason: "header is truncated"
            })
        ));

        let pending = PendingHeadTransaction {
            idempotency_key: [3; 16],
            new_head: [4; 64],
            observed_heads: BTreeSet::from([[1; 64], [2; 64]]),
        };
        store.save_outbox(&pending).unwrap();
        let path = store.directory().join("outbox");
        let mut bytes = fs::read(&path).unwrap();
        bytes[OUTBOX_HEADER_BYTES..OUTBOX_HEADER_BYTES + 64].fill(2);
        bytes[OUTBOX_HEADER_BYTES + 64..].fill(1);
        fs::write(path, bytes).unwrap();
        assert!(matches!(
            store.load_outbox(),
            Err(SyncStateError::Invalid {
                object: "outbox",
                reason: "heads are not sorted"
            })
        ));
    }

    #[test]
    fn sidecar_creation_requires_an_existing_parent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing").join("sync");
        assert!(matches!(
            MachineSyncStore::open(&path),
            Err(SyncStateError::CreateDirectory { path: failed, source })
                if failed == path && source.kind() == std::io::ErrorKind::NotFound
        ));
    }
}
