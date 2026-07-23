use std::collections::BTreeMap;

use blake2::{Blake2b512, Digest as _};
use devspace_kernel::{ObjectKind, ReferenceKind, ValidationError, validate};
use gix::objs::{Kind as GitObjectKind, Write as _};
use thiserror::Error;

use crate::object_closure::to_git_kind;
use crate::pack::{Digest, hash};
use crate::{MachineGitRepository, ObjectKey, Oid, PackManifest, PackManifestError, hex};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledPack {
    pub head_commits: Vec<Oid>,
    pub inserted_objects: usize,
    pub existing_objects: usize,
}

impl MachineGitRepository {
    /// Validates and installs an immutable Git-object pack without publishing
    /// any refs or operation heads.
    pub fn install_pack(
        &self,
        expected_id: Digest,
        manifest_bytes: &[u8],
        chunks: &[Vec<u8>],
    ) -> Result<InstalledPack, PackInstallError> {
        let actual_id = hash(manifest_bytes);
        if actual_id != expected_id {
            return Err(PackInstallError::ManifestIdMismatch {
                expected: hex(&expected_id),
                actual: hex(&actual_id),
            });
        }
        let manifest = PackManifest::decode(manifest_bytes)?;
        if chunks.len() != manifest.chunks().len() {
            return Err(PackInstallError::ChunkCount {
                expected: manifest.chunks().len(),
                actual: chunks.len(),
            });
        }
        let mut data = Vec::with_capacity(manifest.pack_length() as usize);
        for (index, (chunk, expected)) in chunks.iter().zip(manifest.chunks()).enumerate() {
            if chunk.len() != expected.length as usize {
                return Err(PackInstallError::ChunkLength {
                    index,
                    expected: expected.length,
                    actual: chunk.len(),
                });
            }
            if hash(chunk) != expected.hash() {
                return Err(PackInstallError::ChunkHash(index));
            }
            if expected.offset != data.len() as u64 {
                return Err(PackInstallError::ObjectRangesChanged);
            }
            data.extend_from_slice(chunk);
        }
        if data.len() as u64 != manifest.pack_length() {
            return Err(PackInstallError::ObjectRangesChanged);
        }
        let actual_pack_hash: Digest = Blake2b512::digest(&data).into();
        if actual_pack_hash != manifest.pack_hash() {
            return Err(PackInstallError::PackHashMismatch);
        }

        let git_repo = self.git_repo();
        let mut prepared = Vec::with_capacity(manifest.objects().len());
        let mut pack_kinds = BTreeMap::new();
        for entry in manifest.objects() {
            let start = usize::try_from(entry.offset())
                .map_err(|_| PackInstallError::ObjectRangesChanged)?;
            let length =
                usize::try_from(entry.length).map_err(|_| PackInstallError::ObjectRangesChanged)?;
            let end = start
                .checked_add(length)
                .ok_or(PackInstallError::ObjectRangesChanged)?;
            let bytes = data
                .get(start..end)
                .ok_or(PackInstallError::ObjectRangesChanged)?;
            let git_id = gix::ObjectId::from_bytes_or_panic(&entry.key.id.0);
            let existing = git_repo.try_find_object(git_id).map_err(|source| {
                PackInstallError::ReadExistingObject {
                    key: entry.key,
                    source: Box::new(source),
                }
            })?;
            if let Some(existing) = &existing
                && (existing.kind != to_git_kind(entry.key.kind) || existing.data != bytes)
            {
                return Err(PackInstallError::ExistingObjectMismatch(entry.key));
            }
            let validated = validate(entry.key.kind, bytes).map_err(|source| {
                PackInstallError::ValidateObject {
                    key: entry.key,
                    source,
                }
            })?;
            if validated.id != entry.key.id {
                return Err(PackInstallError::ObjectIdMismatch {
                    key: entry.key,
                    actual: hex(&validated.id.0),
                });
            }
            pack_kinds.insert(entry.key.id, entry.key.kind);
            prepared.push((entry.key, bytes, validated.references, existing.is_some()));
        }

        for (key, _, references, _) in &prepared {
            for reference in references {
                let Some(expected_kind) = reference_kind(reference.kind) else {
                    continue;
                };
                if let Some(actual_kind) = pack_kinds.get(&reference.id) {
                    if *actual_kind != expected_kind {
                        return Err(PackInstallError::ReferenceKindMismatch {
                            source_key: *key,
                            target: reference.id,
                            expected: expected_kind,
                            actual: *actual_kind,
                        });
                    }
                    continue;
                }
                let target_id = gix::ObjectId::from_bytes_or_panic(&reference.id.0);
                let target = git_repo.try_find_object(target_id).map_err(|source| {
                    PackInstallError::CheckReference {
                        source_key: *key,
                        target: reference.id,
                        source: Box::new(source),
                    }
                })?;
                let Some(target) = target else {
                    return Err(PackInstallError::MissingReference {
                        source_key: *key,
                        target: reference.id,
                    });
                };
                if target.kind != to_git_kind(expected_kind) {
                    return Err(PackInstallError::ReferenceKindMismatch {
                        source_key: *key,
                        target: reference.id,
                        expected: expected_kind,
                        actual: from_git_kind(target.kind),
                    });
                }
            }
        }

        let mut inserted_objects = 0;
        let mut existing_objects = 0;
        for (key, bytes, _, exists) in prepared {
            if exists {
                existing_objects += 1;
                continue;
            }
            let actual = git_repo
                .objects
                .write_buf(to_git_kind(key.kind), bytes)
                .map_err(|source| PackInstallError::WriteObject { key, source })?;
            if actual.as_bytes() != key.id.0 {
                return Err(PackInstallError::ObjectIdMismatch {
                    key,
                    actual: actual.to_hex().to_string(),
                });
            }
            inserted_objects += 1;
        }
        Ok(InstalledPack {
            head_commits: manifest.head_commits().to_vec(),
            inserted_objects,
            existing_objects,
        })
    }
}

