use std::collections::BTreeSet;

use blake2::{Blake2b512, Digest as _};
use devspace_kernel_git::{ValidationError, validate};
use thiserror::Error;

use crate::pack_manifest::{ChunkEntry, ObjectEntry, PackManifest, PackManifestError};
use crate::{MachineGitRepository, ObjectClosure, ObjectClosureError, ObjectKey, Oid, hex};

pub type Digest = [u8; 64];

pub const MIN_CHUNK_BYTES: u32 = 64 * 1024;
const DEFAULT_CHUNK_BYTES: u32 = 1024 * 1024;
pub const MAX_CHUNK_BYTES: u32 = 8 * 1024 * 1024;
pub const MIN_PACK_BYTES: u64 = 1024 * 1024;
const DEFAULT_PACK_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PACK_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_PACK_OBJECTS: u32 = 65_536;
pub const MAX_PACK_OBJECTS: u32 = 65_536;
pub(crate) const MAX_PACK_HEADS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackOptions {
    pub chunk_bytes: u32,
    pub pack_bytes: u64,
    pub pack_objects: u32,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            pack_bytes: DEFAULT_PACK_BYTES,
            pack_objects: DEFAULT_PACK_OBJECTS,
        }
    }
}

impl PackOptions {
    fn validate(self) -> Result<(), PackBuildError> {
        if !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&self.chunk_bytes) {
            return Err(PackBuildError::InvalidChunkSize(self.chunk_bytes));
        }
        if !(MIN_PACK_BYTES..=MAX_PACK_BYTES).contains(&self.pack_bytes) {
            return Err(PackBuildError::InvalidPackSize(self.pack_bytes));
        }
        if self.pack_objects == 0 || self.pack_objects > MAX_PACK_OBJECTS {
            return Err(PackBuildError::InvalidPackObjectCount(self.pack_objects));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackMetrics {
    pub discovered_objects: usize,
    pub skipped_known_objects: usize,
    pub packed_objects: usize,
    pub packed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPack {
    pub id: Digest,
    pub manifest: PackManifest,
    pub manifest_bytes: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPacks {
    pub packs: Vec<BuiltPack>,
    pub metrics: PackMetrics,
}

pub fn build_packs(
    repository: &MachineGitRepository,
    closure: &ObjectClosure,
    known_objects: &BTreeSet<ObjectKey>,
    options: PackOptions,
) -> Result<BuiltPacks, PackBuildError> {
    options.validate()?;
    let mut source_objects = closure.objects.iter().collect::<Vec<_>>();
    source_objects.sort_unstable_by_key(|object| object.key);
    if let Some(pair) = source_objects
        .windows(2)
        .find(|pair| pair[0].key == pair[1].key)
    {
        return Err(PackBuildError::DuplicateObject(pair[0].key));
    }
    let skipped_known_objects = source_objects
        .iter()
        .filter(|object| known_objects.contains(&object.key))
        .count();
    source_objects.retain(|object| !known_objects.contains(&object.key));
    if closure.head_commits.len() > MAX_PACK_HEADS {
        return Err(PackBuildError::TooManyHeads(closure.head_commits.len()));
    }

    let mut packs = Vec::new();
    let mut builder = PackBuilder::new(options);
    let mut packed_bytes = 0_u64;
    for object in source_objects {
        let bytes = repository.read_object(object.key)?;
        if bytes.len() as u64 != object.length {
            return Err(PackBuildError::ObjectLengthChanged {
                key: object.key,
                discovered: object.length,
                actual: bytes.len() as u64,
            });
        }
        let validated =
            validate(object.key.kind, &bytes).map_err(|source| PackBuildError::ValidateObject {
                key: object.key,
                source,
            })?;
        if validated.id != object.key.id {
            return Err(PackBuildError::ObjectIdMismatch {
                key: object.key,
                actual: hex(&validated.id.0),
            });
        }
        let length = bytes.len() as u64;
        if length > options.pack_bytes {
            return Err(PackBuildError::ObjectExceedsPackLimit {
                key: object.key,
                length,
                limit: options.pack_bytes,
            });
        }
        if !builder.is_empty() && !builder.can_fit(length) {
            packs.push(builder.finish(&closure.head_commits)?);
            builder = PackBuilder::new(options);
        }
        packed_bytes += length;
        builder.push(object.key, bytes);
    }
    if !builder.is_empty() {
        packs.push(builder.finish(&closure.head_commits)?);
    }
    Ok(BuiltPacks {
        metrics: PackMetrics {
            discovered_objects: closure.objects.len(),
            skipped_known_objects,
            packed_objects: closure.objects.len() - skipped_known_objects,
            packed_bytes,
        },
        packs,
    })
}

struct PackBuilder {
    options: PackOptions,
    length: u64,
    objects: Vec<(ObjectKey, Vec<u8>)>,
}

impl PackBuilder {
    fn new(options: PackOptions) -> Self {
        Self {
            options,
            length: 0,
            objects: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn can_fit(&self, length: u64) -> bool {
        self.objects.len() < self.options.pack_objects as usize
            && self
                .length
                .checked_add(length)
                .is_some_and(|total| total <= self.options.pack_bytes)
    }

    fn push(&mut self, key: ObjectKey, bytes: Vec<u8>) {
        self.length += bytes.len() as u64;
        self.objects.push((key, bytes));
    }

    fn finish(self, head_commits: &[Oid]) -> Result<BuiltPack, PackBuildError> {
        let mut data = Vec::with_capacity(self.length as usize);
        let mut entries = Vec::with_capacity(self.objects.len());
        for (key, bytes) in self.objects {
            let offset = data.len() as u64;
            data.extend_from_slice(&bytes);
            entries.push(ObjectEntry {
                key,
                offset,
                length: bytes.len() as u64,
            });
        }
        let chunks = data
            .chunks(self.options.chunk_bytes as usize)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let mut offset = 0_u64;
        let chunk_entries = chunks
            .iter()
            .map(|chunk| {
                let entry = ChunkEntry {
                    offset,
                    length: chunk.len() as u32,
                    hash: hash(chunk),
                };
                offset += chunk.len() as u64;
                entry
            })
            .collect();
        let manifest = PackManifest::new(
            self.options.chunk_bytes,
            data.len() as u64,
            hash(&data),
            head_commits.to_vec(),
            entries,
            chunk_entries,
        )?;
        let manifest_bytes = manifest.encode();
        let id = hash(&manifest_bytes);
        Ok(BuiltPack {
            id,
            manifest,
            manifest_bytes,
            chunks,
        })
    }
}

pub(crate) fn hash(bytes: &[u8]) -> Digest {
    Blake2b512::digest(bytes).into()
}

#[derive(Debug, Error)]
pub enum PackBuildError {
    #[error(transparent)]
    Manifest(#[from] PackManifestError),
    #[error(transparent)]
    Closure(#[from] ObjectClosureError),
    #[error("invalid chunk size {0}")]
    InvalidChunkSize(u32),
    #[error("invalid pack size {0}")]
    InvalidPackSize(u64),
    #[error("invalid pack object count {0}")]
    InvalidPackObjectCount(u32),
    #[error("pack has {0} heads; maximum is {MAX_PACK_HEADS}")]
    TooManyHeads(usize),
    #[error("object closure contains duplicate {0:?}")]
    DuplicateObject(ObjectKey),
    #[error("object {key:?} changed length from {discovered} to {actual}")]
    ObjectLengthChanged {
        key: ObjectKey,
        discovered: u64,
        actual: u64,
    },
    #[error("object {key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: ValidationError,
    },
    #[error("object {key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
    #[error("object {key:?} is {length} bytes; pack limit is {limit}")]
    ObjectExceedsPackLimit {
        key: ObjectKey,
        length: u64,
        limit: u64,
    },
}
