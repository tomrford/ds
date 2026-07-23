use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use jj_lib::lock::FileLock;
use thiserror::Error;

use crate::OpId;

const STATE_MAGIC: &[u8; 4] = b"DSSS";
const OUTBOX_MAGIC: &[u8; 4] = b"DSOB";
const STATE_FORMAT_VERSION: u16 = 1;
const OUTBOX_FORMAT_VERSION: u16 = 2;
const STATE_HEADER_BYTES: usize = 32;
const OUTBOX_HEADER_BYTES: usize = 32;
const OUTBOX_ENTRY_FIXED_BYTES: usize = 80;
const MAX_SYNC_HEADS: usize = 4_096;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpSyncState {
    pub cloud_cursor: u64,
    pub catalog_sequence: u64,
    pub accepted_heads: BTreeSet<OpId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOpHeadTransaction {
    pub idempotency_key: [u8; 16],
    pub new_head: OpId,
    pub observed_heads: BTreeSet<OpId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOpHead {
    pub idempotency_key: [u8; 16],
    pub new_head: OpId,
    observed_mask: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOpHeadBatch {
    pub entries: Vec<PendingOpHead>,
    observed_heads: BTreeSet<OpId>,
}

impl PendingOpHeadBatch {
    pub fn from_transactions(
        transactions: Vec<PendingOpHeadTransaction>,
    ) -> Result<Self, OpSyncStateError> {
        if transactions.is_empty() {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "transaction batch is empty",
            });
        }
        require_head_count(transactions.len())?;
        let observed_heads = transactions
            .iter()
            .flat_map(|transaction| transaction.observed_heads.iter().copied())
            .collect::<BTreeSet<_>>();
        require_head_count(observed_heads.len())?;
        let entries = transactions
            .into_iter()
            .map(|transaction| PendingOpHead {
                idempotency_key: transaction.idempotency_key,
                new_head: transaction.new_head,
                observed_mask: encode_observed_mask(&observed_heads, &transaction.observed_heads),
            })
            .collect();
        let batch = Self {
            entries,
            observed_heads,
        };
        validate_outbox(&batch)?;
        Ok(batch)
    }

    pub fn first_transaction(&self) -> Option<PendingOpHeadTransaction> {
        (!self.entries.is_empty()).then(|| self.transaction(0))
    }

    pub(crate) fn remove_first(&mut self) -> Result<(), OpSyncStateError> {
        if self.entries.is_empty() {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "transaction batch is empty",
            });
        }
        self.entries.remove(0);
        if self.entries.is_empty() {
            self.observed_heads.clear();
            return Ok(());
        }
        let transactions = (0..self.entries.len())
            .map(|index| self.transaction(index))
            .collect();
        *self = Self::from_transactions(transactions)?;
        Ok(())
    }

    fn transaction(&self, index: usize) -> PendingOpHeadTransaction {
        let entry = &self.entries[index];
        let observed_heads = self
            .observed_heads
            .iter()
            .enumerate()
            .filter(|(index, _)| mask_contains(&entry.observed_mask, *index))
            .map(|(_, head)| *head)
            .collect();
        PendingOpHeadTransaction {
            idempotency_key: entry.idempotency_key,
            new_head: entry.new_head,
            observed_heads,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpSyncStore {
    directory: PathBuf,
}

pub struct OpSyncLock {
    _lock: FileLock,
}

impl OpSyncStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, OpSyncStateError> {
        let directory = directory.as_ref().to_path_buf();
        if !directory.exists() {
            fs::create_dir(&directory).map_err(|source| OpSyncStateError::CreateDirectory {
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

    #[cfg(test)]
    fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn lock(&self) -> Result<OpSyncLock, OpSyncStateError> {
        let path = self.directory.join("lock");
        let lock = FileLock::lock(path.clone())
            .map_err(|source| OpSyncStateError::Lock { path, source })?;
        Ok(OpSyncLock { _lock: lock })
    }

    pub fn load_state(&self) -> Result<OpSyncState, OpSyncStateError> {
        let path = self.directory.join("state");
        match read_bounded(&path, STATE_HEADER_BYTES + MAX_SYNC_HEADS * 64) {
            Ok(bytes) => decode_state(&bytes),
            Err(OpSyncStateError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(OpSyncState::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_state(&self, state: &OpSyncState) -> Result<(), OpSyncStateError> {
        require_head_count(state.accepted_heads.len())?;
        let mut bytes = Vec::with_capacity(STATE_HEADER_BYTES + state.accepted_heads.len() * 64);
        bytes.extend_from_slice(STATE_MAGIC);
        bytes.extend_from_slice(&STATE_FORMAT_VERSION.to_le_bytes());
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

    pub fn load_outbox(&self) -> Result<Option<PendingOpHeadBatch>, OpSyncStateError> {
        let path = self.directory.join("outbox");
        match read_bounded(
            &path,
            OUTBOX_HEADER_BYTES
                + MAX_SYNC_HEADS
                    * (OUTBOX_ENTRY_FIXED_BYTES + observed_mask_bytes(MAX_SYNC_HEADS) + 64),
        ) {
            Ok(bytes) => decode_outbox(&bytes).map(Some),
            Err(OpSyncStateError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_outbox(&self, pending: &PendingOpHeadBatch) -> Result<(), OpSyncStateError> {
        validate_outbox(pending)?;
        let mask_bytes = observed_mask_bytes(pending.observed_heads.len());
        let mut bytes = Vec::with_capacity(
            OUTBOX_HEADER_BYTES
                + pending.entries.len() * (OUTBOX_ENTRY_FIXED_BYTES + mask_bytes)
                + pending.observed_heads.len() * 64,
        );
        bytes.extend_from_slice(OUTBOX_MAGIC);
        bytes.extend_from_slice(&OUTBOX_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&(pending.entries.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(pending.observed_heads.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        for entry in &pending.entries {
            bytes.extend_from_slice(&entry.idempotency_key);
            bytes.extend_from_slice(&entry.new_head);
            bytes.extend_from_slice(&entry.observed_mask);
        }
        for head in &pending.observed_heads {
            bytes.extend_from_slice(head);
        }
        atomic_write(&self.directory, "outbox", &bytes)
    }

    pub(crate) fn clear_outbox(&self) -> Result<(), OpSyncStateError> {
        let path = self.directory.join("outbox");
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(&self.directory),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                sync_directory(&self.directory)
            }
            Err(source) => Err(OpSyncStateError::Remove { path, source }),
        }
    }
}

fn decode_state(bytes: &[u8]) -> Result<OpSyncState, OpSyncStateError> {
    require_header(
        bytes,
        STATE_MAGIC,
        STATE_FORMAT_VERSION,
        STATE_HEADER_BYTES,
        "state",
    )?;
    let count = read_u32(bytes, 24) as usize;
    require_zero(&bytes[28..32], "state reserved bytes")?;
    let heads = decode_heads(bytes, STATE_HEADER_BYTES, count, "state")?;
    Ok(OpSyncState {
        cloud_cursor: read_u64(bytes, 8),
        catalog_sequence: read_u64(bytes, 16),
        accepted_heads: heads,
    })
}

fn decode_outbox(bytes: &[u8]) -> Result<PendingOpHeadBatch, OpSyncStateError> {
    require_header(
        bytes,
        OUTBOX_MAGIC,
        OUTBOX_FORMAT_VERSION,
        OUTBOX_HEADER_BYTES,
        "outbox",
    )?;
    let entry_count = read_u32(bytes, 8) as usize;
    let observed_count = read_u32(bytes, 12) as usize;
    require_head_count(entry_count)?;
    require_head_count(observed_count)?;
    if entry_count == 0 {
        return Err(OpSyncStateError::Invalid {
            object: "outbox",
            reason: "transaction batch is empty",
        });
    }
    require_zero(&bytes[16..32], "outbox reserved bytes")?;
    let mask_bytes = observed_mask_bytes(observed_count);
    let entry_bytes = OUTBOX_ENTRY_FIXED_BYTES + mask_bytes;
    let entries_end = OUTBOX_HEADER_BYTES
        .checked_add(entry_count * entry_bytes)
        .ok_or(OpSyncStateError::Invalid {
            object: "outbox",
            reason: "transaction count overflows",
        })?;
    let expected =
        entries_end
            .checked_add(observed_count * 64)
            .ok_or(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "observed head count overflows",
            })?;
    if bytes.len() != expected {
        return Err(OpSyncStateError::Invalid {
            object: "outbox",
            reason: "length does not match counts",
        });
    }
    let mut entries = Vec::with_capacity(entry_count);
    let mut idempotency_keys = BTreeSet::new();
    let mut new_heads = BTreeSet::new();
    for bytes in bytes[OUTBOX_HEADER_BYTES..entries_end].chunks_exact(entry_bytes) {
        let entry = PendingOpHead {
            idempotency_key: bytes[..16].try_into().unwrap(),
            new_head: bytes[16..80].try_into().unwrap(),
            observed_mask: bytes[80..].to_vec(),
        };
        if entry.new_head == [0; 64] {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "new head is the implicit zero operation",
            });
        }
        if !idempotency_keys.insert(entry.idempotency_key) {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "idempotency keys are not unique",
            });
        }
        if !new_heads.insert(entry.new_head) {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "new heads are not unique",
            });
        }
        entries.push(entry);
    }
    let batch = PendingOpHeadBatch {
        entries,
        observed_heads: decode_heads(bytes, entries_end, observed_count, "outbox")?,
    };
    validate_outbox(&batch)?;
    Ok(batch)
}

fn validate_outbox(pending: &PendingOpHeadBatch) -> Result<(), OpSyncStateError> {
    require_head_count(pending.entries.len())?;
    require_head_count(pending.observed_heads.len())?;
    if pending.entries.is_empty() {
        return Err(OpSyncStateError::Invalid {
            object: "outbox",
            reason: "transaction batch is empty",
        });
    }
    let mask_bytes = observed_mask_bytes(pending.observed_heads.len());
    let mut idempotency_keys = BTreeSet::new();
    let mut new_heads = BTreeSet::new();
    for entry in &pending.entries {
        if entry.new_head == [0; 64] {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "new head is the implicit zero operation",
            });
        }
        if !new_heads.insert(entry.new_head) {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "new heads are not unique",
            });
        }
        if !idempotency_keys.insert(entry.idempotency_key) {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "idempotency keys are not unique",
            });
        }
        if entry.observed_mask.len() != mask_bytes {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "observed mask length does not match head count",
            });
        }
        let remainder = pending.observed_heads.len() % 8;
        if remainder != 0
            && entry
                .observed_mask
                .last()
                .is_some_and(|byte| byte & !((1_u8 << remainder) - 1) != 0)
        {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "observed mask has noncanonical trailing bits",
            });
        }
    }
    for index in 0..pending.observed_heads.len() {
        if !pending
            .entries
            .iter()
            .any(|entry| mask_contains(&entry.observed_mask, index))
        {
            return Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "observed head table contains an unused entry",
            });
        }
    }
    Ok(())
}

