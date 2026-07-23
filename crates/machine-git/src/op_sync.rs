use std::collections::BTreeSet;
use std::error::Error;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use blake2::{Blake2b512, Digest as _};
use devspace_kernel_git::ops::{OpObjectKind as KernelOpObjectKind, OpReferenceKind, validate_op};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::repo::Repo as _;
use thiserror::Error;

use crate::{
    MachineGitRepository, OpId, OpReconcileError, OpSyncState, OpSyncStateError, OpSyncStore,
    PendingOpHeadBatch, PendingOpHeadTransaction,
};

const MAX_INVENTORY_KEYS: usize = 4_096;
const MAX_OBJECT_BYTES: u64 = 1024 * 1024;

pub type TransportError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OpObjectKind {
    View,
    Operation,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OpObjectKey {
    pub kind: OpObjectKind,
    pub id: OpId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudOpHeads {
    pub cursor: u64,
    pub heads: BTreeSet<OpId>,
}

#[allow(async_fn_in_trait)]
pub trait OpSyncTransport {
    async fn inventory_op_objects(
        &mut self,
        candidates: &[OpObjectKey],
    ) -> Result<BTreeSet<OpObjectKey>, TransportError>;
    async fn download_op_object(&mut self, key: OpObjectKey) -> Result<Vec<u8>, TransportError>;
    async fn upload_op_object(
        &mut self,
        key: OpObjectKey,
        bytes: &[u8],
    ) -> Result<(), TransportError>;
    async fn get_op_heads(&mut self) -> Result<CloudOpHeads, TransportError>;
    async fn transact_op_heads(
        &mut self,
        pending: &PendingOpHeadTransaction,
    ) -> Result<CloudOpHeads, TransportError>;
}

pub struct OpSyncEngine<'a, T> {
    repository: &'a mut MachineGitRepository,
    state_store: &'a OpSyncStore,
    transport: &'a mut T,
}

impl<'a, T: OpSyncTransport> OpSyncEngine<'a, T> {
    pub fn new(
        repository: &'a mut MachineGitRepository,
        state_store: &'a OpSyncStore,
        transport: &'a mut T,
    ) -> Self {
        Self {
            repository,
            state_store,
            transport,
        }
    }

    pub async fn run(&mut self) -> Result<OpSyncState, OpSyncEngineError> {
        let _lock = self.state_store.lock()?;
        let mut state = self.state_store.load_state()?;
        if let Some(pending) = self.state_store.load_outbox()? {
            let heads = pending
                .entries
                .iter()
                .map(|entry| entry.new_head)
                .collect::<Vec<_>>();
            let closure = self.local_closure(heads, &BTreeSet::new())?;
            self.upload_closure(&closure).await?;
            self.drain_outbox(&mut state, pending).await?;
            return Ok(state);
        }

        let cloud = self.transport.get_op_heads().await?;
        self.download_closures(&cloud.heads).await?;
        if !cloud.heads.is_empty() {
            self.repository
                .reconcile_operation_heads(&cloud.heads)
                .await?;
        }
        apply_heads(&mut state, cloud);
        self.state_store.save_state(&state)?;

        let current_heads = self.repository.current_operation_heads().await?;
        let closure = self.local_closure(current_heads, &state.accepted_heads)?;
        self.upload_closure(&closure).await?;
        let new_heads = closure
            .heads
            .iter()
            .filter(|head| !state.accepted_heads.contains(*head))
            // Every stock store synthesizes this root operation. A virgin
            // repository has no operation object or head transaction to send.
            .filter(|head| **head != [0; 64])
            .copied()
            .collect::<Vec<_>>();
        if new_heads.is_empty() {
            return Ok(state);
        }

        let mut transactions = Vec::with_capacity(new_heads.len());
        for head in new_heads {
            let observed = self.observed_ancestors(head, &state.accepted_heads).await?;
            transactions.push(PendingOpHeadTransaction {
                idempotency_key: request_key(head, &observed),
                new_head: head,
                observed_heads: observed,
            });
        }
        let pending = PendingOpHeadBatch::from_transactions(transactions)?;
        self.state_store.save_outbox(&pending)?;
        self.drain_outbox(&mut state, pending).await?;
        Ok(state)
    }

    async fn drain_outbox(
        &mut self,
        state: &mut OpSyncState,
        mut pending: PendingOpHeadBatch,
    ) -> Result<(), OpSyncEngineError> {
        while let Some(transaction) = pending.first_transaction() {
            let accepted = self.transport.transact_op_heads(&transaction).await?;
            apply_heads(state, accepted);
            self.state_store.save_state(state)?;
            pending.remove_first()?;
            if pending.entries.is_empty() {
                self.state_store.clear_outbox()?;
            } else {
                self.state_store.save_outbox(&pending)?;
            }
        }
        Ok(())
    }

    async fn download_closures(&mut self, heads: &BTreeSet<OpId>) -> Result<(), OpSyncEngineError> {
        let mut pending = heads
            .iter()
            .copied()
            .filter(|id| *id != [0; 64])
            .map(|id| OpObjectKey {
                kind: OpObjectKind::Operation,
                id,
            })
            .collect::<BTreeSet<_>>();
        let mut visited = BTreeSet::new();
        while let Some(key) = pending.pop_first() {
            if !visited.insert(key) {
                continue;
            }
            let path = object_path(self.repository, key);
            let bytes = match read_existing(&path)? {
                Some(bytes) => bytes,
                None => self.transport.download_op_object(key).await?,
            };
            let references = validate_bytes(key, &bytes)?;
            install_object(&path, &bytes)?;
            for (kind, id) in references {
                if kind == OpObjectKind::Operation && id == [0; 64] {
                    continue;
                }
                pending.insert(OpObjectKey { kind, id });
            }
        }
        Ok(())
    }

    fn local_closure(
        &self,
        mut heads: Vec<OpId>,
        accepted_heads: &BTreeSet<OpId>,
    ) -> Result<LocalOpClosure, OpSyncEngineError> {
        heads.sort_unstable();
        heads.dedup();
        let mut pending = heads
            .iter()
            .copied()
            .filter(|id| *id != [0; 64])
            .map(|id| OpObjectKey {
                kind: OpObjectKind::Operation,
                id,
            })
            .collect::<BTreeSet<_>>();
        let mut objects = BTreeSet::new();
        while let Some(key) = pending.pop_first() {
            if objects.contains(&key)
                || (key.kind == OpObjectKind::Operation && accepted_heads.contains(&key.id))
            {
                continue;
            }
            let bytes = read_required(&object_path(self.repository, key))?;
            let references = validate_bytes(key, &bytes)?;
            objects.insert(key);
            for (kind, id) in references {
                if kind == OpObjectKind::Operation && id == [0; 64] {
                    continue;
                }
                pending.insert(OpObjectKey { kind, id });
            }
        }
        Ok(LocalOpClosure {
            heads,
            objects: objects.into_iter().collect(),
        })
    }

    async fn upload_closure(&mut self, closure: &LocalOpClosure) -> Result<(), OpSyncEngineError> {
        let mut installed = BTreeSet::new();
        for page in closure.objects.chunks(MAX_INVENTORY_KEYS) {
            let present = self.transport.inventory_op_objects(page).await?;
            if !present.iter().all(|key| page.binary_search(key).is_ok()) {
                return Err(OpSyncEngineError::Inventory);
            }
            installed.extend(present);
        }
        for key in closure
            .objects
            .iter()
            .filter(|key| !installed.contains(key))
        {
            let bytes = read_required(&object_path(self.repository, *key))?;
            validate_bytes(*key, &bytes)?;
            self.transport.upload_op_object(*key, &bytes).await?;
        }
        Ok(())
    }

    async fn observed_ancestors(
        &self,
        descendant: OpId,
        ancestors: &BTreeSet<OpId>,
    ) -> Result<BTreeSet<OpId>, OpSyncEngineError> {
        let op_store = self.repository.repo().op_store().clone();
        let mut pending = vec![OperationId::new(descendant.to_vec())];
        let mut visited = BTreeSet::new();
        let mut found = BTreeSet::new();
        while let Some(operation_id) = pending.pop() {
            let id: OpId = operation_id.as_bytes().try_into().map_err(|_| {
                OpSyncEngineError::InvalidOperationId(operation_id.as_bytes().len())
            })?;
            if ancestors.contains(&id) {
                found.insert(id);
            }
            if visited.insert(operation_id.clone()) && id != [0; 64] {
                pending.extend(op_store.read_operation(&operation_id).await?.parents);
            }
        }
        Ok(found)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalOpClosure {
    heads: Vec<OpId>,
    objects: Vec<OpObjectKey>,
}

fn validate_bytes(
    key: OpObjectKey,
    bytes: &[u8],
) -> Result<Vec<(OpObjectKind, OpId)>, OpSyncEngineError> {
    let kind = match key.kind {
        OpObjectKind::View => KernelOpObjectKind::View,
        OpObjectKind::Operation => KernelOpObjectKind::Operation,
    };
    let validated = validate_op(kind, bytes)
        .map_err(|source| OpSyncEngineError::InvalidObject { key, source })?;
    if validated.id != key.id {
        return Err(OpSyncEngineError::ObjectIdMismatch { key });
    }
    validated
        .references
        .into_iter()
        .filter_map(|reference| match reference.kind {
            OpReferenceKind::Commit => None,
            OpReferenceKind::View => Some((OpObjectKind::View, reference.id)),
            OpReferenceKind::Operation => Some((OpObjectKind::Operation, reference.id)),
        })
        .map(|(kind, bytes)| {
            let length = bytes.len();
            bytes
                .try_into()
                .map(|id| (kind, id))
                .map_err(|_| OpSyncEngineError::InvalidReferenceId { length })
        })
        .collect()
}

fn object_path(repository: &MachineGitRepository, key: OpObjectKey) -> PathBuf {
    let directory = match key.kind {
        OpObjectKind::View => "views",
        OpObjectKind::Operation => "operations",
    };
    repository
        .operation_store_path()
        .join(directory)
        .join(crate::hex(&key.id))
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, OpSyncEngineError> {
    match File::open(path) {
        Ok(file) => read_bounded(file, path).map(Some),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(OpSyncEngineError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn read_required(path: &Path) -> Result<Vec<u8>, OpSyncEngineError> {
    let file = File::open(path).map_err(|source| OpSyncEngineError::Read {
        path: path.to_owned(),
        source,
    })?;
    read_bounded(file, path)
}

fn read_bounded(file: File, path: &Path) -> Result<Vec<u8>, OpSyncEngineError> {
    let mut bytes = Vec::new();
    file.take(MAX_OBJECT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| OpSyncEngineError::Read {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_OBJECT_BYTES {
        return Err(OpSyncEngineError::ObjectTooLarge {
            path: path.to_owned(),
        });
    }
    Ok(bytes)
}

fn install_object(path: &Path, bytes: &[u8]) -> Result<(), OpSyncEngineError> {
    if let Some(existing) = read_existing(path)? {
        if existing == bytes {
            return Ok(());
        }
        return Err(OpSyncEngineError::ExistingObjectMismatch {
            path: path.to_owned(),
        });
    }
    let directory = path.parent().expect("operation object path has a parent");
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|source| OpSyncEngineError::Write {
            path: path.to_owned(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| OpSyncEngineError::Write {
            path: path.to_owned(),
            source,
        })?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_required(path)? != bytes {
                return Err(OpSyncEngineError::ExistingObjectMismatch {
                    path: path.to_owned(),
                });
            }
        }
        Err(error) => {
            return Err(OpSyncEngineError::Write {
                path: path.to_owned(),
                source: error.error,
            });
        }
    }
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|source| OpSyncEngineError::Write {
            path: path.to_owned(),
            source,
        })
}

fn apply_heads(state: &mut OpSyncState, heads: CloudOpHeads) {
    state.cloud_cursor = heads.cursor;
    state.accepted_heads = heads.heads;
}

fn request_key(new_head: OpId, observed: &BTreeSet<OpId>) -> [u8; 16] {
    let mut hasher = Blake2b512::new();
    hasher.update(new_head);
    for head in observed {
        hasher.update(head);
    }
    hasher.finalize()[..16].try_into().expect("fixed digest")
}

#[derive(Debug, Error)]
pub enum OpSyncEngineError {
    #[error(transparent)]
    State(#[from] OpSyncStateError),
    #[error("operation sync transport failed")]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Reconcile(#[from] OpReconcileError),
    #[error(transparent)]
    ReadOperation(#[from] jj_lib::op_store::OpStoreError),
    #[error("native operation ID must be 64 bytes, got {0}")]
    InvalidOperationId(usize),
    #[error("operation-store reference ID must be 64 bytes, got {length}")]
    InvalidReferenceId { length: usize },
    #[error("operation-store object {key:?} is not canonical")]
    InvalidObject {
        key: OpObjectKey,
        #[source]
        source: devspace_kernel_git::ops::ValidationError,
    },
    #[error("operation-store object {key:?} has the wrong content ID")]
    ObjectIdMismatch { key: OpObjectKey },
    #[error("failed to read operation-store object at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("operation-store object at {path} exceeds the 1 MiB limit")]
    ObjectTooLarge { path: PathBuf },
    #[error("failed to write operation-store object at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing operation-store object at {path} has different bytes")]
    ExistingObjectMismatch { path: PathBuf },
    #[error("cloud returned an operation-store object outside the requested inventory page")]
    Inventory,
}
