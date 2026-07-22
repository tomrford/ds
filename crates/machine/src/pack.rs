use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use blake2::{Blake2b512, Digest};
use devspace_kernel::validate;
use thiserror::Error;

use crate::object_closure::ObjectId;
use crate::pack_manifest::{
    ChunkEntry, MAX_MANIFEST_BYTES, ObjectEntry, PackManifest, PackManifestError,
};
use crate::{
    MAX_OBJECT_BYTES, MachineObject, ObjectClosure, ObjectKey, ObjectKind, encode_lower_hex as hex,
};

pub const MIN_CHUNK_BYTES: u32 = 64 * 1024;
pub(crate) const DEFAULT_CHUNK_BYTES: u32 = 1024 * 1024;
pub const MAX_CHUNK_BYTES: u32 = 8 * 1024 * 1024;
pub const MIN_PACK_BYTES: u64 = 1024 * 1024;
const DEFAULT_PACK_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PACK_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_PACK_OBJECTS: u32 = 65_536;
pub const MAX_PACK_OBJECTS: u32 = 65_536;
pub(crate) const MAX_PACK_OPERATION_HEADS: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackMetrics {
    pub discovered_objects: usize,
    pub skipped_known_objects: usize,
    pub packed_objects: usize,
    pub packed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPack {
    pub id: ObjectId,
    pub directory: PathBuf,
    pub manifest: PackManifest,
}

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
        if validate_chunk_bytes(self.chunk_bytes).is_err() {
            return Err(PackBuildError::InvalidChunkSize {
                chunk_bytes: self.chunk_bytes,
                minimum: MIN_CHUNK_BYTES,
                maximum: MAX_CHUNK_BYTES,
            });
        }
        if !(MIN_PACK_BYTES..=MAX_PACK_BYTES).contains(&self.pack_bytes) {
            return Err(PackBuildError::InvalidPackSize {
                pack_bytes: self.pack_bytes,
                minimum: MIN_PACK_BYTES,
                maximum: MAX_PACK_BYTES,
            });
        }
        if self.pack_objects == 0 || self.pack_objects > MAX_PACK_OBJECTS {
            return Err(PackBuildError::InvalidPackObjectCount {
                pack_objects: self.pack_objects,
                maximum: MAX_PACK_OBJECTS,
            });
        }
        Ok(())
    }
}

