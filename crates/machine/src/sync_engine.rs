use std::collections::BTreeSet;
use std::error::Error;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use blake2::{Blake2b512, Digest as _};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::repo::Repo as _;
use thiserror::Error;

use crate::{
    MachineRepository, MachineSyncStore, ObjectId, ObjectKey, PackOptions, PendingHeadBatch,
    PendingHeadTransaction, ReconcileOperationHeadsError, SyncState, SyncStateError, build_packs,
};

pub const MAX_OBJECT_INVENTORY_KEYS: usize = 4_096;
const MAX_HEAD_SNAPSHOTS_PER_PASS: usize = 4;
const MAX_PACK_GC_DIRECTORIES: usize = 256;
const MAX_PACK_GC_FILES: usize = 1_025;

pub type TransportError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackCatalogEntry {
    pub sequence: u64,
    pub id: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackCatalogPage {
    pub packs: Vec<PackCatalogEntry>,
    pub next_after: u64,
    pub through: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedPack {
    pub id: ObjectId,
    pub manifest: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudHeads {
    pub cursor: u64,
    pub heads: BTreeSet<ObjectId>,
}

pub type HeadTransactionResult = CloudHeads;

#[allow(async_fn_in_trait)]
pub trait SyncTransport {
    /// Returns the installed subset of one sorted, unique candidate set.
    ///
    /// Installed content-addressed objects are immutable, so positive answers
    /// remain authoritative across concurrent pack installs. A concurrent
    /// false negative can only cause an idempotent re-upload.
    async fn inventory_objects(
        &mut self,
        candidates: &[ObjectKey],
    ) -> Result<BTreeSet<ObjectKey>, TransportError>;
    async fn list_packs(
        &mut self,
        after: u64,
        through: Option<u64>,
    ) -> Result<PackCatalogPage, TransportError>;
    async fn download_pack(&mut self, id: ObjectId) -> Result<DownloadedPack, TransportError>;
    async fn upload_manifest(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), TransportError>;
    async fn upload_chunk(
        &mut self,
        id: ObjectId,
        position: usize,
        bytes: &[u8],
    ) -> Result<(), TransportError>;
    async fn install_pack(&mut self, id: ObjectId) -> Result<(), TransportError>;
    async fn get_heads(&mut self) -> Result<CloudHeads, TransportError>;
    async fn transact_heads(
        &mut self,
        pending: &PendingHeadTransaction,
    ) -> Result<HeadTransactionResult, TransportError>;
}

pub struct SyncEngine<'a, T> {
    repository: &'a mut MachineRepository,
    state_store: &'a MachineSyncStore,
    packs_root: PathBuf,
    transport: &'a mut T,
    pack_options: PackOptions,
}

impl<'a, T: SyncTransport> SyncEngine<'a, T> {
    pub fn new(
        repository: &'a mut MachineRepository,
        state_store: &'a MachineSyncStore,
        packs_root: impl AsRef<Path>,
        transport: &'a mut T,
    ) -> Self {
        Self {
            repository,
            state_store,
            packs_root: packs_root.as_ref().to_path_buf(),
            transport,
            pack_options: PackOptions::default(),
        }
    }

    pub fn with_pack_options(mut self, options: PackOptions) -> Self {
        self.pack_options = options;
        self
    }

    pub async fn run(&mut self) -> Result<SyncState, SyncEngineError> {
        let _lock = self.state_store.lock()?;
        let mut state = self.state_store.load_state()?;
        if let Some(pending) = self.state_store.load_outbox()? {
            let closure = self.repository.object_closure_from_heads(
                pending.entries.iter().map(|entry| entry.new_head).collect(),
                &BTreeSet::new(),
            )?;
            self.upload_closure(&closure).await?;
            self.drain_outbox(&mut state, pending).await?;
            cleanup_local_packs(&self.packs_root)?;
            return Ok(state);
        }

        self.download_new_packs(&mut state).await?;
        let cloud = self.transport.get_heads().await?;
        self.repository
            .reconcile_operation_heads(&cloud.heads)
            .await?;
        apply_heads(&mut state, cloud.clone());
        self.state_store.save_state(&state)?;

        let closure = self.upload_current_closure(&state.accepted_heads).await?;
        let observed_heads = state.accepted_heads.clone();
        let new_heads = closure
            .operation_heads
            .iter()
            .filter(|head| !observed_heads.contains(*head))
            // The implicit root operation exists in every store; a virgin
            // repository (empty operation log) has nothing to transact.
            .filter(|head| **head != [0; 64])
            .copied()
            .collect::<Vec<_>>();
        if new_heads.is_empty() {
            cleanup_local_packs(&self.packs_root)?;
            return Ok(state);
        }
        let mut transactions = Vec::with_capacity(new_heads.len());
        for head in new_heads {
            let observed = self.observed_ancestors(head, &observed_heads).await?;
            transactions.push(PendingHeadTransaction {
                idempotency_key: request_key(head, &observed),
                new_head: head,
                observed_heads: observed,
            });
        }
        // Siblings never include one another in their observed subsets, so
        // sequential transactions can remove only proven cloud ancestors.
        let pending = PendingHeadBatch::from_transactions(transactions)?;
        self.state_store.save_outbox(&pending)?;
        self.drain_outbox(&mut state, pending).await?;
        cleanup_local_packs(&self.packs_root)?;
        Ok(state)
    }

    async fn drain_outbox(
        &mut self,
        state: &mut SyncState,
        mut pending: PendingHeadBatch,
    ) -> Result<(), SyncEngineError> {
        while !pending.entries.is_empty() {
            let transaction = pending.first_transaction().unwrap();
            let accepted = self.transport.transact_heads(&transaction).await?;
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

    async fn upload_current_closure(
        &mut self,
        accepted_heads: &BTreeSet<ObjectId>,
    ) -> Result<crate::ObjectClosure, SyncEngineError> {
        let mut closure = self.repository.object_closure(accepted_heads).await?;
        for snapshot in 0..MAX_HEAD_SNAPSHOTS_PER_PASS {
            self.upload_closure(&closure).await?;
            let current_heads = self.repository.current_operation_heads().await?;
            if current_heads == closure.operation_heads
                || snapshot + 1 == MAX_HEAD_SNAPSHOTS_PER_PASS
            {
                return Ok(closure);
            }
            closure = self
                .repository
                .object_closure_from_heads(current_heads, accepted_heads)?;
        }
        unreachable!()
    }

    async fn observed_ancestors(
        &self,
        descendant: ObjectId,
        ancestors: &BTreeSet<ObjectId>,
    ) -> Result<BTreeSet<ObjectId>, SyncEngineError> {
        let op_store = self.repository.repo().op_store().clone();
        let mut pending = vec![OperationId::new(descendant.to_vec())];
        let mut visited = BTreeSet::new();
        let mut found = BTreeSet::new();
        while let Some(operation_id) = pending.pop() {
            let id: ObjectId = operation_id.as_bytes().try_into().map_err(|_| {
                SyncEngineError::OperationIdLength {
                    length: operation_id.as_bytes().len(),
                }
            })?;
            if ancestors.contains(&id) {
                found.insert(id);
                if found.len() == ancestors.len() {
                    return Ok(found);
                }
            }
            if visited.insert(operation_id.clone()) {
                pending.extend(op_store.read_operation(&operation_id).await?.parents);
            }
        }
        Ok(found)
    }

    async fn upload_closure(
        &mut self,
        closure: &crate::ObjectClosure,
    ) -> Result<(), SyncEngineError> {
        upload_object_closure(closure, &self.packs_root, self.pack_options, self.transport).await
    }

    async fn download_new_packs(&mut self, state: &mut SyncState) -> Result<(), SyncEngineError> {
        let mut after = state.catalog_sequence;
        let mut through = None;
        loop {
            let page = self.transport.list_packs(after, through).await?;
            if let Some(expected) = through {
                if page.through != expected {
                    return Err(SyncEngineError::Catalog("catalog high-water changed"));
                }
            } else {
                through = Some(page.through);
            }
            let mut previous = after;
            for entry in page.packs {
                if entry.sequence <= previous || entry.sequence > page.through {
                    return Err(SyncEngineError::Catalog("catalog entries are not ordered"));
                }
                let pack = self.transport.download_pack(entry.id).await?;
                if pack.id != entry.id {
                    return Err(SyncEngineError::Catalog("downloaded pack ID changed"));
                }
                self.repository
                    .install_pack(pack.id, &pack.manifest, &pack.chunks)?;
                previous = entry.sequence;
            }
            if page.next_after != previous || page.next_after > page.through {
                return Err(SyncEngineError::Catalog("catalog cursor is inconsistent"));
            }
            after = page.next_after;
            if page.has_more && after == state.catalog_sequence {
                return Err(SyncEngineError::Catalog("catalog page made no progress"));
            }
            state.catalog_sequence = after;
            self.state_store.save_state(state)?;
            if !page.has_more {
                break;
            }
        }
        Ok(())
    }
}

/// Makes an already-discovered immutable object closure cloud durable.
///
/// Projection uses this for commit closures which are intentionally not
/// reachable from an operation head. Inventory negotiation and pack upload are
/// identical to the ordinary synchronization path.
pub async fn upload_object_closure<T: SyncTransport>(
    closure: &crate::ObjectClosure,
    packs_root: impl AsRef<Path>,
    pack_options: PackOptions,
    transport: &mut T,
) -> Result<(), SyncEngineError> {
    let mut cloud_objects = BTreeSet::new();
    let candidates = closure
        .objects
        .iter()
        .map(|object| object.key)
        .collect::<Vec<_>>();
    for page in candidates.chunks(MAX_OBJECT_INVENTORY_KEYS) {
        let installed = transport.inventory_objects(page).await?;
        if !installed.iter().all(|key| page.binary_search(key).is_ok()) {
            return Err(SyncEngineError::Inventory(
                "cloud returned an object that was not requested",
            ));
        }
        cloud_objects.extend(installed);
    }
    let built = build_packs(closure, &cloud_objects, packs_root, pack_options)?;
    for pack in &built.packs {
        let manifest = pack.manifest.encode();
        transport.upload_manifest(pack.id, &manifest).await?;
        for position in 0..pack.manifest.chunks().len() {
            let bytes = fs::read(pack.directory.join(format!("{position:08}.chunk"))).map_err(
                |source| SyncEngineError::ReadPack {
                    path: pack.directory.clone(),
                    source,
                },
            )?;
            transport.upload_chunk(pack.id, position, &bytes).await?;
        }
        transport.install_pack(pack.id).await?;
    }
    Ok(())
}

fn cleanup_local_packs(packs_root: &Path) -> Result<(), PackGcError> {
    cleanup_local_packs_with_hook(packs_root, |_| Ok(()))
}

fn cleanup_local_packs_with_hook(
    packs_root: &Path,
    mut after_manifest_removed: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), PackGcError> {
    let entries = match fs::read_dir(packs_root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PackGcError::ReadDirectory {
                path: packs_root.to_path_buf(),
                source,
            });
        }
    };
    let mut directories = entries
        .take(MAX_PACK_GC_DIRECTORIES)
        .map(|entry| {
            entry.map_err(|source| PackGcError::ReadDirectory {
                path: packs_root.to_path_buf(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    directories.sort_unstable_by_key(|entry| entry.file_name());
    for entry in directories
        .into_iter()
        .filter(|entry| is_pack_directory_name(&entry.file_name()))
    {
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|source| PackGcError::ReadDirectory {
                path: path.clone(),
                source,
            })?
            .is_dir()
        {
            return Err(PackGcError::UnexpectedEntry { path });
        }
        remove_pack_directory(packs_root, &path, &mut after_manifest_removed)?;
    }
    Ok(())
}

fn remove_pack_directory(
    packs_root: &Path,
    directory: &Path,
    after_manifest_removed: &mut impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), PackGcError> {
    let manifest = directory.join("manifest.bin");
    match fs::remove_file(&manifest) {
        Ok(()) => sync_gc_directory(directory)?,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PackGcError::Remove {
                path: manifest,
                source,
            });
        }
    }
    after_manifest_removed(directory).map_err(|source| PackGcError::Remove {
        path: directory.to_path_buf(),
        source,
    })?;

    let mut files = fs::read_dir(directory)
        .map_err(|source| PackGcError::ReadDirectory {
            path: directory.to_path_buf(),
            source,
        })?
        .take(MAX_PACK_GC_FILES + 1)
        .map(|entry| {
            entry.map_err(|source| PackGcError::ReadDirectory {
                path: directory.to_path_buf(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if files.len() > MAX_PACK_GC_FILES {
        return Err(PackGcError::TooManyFiles {
            path: directory.to_path_buf(),
            maximum: MAX_PACK_GC_FILES,
        });
    }
    files.sort_unstable_by_key(|entry| entry.file_name());
    for entry in files {
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|source| PackGcError::ReadDirectory {
                path: path.clone(),
                source,
            })?
            .is_file()
            || !is_chunk_name(&entry.file_name())
        {
            return Err(PackGcError::UnexpectedEntry { path });
        }
        fs::remove_file(&path).map_err(|source| PackGcError::Remove { path, source })?;
    }
    sync_gc_directory(directory)?;
    fs::remove_dir(directory).map_err(|source| PackGcError::Remove {
        path: directory.to_path_buf(),
        source,
    })?;
    sync_gc_directory(packs_root)
}

fn is_pack_directory_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    (name.len() == 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
        || name.starts_with(".pack-")
}

fn is_chunk_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.len() == 14
            && name[..8].bytes().all(|byte| byte.is_ascii_digit())
            && &name[8..] == ".chunk"
    })
}

fn sync_gc_directory(path: &Path) -> Result<(), PackGcError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| PackGcError::Sync {
            path: path.to_path_buf(),
            source,
        })
}

fn apply_heads(state: &mut SyncState, heads: CloudHeads) {
    state.cloud_cursor = heads.cursor;
    state.accepted_heads = heads.heads;
}

fn request_key(new_head: ObjectId, observed: &BTreeSet<ObjectId>) -> [u8; 16] {
    let mut hasher = Blake2b512::new();
    hasher.update(new_head);
    for head in observed {
        hasher.update(head);
    }
    let digest = hasher.finalize();
    digest[..16].try_into().unwrap()
}

#[derive(Debug, Error)]
pub enum SyncEngineError {
    #[error(transparent)]
    State(#[from] SyncStateError),
    #[error("sync transport failed")]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Closure(#[from] crate::ObjectClosureError),
    #[error(transparent)]
    BuildPack(#[from] crate::PackBuildError),
    #[error(transparent)]
    InstallPack(#[from] crate::PackInstallError),
    #[error(transparent)]
    PackGc(#[from] PackGcError),
    #[error(transparent)]
    Reconcile(#[from] ReconcileOperationHeadsError),
    #[error("failed to read built pack at {path}")]
    ReadPack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid cloud object inventory: {0}")]
    Inventory(&'static str),
    #[error("native operation ID must be 64 bytes, got {length}")]
    OperationIdLength { length: usize },
    #[error("failed to read native operation ancestry")]
    ReadOperation(#[from] jj_lib::op_store::OpStoreError),
    #[error("invalid cloud pack catalog: {0}")]
    Catalog(&'static str),
}

#[derive(Debug, Error)]
pub enum PackGcError {
    #[error("failed to read local pack directory {path}")]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove local pack path {path}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to sync local pack directory {path}")]
    Sync {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("local pack directory {path} exceeds the {maximum}-file cleanup limit")]
    TooManyFiles { path: PathBuf, maximum: usize },
    #[error("local pack cleanup found an unexpected entry at {path}")]
    UnexpectedEntry { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_gc_recovers_after_a_fault_following_manifest_removal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("packs");
        let pack = root.join("0".repeat(128));
        fs::create_dir_all(&pack).unwrap();
        fs::write(pack.join("manifest.bin"), b"manifest").unwrap();
        fs::write(pack.join("00000000.chunk"), b"chunk").unwrap();

        let error = cleanup_local_packs_with_hook(&root, |_| {
            Err(std::io::Error::other("injected GC fault"))
        })
        .unwrap_err();
        assert!(matches!(error, PackGcError::Remove { .. }));
        assert!(!pack.join("manifest.bin").exists());
        assert!(pack.join("00000000.chunk").exists());

        cleanup_local_packs(&root).unwrap();
        assert!(fs::read_dir(root).unwrap().next().is_none());
    }
}
