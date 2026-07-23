//! Local machine-store proofs for jj-lib's Git backend.
//!
//! Git blobs, trees, and commits are packed from the bare object database.
//! jj's simple operation store and operation-head store remain beside it in
//! `op_store/` and `op_heads/`; they are intentionally not part of this pack
//! format.

mod http_transport;
mod install;
mod object_closure;
mod pack;
mod pack_manifest;
mod store;

pub use http_transport::{
    DownloadedGitPack, GitHttpTransport, GitHttpTransportError, GitInstallReceipt,
    GitPackCatalogEntry, GitPackCatalogPage, GitUploadReceipt,
};
pub use install::{InstalledPack, PackInstallError};
pub use object_closure::{
    MAX_OBJECT_BYTES, MachineObject, ObjectClosure, ObjectClosureError, ObjectKey,
};
pub use pack::{
    BuiltPack, BuiltPacks, Digest, MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_OBJECTS,
    MIN_CHUNK_BYTES, MIN_PACK_BYTES, PackBuildError, PackMetrics, PackOptions, build_packs,
};
pub use pack_manifest::{ChunkEntry, ObjectEntry, PackManifest, PackManifestError};
pub use store::{MachineGitRepository, MachineGitRepositoryError};

pub use devspace_kernel_git::{ObjectKind, Oid};

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