fn validate_chunk_bytes(chunk_bytes: u32) -> Result<(), ()> {
    if (MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&chunk_bytes) {
        Ok(())
    } else {
        Err(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPacks {
    pub packs: Vec<BuiltPack>,
    pub metrics: PackMetrics,
}

pub fn build_packs(
    closure: &ObjectClosure,
    cloud_objects: &BTreeSet<ObjectKey>,
    packs_root: impl AsRef<Path>,
    options: PackOptions,
) -> Result<BuiltPacks, PackBuildError> {
    options.validate()?;

    let packs_root = packs_root.as_ref();
    fs::create_dir_all(packs_root).map_err(|source| PackBuildError::CreatePackRoot {
        path: packs_root.to_path_buf(),
        source,
    })?;
    let mut source_objects = closure.objects.iter().collect::<Vec<_>>();
    source_objects.sort_unstable_by_key(|object| object.key);
    if let Some(duplicate) = source_objects
        .windows(2)
        .find(|pair| pair[0].key == pair[1].key)
    {
        return Err(PackBuildError::DuplicateObject(duplicate[0].key));
    }
    let skipped_known_objects = source_objects
        .iter()
        .filter(|object| cloud_objects.contains(&object.key))
        .count();
    source_objects.retain(|object| !cloud_objects.contains(&object.key));

    let mut operation_heads = closure.operation_heads.clone();
    operation_heads.sort_unstable();
    operation_heads.dedup();
    if operation_heads.len() > MAX_PACK_OPERATION_HEADS {
        return Err(PackBuildError::TooManyOperationHeads {
            count: operation_heads.len(),
            maximum: MAX_PACK_OPERATION_HEADS,
        });
    }
    let mut packs = Vec::new();
    let mut builder = None;
    let mut packed_bytes = 0_u64;
    for object in source_objects {
        let prepared = prepare_object(object, options.pack_bytes)?;
        let needs_new_pack = builder
            .as_ref()
            .is_some_and(|builder: &PackBuilder| !builder.can_fit(prepared.length(), options));
        if needs_new_pack {
            packs.push(
                builder
                    .take()
                    .unwrap()
                    .finish(packs_root, &operation_heads)?,
            );
        }
        let builder = builder.get_or_insert(PackBuilder::new(packs_root, options)?);
        packed_bytes += builder.write(prepared)?;
    }
    if let Some(builder) = builder {
        packs.push(builder.finish(packs_root, &operation_heads)?);
    }

    Ok(BuiltPacks {
        packs,
        metrics: PackMetrics {
            discovered_objects: closure.objects.len(),
            skipped_known_objects,
            packed_objects: closure.objects.len() - skipped_known_objects,
            packed_bytes,
        },
    })
}

struct PackBuilder {
    staging: tempfile::TempDir,
    writer: ChunkWriter,
    objects: Vec<ObjectEntry>,
    chunk_bytes: u32,
}

impl PackBuilder {
    fn new(packs_root: &Path, options: PackOptions) -> Result<Self, PackBuildError> {
        let staging = tempfile::Builder::new()
            .prefix(".pack-")
            .tempdir_in(packs_root)
            .map_err(|source| PackBuildError::CreatePackRoot {
                path: packs_root.to_path_buf(),
                source,
            })?;
        let writer = ChunkWriter::new(staging.path(), options.chunk_bytes, options.pack_bytes);
        Ok(Self {
            staging,
            writer,
            objects: Vec::new(),
            chunk_bytes: options.chunk_bytes,
        })
    }

    fn can_fit(&self, object_length: u64, options: PackOptions) -> bool {
        self.objects.len() < options.pack_objects as usize
            && self
                .writer
                .length()
                .checked_add(object_length)
                .is_some_and(|length| length <= options.pack_bytes)
    }

    fn write(&mut self, object: PreparedObject<'_>) -> Result<u64, PackBuildError> {
        let key = object.key();
        let offset = self.writer.length();
        let length = object.write(&mut self.writer)?;
        self.objects.push(ObjectEntry {
            key,
            offset,
            length,
        });
        Ok(length)
    }

    fn finish(
        self,
        packs_root: &Path,
        operation_heads: &[ObjectId],
    ) -> Result<BuiltPack, PackBuildError> {
        let Self {
            staging,
            writer,
            objects,
            chunk_bytes,
        } = self;
        let (pack_length, pack_hash, chunks) = writer.finish()?;
        let manifest = PackManifest::new(
            chunk_bytes,
            pack_length,
            pack_hash,
            operation_heads.to_vec(),
            objects,
            chunks,
        )?;
        let manifest_bytes = manifest.encode();
        let id = hash(&manifest_bytes);
        write_file(staging.path().join("manifest.bin"), &manifest_bytes)?;
        sync_directory(staging.path())?;

        let directory = packs_root.join(hex(&id));
        if directory.exists() {
            verify_existing_pack(&directory, &manifest_bytes, &manifest)?;
        } else if let Err(source) = fs::rename(staging.path(), &directory) {
            if directory.exists() {
                verify_existing_pack(&directory, &manifest_bytes, &manifest)?;
            } else {
                return Err(PackBuildError::PersistPack {
                    path: directory,
                    source,
                });
            }
        }
        // A failed parent sync is retried even when the immutable pack already
        // exists, so no caller observes a pack before its directory entry is durable.
        sync_directory(packs_root)?;

        Ok(BuiltPack {
            id,
            directory,
            manifest,
        })
    }
}

enum PreparedObject<'a> {
    File {
        object: &'a MachineObject,
        file: File,
        length: u64,
    },
    Buffered {
        object: &'a MachineObject,
        bytes: Vec<u8>,
    },
}

impl PreparedObject<'_> {
    fn key(&self) -> ObjectKey {
        match self {
            Self::File { object, .. } | Self::Buffered { object, .. } => object.key,
        }
    }

    fn length(&self) -> u64 {
        match self {
            Self::File { length, .. } => *length,
            Self::Buffered { bytes, .. } => bytes.len() as u64,
        }
    }

    fn write(self, writer: &mut ChunkWriter) -> Result<u64, PackBuildError> {
        match self {
            Self::File { object, file, .. } => write_file_object(object, file, writer),
            Self::Buffered { bytes, .. } => {
                writer.write_all(&bytes)?;
                Ok(bytes.len() as u64)
            }
        }
    }
}

fn prepare_object(
    object: &MachineObject,
    pack_bytes: u64,
) -> Result<PreparedObject<'_>, PackBuildError> {
    let file = File::open(&object.path).map_err(|source| PackBuildError::ReadObject {
        key: object.key,
        path: object.path.clone(),
        source,
    })?;
    let length = file
        .metadata()
        .map_err(|source| PackBuildError::ReadObject {
            key: object.key,
            path: object.path.clone(),
            source,
        })?
        .len();
    if length > MAX_OBJECT_BYTES {
        return Err(PackBuildError::ObjectTooLarge {
            key: object.key,
            length,
            limit: MAX_OBJECT_BYTES,
        });
    }
    if length > pack_bytes {
        return Err(PackBuildError::ObjectExceedsPackLimit {
            key: object.key,
            length,
            limit: pack_bytes,
        });
    }

    if object.key.kind == ObjectKind::File {
        Ok(PreparedObject::File {
            object,
            file,
            length,
        })
    } else {
        let mut bytes = Vec::with_capacity(object.length.min(MAX_OBJECT_BYTES) as usize);
        file.take(MAX_OBJECT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| PackBuildError::ReadObject {
                key: object.key,
                path: object.path.clone(),
                source,
            })?;
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(PackBuildError::ObjectTooLarge {
                key: object.key,
                length: bytes.len() as u64,
                limit: MAX_OBJECT_BYTES,
            });
        }
        if bytes.len() as u64 > pack_bytes {
            return Err(PackBuildError::ObjectExceedsPackLimit {
                key: object.key,
                length: bytes.len() as u64,
                limit: pack_bytes,
            });
        }
        let validated =
            validate(object.key.kind, &bytes).map_err(|source| PackBuildError::ValidateObject {
                key: object.key,
                source,
            })?;
        require_id(object.key, validated.id)?;
        Ok(PreparedObject::Buffered { object, bytes })
    }
}

