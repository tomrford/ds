mod control_plane_client;
mod creation_intent;
mod fsync;
mod git_lift;
mod git_projection;
mod git_subprocess;
mod http_client;
mod http_transport;
mod install;
mod locked_json;
mod machine_config;
mod machine_store;
mod object_closure;
mod pack;
mod pack_manifest;
mod projection_transport;
#[cfg(test)]
mod reconciliation_tests;
mod sync_engine;
mod sync_state;
mod wire;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::default_index::DefaultIndexStore;
use jj_lib::default_submodule_store::DefaultSubmoduleStore;
use jj_lib::op_heads_store::OpHeadsStoreError;
use jj_lib::op_store::OperationId;
use jj_lib::repo::{ReadonlyRepo, RepoInitError, RepoLoader, RepoLoaderError, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::signing::{SignInitError, Signer};
use jj_lib::simple_backend::SimpleBackend;
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::simple_op_store::SimpleOpStore;
use thiserror::Error;

pub use control_plane_client::{
    CloudRepository, ControlPlaneClient, ControlPlaneClientError, ControlPlaneRemoteErrorKind,
};
pub use creation_intent::{
    RepositoryCreationIntent, RepositoryCreationIntentError, RepositoryCreationKey,
    RepositoryCreationTarget,
};
pub use devspace_kernel::ObjectKind;
pub use fsync::sync_directory;
pub use git_lift::{
    FetchedGitRef, GitLiftError, GitSeed, LiftResult, LiftedCommitState, SeedSelection,
    TOMBSTONE_A, TOMBSTONE_B, lift_imported, select_seeds,
};
pub use git_projection::{
    CommitMapping, ExportMappings, ExportResult, GitProjection, HiddenSet, HiddenSetIdentity,
    ImportMappings, ImportResult, MAX_IMPORT_HEADS, MAX_IMPORT_TOTAL_COMMITS,
    MAX_IMPORT_TREE_DEPTH, MAX_IMPORT_TREE_ENTRIES, ProjectionError,
};
pub use git_subprocess::{
    CommandDiagnostic, FetchError, FetchReport, GitOid, GitOidParseError, GitProcessEnvironment,
    GitProcessMode, LeaseUpdate, PushError, PushErrorKind, PushRefReport, PushRefStatus,
    PushReport, QualifiedRef, QualifiedRefError, RemoteHead, RemoteHeadsError, RemoteUrl, fetch,
    ls_remote_head, ls_remote_heads, push,
};
pub use http_transport::HttpTransport;
pub use install::{InstalledPack, PackInstallError};
pub use machine_config::{MachineConfig, MachineConfigError, MachineId, SharedSecret};
pub use machine_store::{
    CatalogEntry, CheckoutDestinationGuard, MACHINE_STORE_OVERRIDE, MachineStore,
    MachineStoreError, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    RepositorySyncGuard, StagedRepositoryClone,
};
pub use object_closure::{
    MAX_OBJECT_BYTES, MachineObject, ObjectClosure, ObjectClosureError, ObjectId, ObjectKey,
};
pub use pack::{
    BuiltPack, BuiltPacks, MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_OBJECTS, MIN_CHUNK_BYTES,
    MIN_PACK_BYTES, PackBuildError, PackMetrics, PackOptions, build_packs,
};
pub use pack_manifest::{ChunkEntry, ObjectEntry, PackManifest, PackManifestError};
pub use projection_transport::{
    FetchReceipt, FetchRef, FetchResult, PendingProjectionBatch, PendingProjectionRef,
    ProjectionBatchResult, ProjectionClaimResult, ProjectionCursor, ProjectionMapping,
    ProjectionObservation, ProjectionReplay, ProjectionSnapshot, ProjectionState, ProjectionUpdate,
    RegisteredRemote,
};
pub use sync_engine::{
    CloudHeads, DownloadedPack, HeadTransactionResult, PackCatalogEntry, PackCatalogPage,
    PackGcError, SyncEngine, SyncEngineError, SyncTransport, TransportError, upload_object_closure,
};
pub use sync_state::{
    MachineSyncLock, MachineSyncStore, PendingHead, PendingHeadBatch, PendingHeadTransaction,
    SyncState, SyncStateError,
};
pub use wire::{LowerHexError, decode_lower_hex, encode_lower_hex};

/// A native jj repository in a machine store.
///
/// Devspace owns replication around this repository, not replacements for its
/// backend, operation store, or operation-head store.
pub struct MachineRepository {
    path: PathBuf,
    repo: Arc<ReadonlyRepo>,
}

impl MachineRepository {
    pub async fn init(
        path: impl AsRef<Path>,
        settings: &UserSettings,
    ) -> Result<Self, MachineRepositoryError> {
        let path = path.as_ref();
        fs::create_dir_all(path).map_err(|source| MachineRepositoryError::CreateRepository {
            path: path.to_path_buf(),
            source,
        })?;

        let signer = Signer::from_settings(settings)?;
        let repo = ReadonlyRepo::init(
            settings,
            path,
            &|_settings, store_path| Ok(Box::new(SimpleBackend::init(store_path))),
            signer,
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .await?;

        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }

    pub async fn open(
        path: impl AsRef<Path>,
        settings: &UserSettings,
    ) -> Result<Self, MachineRepositoryError> {
        let path = path.as_ref();
        require_store_type(path, "store", SimpleBackend::name())?;
        require_store_type(path, "op_store", SimpleOpStore::name())?;
        require_store_type(path, "op_heads", SimpleOpHeadsStore::name())?;
        require_store_type(path, "index", DefaultIndexStore::name())?;
        require_store_type(path, "submodule_store", DefaultSubmoduleStore::name())?;

        let loader = RepoLoader::init_from_file_system(settings, path, &StoreFactories::default())?;
        let repo = loader.load_at_head().await?;
        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo(&self) -> &Arc<ReadonlyRepo> {
        &self.repo
    }

    /// Adds complete cloud operation heads to the stock jj head store and asks
    /// jj to resolve the resulting local divergence.
    pub async fn reconcile_operation_heads(
        &mut self,
        cloud_heads: &BTreeSet<ObjectId>,
    ) -> Result<OperationId, ReconcileOperationHeadsError> {
        self.reconcile_operation_heads_with_hook(cloud_heads, |_, _| Ok(()))
            .await
    }

    async fn reconcile_operation_heads_with_hook(
        &mut self,
        cloud_heads: &BTreeSet<ObjectId>,
        mut before_publish: impl FnMut(usize, &OperationId) -> Result<(), OpHeadsStoreError>,
    ) -> Result<OperationId, ReconcileOperationHeadsError> {
        let closure = self
            .object_closure_from_heads(cloud_heads.iter().copied().collect(), &BTreeSet::new())?;
        self.validate_leaf_objects(&closure)?;

        let op_heads_store = self.repo.op_heads_store().clone();
        let mut publish_error = None;
        {
            let _lock = op_heads_store.lock().await?;
            for (index, head) in cloud_heads.iter().enumerate() {
                let operation_id = OperationId::new(head.to_vec());
                if let Err(error) = before_publish(index, &operation_id) {
                    publish_error = Some(error);
                    break;
                }
                if let Err(error) = op_heads_store.update_op_heads(&[], &operation_id).await {
                    publish_error = Some(error);
                    break;
                }
            }
        }

        let repo = match (publish_error, self.repo.loader().load_at_head().await) {
            (None, Ok(repo)) => repo,
            (None, Err(error)) => return Err(error.into()),
            (Some(source), Ok(repo)) => {
                let recovered_head = repo.op_id().clone();
                self.repo = repo;
                return Err(ReconcileOperationHeadsError::PartialPublication {
                    recovered_head,
                    source,
                });
            }
            (Some(publish), Err(recovery)) => {
                return Err(ReconcileOperationHeadsError::PublicationRecovery {
                    publish,
                    recovery,
                });
            }
        };
        let operation_id = repo.op_id().clone();
        self.repo = repo;
        Ok(operation_id)
    }
}

fn require_store_type(
    root: &Path,
    directory: &'static str,
    expected: &'static str,
) -> Result<(), MachineRepositoryError> {
    let path = root.join(directory).join("type");
    let actual =
        fs::read_to_string(&path).map_err(|source| MachineRepositoryError::ReadStoreType {
            path: path.clone(),
            source,
        })?;
    if actual == expected {
        Ok(())
    } else {
        Err(MachineRepositoryError::UnsupportedStore {
            store: directory,
            expected,
            actual,
        })
    }
}

#[derive(Debug, Error)]
pub enum MachineRepositoryError {
    #[error("failed to create machine repository at {path}")]
    CreateRepository {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read machine repository store type at {path}")]
    ReadStoreType {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("machine repository {store} type must be {expected}, got {actual:?}")]
    UnsupportedStore {
        store: &'static str,
        expected: &'static str,
        actual: String,
    },
    #[error(transparent)]
    Initialize(#[from] RepoInitError),
    #[error(transparent)]
    LoadStores(#[from] jj_lib::repo::StoreLoadError),
    #[error(transparent)]
    LoadRepository(#[from] RepoLoaderError),
    #[error(transparent)]
    Signing(#[from] SignInitError),
}

#[derive(Debug, Error)]
pub enum ReconcileOperationHeadsError {
    #[error("cloud operation closure is incomplete or invalid")]
    ValidateClosure(#[from] ObjectClosureError),
    #[error(transparent)]
    OperationHeads(#[from] OpHeadsStoreError),
    #[error(transparent)]
    LoadRepository(#[from] RepoLoaderError),
    #[error(
        "cloud operation-head publication failed; published heads were reconciled at {recovered_head}"
    )]
    PartialPublication {
        recovered_head: OperationId,
        #[source]
        source: OpHeadsStoreError,
    },
    #[error(
        "cloud operation-head publication failed and repository recovery also failed: {recovery}"
    )]
    PublicationRecovery {
        #[source]
        publish: OpHeadsStoreError,
        recovery: RepoLoaderError,
    },
}
