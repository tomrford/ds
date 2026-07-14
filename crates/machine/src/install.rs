use std::fs::{self, File};
use std::io::{Read as _, Write as _};

use blake2::{Blake2b512, Digest as _};
use devspace_kernel::validate;
use thiserror::Error;

use crate::object_closure::{ObjectId, hex};
use crate::{MAX_OBJECT_BYTES, MachineRepository, ObjectKey, PackManifest, PackManifestError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledPack {
    pub operation_heads: Vec<ObjectId>,
    pub inserted_objects: usize,
    pub existing_objects: usize,
}

impl MachineRepository {
    /// Validates and installs one downloaded immutable pack into jj's stock
    /// simple stores. Operation heads are returned but never published here.
    pub fn install_pack(
        &self,
        expected_id: ObjectId,
        manifest_bytes: &[u8],
        chunks: &[Vec<u8>],
    ) -> Result<InstalledPack, PackInstallError> {
        let actual_id: ObjectId = Blake2b512::digest(manifest_bytes).into();
        if actual_id != expected_id {
            return Err(PackInstallError::ManifestIdMismatch {
                expected: hex(expected_id),
                actual: hex(actual_id),
            });
        }
        let manifest = PackManifest::decode(manifest_bytes)?;
        if chunks.len() != manifest.chunks().len() {
            return Err(PackInstallError::ChunkCount {
                expected: manifest.chunks().len(),
                actual: chunks.len(),
            });
        }

        let mut pack_hash = Blake2b512::new();
        let mut pack_offset = 0_u64;
        let mut object_index = 0_usize;
        let mut object_bytes = Vec::new();
        let mut inserted_objects = 0_usize;
        let mut existing_objects = 0_usize;

        finish_ready_objects(
            self,
            &manifest,
            &mut object_index,
            &mut object_bytes,
            &mut inserted_objects,
            &mut existing_objects,
        )?;
        for (index, (chunk, expected)) in chunks.iter().zip(manifest.chunks()).enumerate() {
            if chunk.len() != expected.length as usize {
                return Err(PackInstallError::ChunkLength {
                    index,
                    expected: expected.length,
                    actual: chunk.len(),
                });
            }
            let actual_hash: ObjectId = Blake2b512::digest(chunk).into();
            if actual_hash != expected.hash {
                return Err(PackInstallError::ChunkHash { index });
            }
            if expected.offset != pack_offset {
                return Err(PackInstallError::ObjectRangesChanged);
            }
            pack_hash.update(chunk);

            let mut chunk_offset = 0_usize;
            while chunk_offset < chunk.len() {
                let object = manifest
                    .objects()
                    .get(object_index)
                    .ok_or(PackInstallError::ObjectRangesChanged)?;
                let written = object_bytes.len() as u64;
                if object.offset + written != pack_offset {
                    return Err(PackInstallError::ObjectRangesChanged);
                }
                let remaining = object
                    .length
                    .checked_sub(written)
                    .and_then(|remaining| usize::try_from(remaining).ok())
                    .ok_or(PackInstallError::ObjectRangesChanged)?;
                let count = remaining.min(chunk.len() - chunk_offset);
                object_bytes.extend_from_slice(&chunk[chunk_offset..chunk_offset + count]);
                chunk_offset += count;
                pack_offset += count as u64;
                finish_ready_objects(
                    self,
                    &manifest,
                    &mut object_index,
                    &mut object_bytes,
                    &mut inserted_objects,
                    &mut existing_objects,
                )?;
            }
        }
        finish_ready_objects(
            self,
            &manifest,
            &mut object_index,
            &mut object_bytes,
            &mut inserted_objects,
            &mut existing_objects,
        )?;
        if pack_offset != manifest.pack_length() || object_index != manifest.objects().len() {
            return Err(PackInstallError::ObjectRangesChanged);
        }
        let actual_pack_hash: ObjectId = pack_hash.finalize().into();
        if actual_pack_hash != manifest.pack_hash() {
            return Err(PackInstallError::PackHashMismatch);
        }

        Ok(InstalledPack {
            operation_heads: manifest.operation_heads().to_vec(),
            inserted_objects,
            existing_objects,
        })
    }

    fn install_object(&self, key: ObjectKey, bytes: &[u8]) -> Result<bool, PackInstallError> {
        let validated = validate(key.kind, bytes)
            .map_err(|source| PackInstallError::ValidateObject { key, source })?;
        if validated.id != key.id {
            return Err(PackInstallError::ObjectIdMismatch {
                key,
                actual: hex(validated.id),
            });
        }

        let path = self.object_path(key);
        if path.exists() {
            require_existing_object(key, &path, bytes)?;
            sync_object_directory(key, &path)?;
            return Ok(false);
        }
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).map_err(|source| PackInstallError::WriteObject {
            key,
            path: path.clone(),
            source,
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| {
            PackInstallError::WriteObject {
                key,
                path: path.clone(),
                source,
            }
        })?;
        temporary
            .write_all(bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| PackInstallError::WriteObject {
                key,
                path: path.clone(),
                source,
            })?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => {
                sync_object_directory(key, &path)?;
                Ok(true)
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                require_existing_object(key, &path, bytes)?;
                sync_object_directory(key, &path)?;
                Ok(false)
            }
            Err(error) => Err(PackInstallError::WriteObject {
                key,
                path,
                source: error.error,
            }),
        }
    }
}