fn write_file_object(
    object: &MachineObject,
    file: File,
    writer: &mut ChunkWriter,
) -> Result<u64, PackBuildError> {
    let mut reader = file.take(MAX_OBJECT_BYTES + 1);
    let mut hasher = Blake2b512::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| PackBuildError::ReadObject {
                key: object.key,
                path: object.path.clone(),
                source,
            })?;
        if read == 0 {
            break;
        }
        length += read as u64;
        if length > MAX_OBJECT_BYTES {
            return Err(PackBuildError::ObjectTooLarge {
                key: object.key,
                length,
                limit: MAX_OBJECT_BYTES,
            });
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
    }
    require_id(object.key, hasher.finalize().into())?;
    Ok(length)
}

fn require_id(key: ObjectKey, actual: ObjectId) -> Result<(), PackBuildError> {
    if actual == key.id {
        Ok(())
    } else {
        Err(PackBuildError::ObjectIdMismatch {
            key,
            actual: hex(&actual),
        })
    }
}

fn write_file(path: PathBuf, bytes: &[u8]) -> Result<(), PackBuildError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| PackBuildError::WritePack {
            path: path.clone(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| PackBuildError::WritePack { path, source })
}

fn sync_directory(path: &Path) -> Result<(), PackBuildError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| PackBuildError::WritePack {
            path: path.to_path_buf(),
            source,
        })
}