fn observed_mask_bytes(head_count: usize) -> usize {
    head_count.div_ceil(8)
}

fn encode_observed_mask(all_heads: &BTreeSet<OpId>, observed_heads: &BTreeSet<OpId>) -> Vec<u8> {
    let mut mask = vec![0; observed_mask_bytes(all_heads.len())];
    for (index, head) in all_heads.iter().enumerate() {
        if observed_heads.contains(head) {
            mask[index / 8] |= 1 << (index % 8);
        }
    }
    mask
}

fn mask_contains(mask: &[u8], index: usize) -> bool {
    mask[index / 8] & (1 << (index % 8)) != 0
}

fn require_header(
    bytes: &[u8],
    magic: &[u8; 4],
    version: u16,
    header_bytes: usize,
    object: &'static str,
) -> Result<(), OpSyncStateError> {
    if bytes.len() < header_bytes {
        return Err(OpSyncStateError::Invalid {
            object,
            reason: "header is truncated",
        });
    }
    if &bytes[..4] != magic {
        return Err(OpSyncStateError::Invalid {
            object,
            reason: "magic does not match",
        });
    }
    if u16::from_le_bytes(bytes[4..6].try_into().unwrap()) != version {
        return Err(OpSyncStateError::Invalid {
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
) -> Result<BTreeSet<OpId>, OpSyncStateError> {
    require_head_count(count)?;
    let expected = offset
        .checked_add(count * 64)
        .ok_or(OpSyncStateError::Invalid {
            object,
            reason: "head count overflows",
        })?;
    if bytes.len() != expected {
        return Err(OpSyncStateError::Invalid {
            object,
            reason: "length does not match head count",
        });
    }
    let mut heads = BTreeSet::new();
    for chunk in bytes[offset..].chunks_exact(64) {
        let head = chunk.try_into().unwrap();
        if !heads.insert(head) {
            return Err(OpSyncStateError::Invalid {
                object,
                reason: "heads are not unique",
            });
        }
    }
    if !heads.iter().copied().eq(bytes[offset..]
        .chunks_exact(64)
        .map(|chunk| OpId::try_from(chunk).unwrap()))
    {
        return Err(OpSyncStateError::Invalid {
            object,
            reason: "heads are not sorted",
        });
    }
    Ok(heads)
}

fn require_head_count(count: usize) -> Result<(), OpSyncStateError> {
    if count <= MAX_SYNC_HEADS {
        Ok(())
    } else {
        Err(OpSyncStateError::TooManyHeads {
            count,
            maximum: MAX_SYNC_HEADS,
        })
    }
}

fn require_zero(bytes: &[u8], reason: &'static str) -> Result<(), OpSyncStateError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(OpSyncStateError::Invalid {
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

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, OpSyncStateError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|source| OpSyncStateError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| OpSyncStateError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > limit {
        return Err(OpSyncStateError::Invalid {
            object: "sync data",
            reason: "file exceeds the size limit",
        });
    }
    Ok(bytes)
}

fn atomic_write(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), OpSyncStateError> {
    let path = directory.join(name);
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|source| OpSyncStateError::Write {
            path: path.clone(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| OpSyncStateError::Write {
            path: path.clone(),
            source,
        })?;
    temporary
        .persist(&path)
        .map_err(|error| OpSyncStateError::Write {
            path,
            source: error.error,
        })?;
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<(), OpSyncStateError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| OpSyncStateError::Write {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug, Error)]
pub enum OpSyncStateError {
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
        let store = OpSyncStore::open(temp.path().join("sync")).unwrap();
        assert_eq!(store.load_state().unwrap(), OpSyncState::default());
        assert_eq!(store.load_outbox().unwrap(), None);

        let state = OpSyncState {
            cloud_cursor: 9,
            catalog_sequence: 12,
            accepted_heads: BTreeSet::from([[1; 64], [2; 64]]),
        };
        let pending = PendingOpHeadBatch::from_transactions(vec![
            PendingOpHeadTransaction {
                idempotency_key: [3; 16],
                new_head: [4; 64],
                observed_heads: BTreeSet::from([[1; 64], [2; 64]]),
            },
            PendingOpHeadTransaction {
                idempotency_key: [5; 16],
                new_head: [6; 64],
                observed_heads: BTreeSet::from([[2; 64]]),
            },
        ])
        .unwrap();
        store.save_state(&state).unwrap();
        store.save_outbox(&pending).unwrap();

        let reopened = OpSyncStore::open(store.directory()).unwrap();
        assert_eq!(reopened.load_state().unwrap(), state);
        assert_eq!(reopened.load_outbox().unwrap(), Some(pending));
        reopened.clear_outbox().unwrap();
        reopened.clear_outbox().unwrap();
        assert_eq!(reopened.load_outbox().unwrap(), None);
    }

    #[test]
    fn malformed_or_noncanonical_files_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = OpSyncStore::open(temp.path().join("sync")).unwrap();
        fs::write(store.directory().join("state"), b"DSSS").unwrap();
        assert!(matches!(
            store.load_state(),
            Err(OpSyncStateError::Invalid {
                object: "state",
                reason: "header is truncated"
            })
        ));

        let pending = PendingOpHeadBatch::from_transactions(vec![
            PendingOpHeadTransaction {
                idempotency_key: [3; 16],
                new_head: [4; 64],
                observed_heads: BTreeSet::from([[1; 64], [2; 64]]),
            },
            PendingOpHeadTransaction {
                idempotency_key: [5; 16],
                new_head: [6; 64],
                observed_heads: BTreeSet::from([[2; 64]]),
            },
        ])
        .unwrap();
        store.save_outbox(&pending).unwrap();
        let path = store.directory().join("outbox");
        let mut bytes = fs::read(&path).unwrap();
        let entry_bytes = OUTBOX_ENTRY_FIXED_BYTES + 1;
        bytes[OUTBOX_HEADER_BYTES + entry_bytes + 16..OUTBOX_HEADER_BYTES + 2 * entry_bytes - 1]
            .fill(4);
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            store.load_outbox(),
            Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "new heads are not unique"
            })
        ));

        store.save_outbox(&pending).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[OUTBOX_HEADER_BYTES + OUTBOX_ENTRY_FIXED_BYTES] |= 0x80;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            store.load_outbox(),
            Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "observed mask has noncanonical trailing bits"
            })
        ));

        store.save_outbox(&pending).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[OUTBOX_HEADER_BYTES + OUTBOX_ENTRY_FIXED_BYTES] &= !1;
        fs::write(path, bytes).unwrap();
        assert!(matches!(
            store.load_outbox(),
            Err(OpSyncStateError::Invalid {
                object: "outbox",
                reason: "observed head table contains an unused entry"
            })
        ));
    }

    #[test]
    fn sidecar_creation_requires_an_existing_parent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing").join("sync");
        assert!(matches!(
            OpSyncStore::open(&path),
            Err(OpSyncStateError::CreateDirectory { path: failed, source })
                if failed == path && source.kind() == std::io::ErrorKind::NotFound
        ));
    }
}