fn sync_object_directory(key: ObjectKey, path: &std::path::Path) -> Result<(), PackInstallError> {
    File::open(path.parent().unwrap())
        .and_then(|directory| directory.sync_all())
        .map_err(|source| PackInstallError::WriteObject {
            key,
            path: path.to_path_buf(),
            source,
        })
}

fn finish_ready_objects(
    repository: &MachineRepository,
    manifest: &PackManifest,
    object_index: &mut usize,
    object_bytes: &mut Vec<u8>,
    inserted_objects: &mut usize,
    existing_objects: &mut usize,
) -> Result<(), PackInstallError> {
    while let Some(object) = manifest.objects().get(*object_index) {
        if object_bytes.len() as u64 != object.length {
            break;
        }
        if repository.install_object(object.key, object_bytes)? {
            *inserted_objects += 1;
        } else {
            *existing_objects += 1;
        }
        *object_index += 1;
        *object_bytes = Vec::with_capacity(
            manifest
                .objects()
                .get(*object_index)
                .map_or(0, |next| next.length as usize),
        );
    }
    Ok(())
}

fn require_existing_object(
    key: ObjectKey,
    path: &std::path::Path,
    expected: &[u8],
) -> Result<(), PackInstallError> {
    let mut bytes = Vec::with_capacity(expected.len());
    File::open(path)
        .map_err(|source| PackInstallError::ReadExistingObject {
            key,
            path: path.to_path_buf(),
            source,
        })?
        .take(MAX_OBJECT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| PackInstallError::ReadExistingObject {
            key,
            path: path.to_path_buf(),
            source,
        })?;
    if bytes == expected {
        Ok(())
    } else {
        Err(PackInstallError::ExistingObjectMismatch {
            key,
            path: path.to_path_buf(),
        })
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
    #[error("pack chunk {index} hash does not match its manifest")]
    ChunkHash { index: usize },
    #[error("pack object ranges changed after manifest validation")]
    ObjectRangesChanged,
    #[error("pack object {key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: devspace_kernel::ValidationError,
    },
    #[error("pack object {key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
    #[error("whole-pack hash does not match its manifest")]
    PackHashMismatch,
    #[error("failed to read existing object {key:?} at {path}")]
    ReadExistingObject {
        key: ObjectKey,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing object {key:?} at {path} differs from downloaded canonical bytes")]
    ExistingObjectMismatch {
        key: ObjectKey,
        path: std::path::PathBuf,
    },
    #[error("failed to install object {key:?} at {path}")]
    WriteObject {
        key: ObjectKey,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}