fn read_bounded_existing_file(path: &Path, limit: u64) -> Result<Vec<u8>, PackBuildError> {
    let file = File::open(path).map_err(|source| PackBuildError::ReadExistingPack {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| PackBuildError::ReadExistingPack {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > limit {
        return Err(PackBuildError::ExistingPackInvalid {
            path: path.to_path_buf(),
            reason: format!("file exceeds {limit}-byte limit"),
        });
    }
    Ok(bytes)
}

fn verify_existing_pack(
    directory: &Path,
    manifest_bytes: &[u8],
    manifest: &PackManifest,
) -> Result<(), PackBuildError> {
    let path = directory.join("manifest.bin");
    let existing = read_bounded_existing_file(&path, MAX_MANIFEST_BYTES as u64)?;
    if existing != manifest_bytes {
        return Err(PackBuildError::ExistingPackInvalid {
            path,
            reason: "manifest bytes differ".to_string(),
        });
    }
    for (index, expected) in manifest.chunks().iter().enumerate() {
        let path = directory.join(chunk_name(index));
        let file = File::open(&path).map_err(|source| PackBuildError::ReadExistingPack {
            path: path.clone(),
            source,
        })?;
        let mut file = file.take(u64::from(expected.length) + 1);
        let mut hasher = Blake2b512::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read =
                file.read(&mut buffer)
                    .map_err(|source| PackBuildError::ReadExistingPack {
                        path: path.clone(),
                        source,
                    })?;
            if read == 0 {
                break;
            }
            length += read as u64;
            hasher.update(&buffer[..read]);
        }
        let actual_hash: ObjectId = hasher.finalize().into();
        if length != u64::from(expected.length) || actual_hash != expected.hash {
            return Err(PackBuildError::ExistingPackInvalid {
                path,
                reason: "chunk length or hash differs".to_string(),
            });
        }
    }
    Ok(())
}

struct ChunkWriter {
    directory: PathBuf,
    chunk_bytes: u32,
    current: Option<File>,
    current_hash: Blake2b512,
    current_length: u32,
    pack_hash: Blake2b512,
    length: u64,
    max_length: u64,
    chunks: Vec<ChunkEntry>,
}

impl ChunkWriter {
    fn new(directory: &Path, chunk_bytes: u32, max_length: u64) -> Self {
        Self {
            directory: directory.to_path_buf(),
            chunk_bytes,
            current: None,
            current_hash: Blake2b512::new(),
            current_length: 0,
            pack_hash: Blake2b512::new(),
            length: 0,
            max_length,
            chunks: Vec::new(),
        }
    }

    fn length(&self) -> u64 {
        self.length
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), PackBuildError> {
        if self
            .length
            .checked_add(bytes.len() as u64)
            .is_none_or(|length| length > self.max_length)
        {
            return Err(PackBuildError::PackLimitExceeded {
                limit: self.max_length,
            });
        }
        while !bytes.is_empty() {
            if self.current.is_none() {
                let path = self.directory.join(chunk_name(self.chunks.len()));
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|source| PackBuildError::WritePack { path, source })?;
                self.current = Some(file);
            }
            let available = (self.chunk_bytes - self.current_length) as usize;
            let count = available.min(bytes.len());
            let part = &bytes[..count];
            let path = self.directory.join(chunk_name(self.chunks.len()));
            self.current
                .as_mut()
                .unwrap()
                .write_all(part)
                .map_err(|source| PackBuildError::WritePack { path, source })?;
            self.current_hash.update(part);
            self.pack_hash.update(part);
            self.current_length += count as u32;
            self.length += count as u64;
            bytes = &bytes[count..];
            if self.current_length == self.chunk_bytes {
                self.finish_chunk()?;
            }
        }
        Ok(())
    }

    fn finish_chunk(&mut self) -> Result<(), PackBuildError> {
        let path = self.directory.join(chunk_name(self.chunks.len()));
        let file = self.current.take().unwrap();
        file.sync_all()
            .map_err(|source| PackBuildError::WritePack { path, source })?;
        let hash = std::mem::replace(&mut self.current_hash, Blake2b512::new())
            .finalize()
            .into();
        self.chunks.push(ChunkEntry {
            offset: self.length - u64::from(self.current_length),
            length: self.current_length,
            hash,
        });
        self.current_length = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<(u64, ObjectId, Vec<ChunkEntry>), PackBuildError> {
        if self.current.is_some() {
            self.finish_chunk()?;
        }
        Ok((self.length, self.pack_hash.finalize().into(), self.chunks))
    }
}

fn chunk_name(index: usize) -> String {
    format!("{index:08}.chunk")
}

fn hash(bytes: &[u8]) -> ObjectId {
    Blake2b512::digest(bytes).into()
}

#[derive(Debug, Error)]
pub enum PackBuildError {
    #[error(transparent)]
    Manifest(#[from] PackManifestError),
    #[error("chunk size must be between {minimum} and {maximum} bytes, got {chunk_bytes}")]
    InvalidChunkSize {
        chunk_bytes: u32,
        minimum: u32,
        maximum: u32,
    },
    #[error("pack size must be between {minimum} and {maximum} bytes, got {pack_bytes}")]
    InvalidPackSize {
        pack_bytes: u64,
        minimum: u64,
        maximum: u64,
    },
    #[error("pack object limit must be between 1 and {maximum}, got {pack_objects}")]
    InvalidPackObjectCount { pack_objects: u32, maximum: u32 },
    #[error("operation-head count {count} exceeds the {maximum}-head pack limit")]
    TooManyOperationHeads { count: usize, maximum: usize },
    #[error("object closure contains duplicate {0:?}")]
    DuplicateObject(ObjectKey),
    #[error("failed to create pack root {path}")]
    CreatePackRoot {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read {key:?} at {path}")]
    ReadObject {
        key: ObjectKey,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{key:?} is {length} bytes, exceeding the {limit}-byte object limit")]
    ObjectTooLarge {
        key: ObjectKey,
        length: u64,
        limit: u64,
    },
    #[error("{key:?} is {length} bytes, exceeding the {limit}-byte pack limit")]
    ObjectExceedsPackLimit {
        key: ObjectKey,
        length: u64,
        limit: u64,
    },
    #[error("source object changed while writing and exceeded the {limit}-byte pack limit")]
    PackLimitExceeded { limit: u64 },
    #[error("{key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: devspace_kernel::ValidationError,
    },
    #[error("{key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
    #[error("failed to write pack file {path}")]
    WritePack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to persist pack at {path}")]
    PersistPack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read existing pack file {path}")]
    ReadExistingPack {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing pack file {path} is invalid: {reason}")]
    ExistingPackInvalid { path: PathBuf, reason: String },
}
