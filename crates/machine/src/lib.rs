//! Local machine-store proofs for jj-lib's Git backend.
//!
//! Git blobs, trees, and commits are packed from the bare object database.
//! jj's simple operation store and operation-head store remain beside it in
//! `op_store/` and `op_heads/`; they are intentionally not part of this pack
//! format.

mod control_plane_client;
mod creation_intent;
mod fsync;
mod git_subprocess;
mod http_client;
mod http_transport;
mod install;
mod journal_flow;
mod lift;
mod locked_json;
mod machine_config;
mod machine_store;
mod object_closure;
mod op_sync;
mod op_sync_state;
mod pack;
mod pack_manifest;
mod projection;
mod store;
mod wire;

pub use control_plane_client::{
    CloudRepository, ControlPlaneClient, ControlPlaneClientError, ControlPlaneRemoteErrorKind,
};
pub use creation_intent::{
    RepositoryCreationIntent, RepositoryCreationIntentError, RepositoryCreationKey,
    RepositoryCreationTarget,
};
pub use fsync::sync_directory;
pub use git_subprocess::{
    FetchError, FetchReport, GitProcessEnvironment, GitProcessMode, LeaseUpdate, PushError,
    PushErrorKind, PushRefReport, PushRefStatus, PushReport, QualifiedRef, QualifiedRefError,
    RemoteHead, RemoteHeadsError, RemoteUrl, fetch, ls_remote_head, ls_remote_heads, push,
};
pub use http_transport::{
    DownloadedGitPack, GitHttpTransport, GitHttpTransportError, GitInstallReceipt,
    GitPackCatalogEntry, GitPackCatalogPage, GitUploadReceipt, PendingProjectionGitBatch,
    PendingProjectionGitRef, ProjectionGitBatchResult, ProjectionGitClaimResult,
    ProjectionGitCursor, ProjectionGitFetchRef, ProjectionGitFetchResult, ProjectionGitMapping,
    ProjectionGitObservation, ProjectionGitReplay, ProjectionGitSnapshot, ProjectionGitState,
    ProjectionGitUpdate, RegisteredGitRemote,
};
pub use install::{InstalledPack, PackInstallError};
pub use journal_flow::{
    FetchFlowResult, JournalFlowError, PushFailpoint, PushFlowResult, PushHead, fetch_with_journal,
    push_with_journal,
};
pub use lift::{Disclosure, LiftError, LiftedCommit, OverlayLiftResult, overlay_lift};
pub use machine_config::{MachineConfig, MachineConfigError, MachineId, SharedSecret};
pub use machine_store::{
    CatalogEntry, CheckoutDestinationGuard, MACHINE_STORE_OVERRIDE, MachineStore,
    MachineStoreError, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    RepositorySyncGuard, StagedRepositoryClone,
};
pub use object_closure::{
    MAX_OBJECT_BYTES, MachineObject, ObjectClosure, ObjectClosureError, ObjectKey,
};
pub use op_sync::{
    CloudOpHeads, OpObjectKey, OpObjectKind, OpSyncEngine, OpSyncEngineError, OpSyncTransport,
    TransportError as OpTransportError,
};
pub use op_sync_state::{
    OpSyncState, OpSyncStateError, OpSyncStore, PendingOpHeadBatch, PendingOpHeadTransaction,
};
pub use pack::{
    BuiltPack, BuiltPacks, Digest, MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_OBJECTS,
    MIN_CHUNK_BYTES, MIN_PACK_BYTES, PackBuildError, PackMetrics, PackOptions, build_packs,
};
pub use pack_manifest::{ChunkEntry, ObjectEntry, PackManifest, PackManifestError};
pub use projection::{
    CommitMapping, HiddenSet, HiddenSetIdentity, ProjectionError, ProjectionMappings,
    ProjectionResult,
};
pub use store::OpReconcileError;
pub use store::{MachineGitRepository, MachineGitRepositoryError};
pub use wire::{LowerHexError, decode_lower_hex, encode_lower_hex};

pub use devspace_kernel::{ObjectKind, Oid};

pub type OpId = [u8; 64];

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