fn reference_kind(kind: ReferenceKind) -> Option<ObjectKind> {
    match kind {
        ReferenceKind::Blob | ReferenceKind::Executable | ReferenceKind::Symlink => {
            Some(ObjectKind::Blob)
        }
        ReferenceKind::Tree => Some(ObjectKind::Tree),
        ReferenceKind::Commit => Some(ObjectKind::Commit),
        ReferenceKind::Gitlink => None,
    }
}

fn from_git_kind(kind: GitObjectKind) -> ObjectKind {
    match kind {
        GitObjectKind::Blob => ObjectKind::Blob,
        GitObjectKind::Tree => ObjectKind::Tree,
        GitObjectKind::Commit => ObjectKind::Commit,
        GitObjectKind::Tag => unreachable!("tags are excluded by closure validation"),
    }
}

#[derive(Debug, Error)]
pub enum PackInstallError {
    #[error(transparent)]
    Manifest(#[from] PackManifestError),
    #[error("manifest ID mismatch: expected {expected}, got {actual}")]
    ManifestIdMismatch { expected: String, actual: String },
    #[error("pack has {actual} chunks, expected {expected}")]
    ChunkCount { expected: usize, actual: usize },
    #[error("pack chunk {index} is {actual} bytes, expected {expected}")]
    ChunkLength {
        index: usize,
        expected: u32,
        actual: usize,
    },
    #[error("pack chunk {0} hash does not match its manifest")]
    ChunkHash(usize),
    #[error("pack object ranges changed after manifest validation")]
    ObjectRangesChanged,
    #[error("whole-pack hash does not match its manifest")]
    PackHashMismatch,
    #[error("failed to read existing object {key:?}")]
    ReadExistingObject {
        key: ObjectKey,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("existing object {0:?} differs from downloaded bytes; refusing to clobber")]
    ExistingObjectMismatch(ObjectKey),
    #[error("pack object {key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: ValidationError,
    },
    #[error("pack object {key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
    #[error("object {source_key:?} references missing object {target:?}")]
    MissingReference { source_key: ObjectKey, target: Oid },
    #[error("failed to check reference {target:?} from {source_key:?}")]
    CheckReference {
        source_key: ObjectKey,
        target: Oid,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("object {source_key:?} reference {target:?} expects {expected:?}, found {actual:?}")]
    ReferenceKindMismatch {
        source_key: ObjectKey,
        target: Oid,
        expected: ObjectKind,
        actual: ObjectKind,
    },
    #[error("failed to write Git object {key:?}")]
    WriteObject {
        key: ObjectKey,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
