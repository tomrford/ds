use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use blake2::{Blake2b512, Digest as _};
use thiserror::Error;

use crate::{
    MachineRepository, MachineSyncStore, ObjectId, PackOptions, PendingHeadTransaction,
    ReconcileOperationHeadsError, SyncState, SyncStateError, build_packs,
};

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
            let closure = self
                .repository
                .object_closure_from_heads(vec![pending.new_head], &BTreeSet::new())?;
            self.upload_closure(&closure).await?;
            let accepted = self.transport.transact_heads(&pending).await?;
            apply_heads(&mut state, accepted);
            self.state_store.save_state(&state)?;
            self.state_store.clear_outbox()?;
        }

        self.download_new_packs(&mut state).await?;
        let cloud = self.transport.get_heads().await?;
        self.repository
            .reconcile_operation_heads(&cloud.heads)
            .await?;
        apply_heads(&mut state, cloud.clone());
        self.state_store.save_state(&state)?;

        let closure = self
            .repository
            .object_closure(&state.accepted_heads)
            .await?;
        self.upload_closure(&closure).await?;

        let [new_head] = closure.operation_heads.as_slice() else {
            return Err(SyncEngineError::LocalHeadCount {
                count: closure.operation_heads.len(),
            });
        };
        if state.accepted_heads == BTreeSet::from([*new_head]) {
            return Ok(state);
        }
        let pending = PendingHeadTransaction {
            idempotency_key: request_key(*new_head, &state.accepted_heads),
            new_head: *new_head,
            observed_heads: state.accepted_heads.clone(),
        };
        self.state_store.save_outbox(&pending)?;
        let accepted = self.transport.transact_heads(&pending).await?;
        apply_heads(&mut state, accepted);
        self.state_store.save_state(&state)?;
        self.state_store.clear_outbox()?;
        Ok(state)
    }

    async fn upload_closure(
        &mut self,
        closure: &crate::ObjectClosure,
    ) -> Result<(), SyncEngineError> {
        let built = build_packs(
            closure,
            &BTreeSet::new(),
            &self.packs_root,
            self.pack_options,
        )?;
        for pack in &built.packs {
            let manifest = pack.manifest.encode();
            self.transport.upload_manifest(pack.id, &manifest).await?;
            for position in 0..pack.manifest.chunks().len() {
                let bytes = fs::read(pack.directory.join(format!("{position:08}.chunk"))).map_err(
                    |source| SyncEngineError::ReadPack {
                        path: pack.directory.clone(),
                        source,
                    },
                )?;
                self.transport
                    .upload_chunk(pack.id, position, &bytes)
                    .await?;
            }
            self.transport.install_pack(pack.id).await?;
        }
        Ok(())
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
    Reconcile(#[from] ReconcileOperationHeadsError),
    #[error("failed to read built pack at {path}")]
    ReadPack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("native repository has {count} operation heads after reconciliation")]
    LocalHeadCount { count: usize },
    #[error("invalid cloud pack catalog: {0}")]
    Catalog(&'static str),
}
