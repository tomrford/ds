use thiserror::Error;

use crate::ObjectKey;
use crate::object_closure::ObjectId;
use crate::pack::{
    MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_OBJECTS, MAX_PACK_OPERATION_HEADS, MIN_CHUNK_BYTES,
};

const MANIFEST_MAGIC: &[u8; 4] = b"DSPK";
const MANIFEST_VERSION: u16 = 1;
const MANIFEST_HEADER_BYTES: usize = 96;
const OBJECT_ENTRY_BYTES: usize = 88;
const CHUNK_ENTRY_BYTES: usize = 80;
const MAX_PACK_CHUNKS: usize = (MAX_PACK_BYTES / MIN_CHUNK_BYTES as u64) as usize;
pub(crate) const MAX_MANIFEST_BYTES: usize = MANIFEST_HEADER_BYTES
    + MAX_PACK_OPERATION_HEADS * 64
    + MAX_PACK_OBJECTS as usize * OBJECT_ENTRY_BYTES
    + MAX_PACK_CHUNKS * CHUNK_ENTRY_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectEntry {
    pub key: ObjectKey,
    pub offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkEntry {
    pub offset: u64,
    pub length: u32,
    pub hash: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackManifest {
    chunk_bytes: u32,
    pack_length: u64,
    pack_hash: ObjectId,
    operation_heads: Vec<ObjectId>,
    objects: Vec<ObjectEntry>,
    chunks: Vec<ChunkEntry>,
}

impl PackManifest {
    pub(crate) fn new(
        chunk_bytes: u32,
        pack_length: u64,
        pack_hash: ObjectId,
        operation_heads: Vec<ObjectId>,
        objects: Vec<ObjectEntry>,
        chunks: Vec<ChunkEntry>,
    ) -> Result<Self, PackManifestError> {
        let manifest = Self {
            chunk_bytes,
            pack_length,
            pack_hash,
            operation_heads,
            objects,
            chunks,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn chunk_bytes(&self) -> u32 {
        self.chunk_bytes
    }

    pub fn pack_length(&self) -> u64 {
        self.pack_length
    }

    pub fn pack_hash(&self) -> ObjectId {
        self.pack_hash
    }

    pub fn operation_heads(&self) -> &[ObjectId] {
        &self.operation_heads
    }

    pub fn objects(&self) -> &[ObjectEntry] {
        &self.objects
    }

    pub fn chunks(&self) -> &[ChunkEntry] {
        &self.chunks
    }

    pub fn encode(&self) -> Vec<u8> {
        let head_count = self.operation_heads.len() as u32;
        let object_count = self.objects.len() as u32;
        let chunk_count = self.chunks.len() as u32;
        let mut bytes = Vec::with_capacity(
            MANIFEST_HEADER_BYTES
                + self.operation_heads.len() * 64
                + self.objects.len() * OBJECT_ENTRY_BYTES
                + self.chunks.len() * CHUNK_ENTRY_BYTES,
        );
        bytes.extend_from_slice(MANIFEST_MAGIC);
        bytes.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&self.chunk_bytes.to_le_bytes());
        bytes.extend_from_slice(&head_count.to_le_bytes());
        bytes.extend_from_slice(&object_count.to_le_bytes());
        bytes.extend_from_slice(&chunk_count.to_le_bytes());
        bytes.extend_from_slice(&self.pack_length.to_le_bytes());
        bytes.extend_from_slice(&self.pack_hash);
        debug_assert_eq!(bytes.len(), MANIFEST_HEADER_BYTES);

        for head in &self.operation_heads {
            bytes.extend_from_slice(head);
        }
        for object in &self.objects {
            bytes.push(object.key.kind as u8);
            bytes.extend_from_slice(&[0; 7]);
            bytes.extend_from_slice(&object.key.id);
            bytes.extend_from_slice(&object.offset.to_le_bytes());
            bytes.extend_from_slice(&object.length.to_le_bytes());
        }
        for chunk in &self.chunks {
            bytes.extend_from_slice(&chunk.offset.to_le_bytes());
            bytes.extend_from_slice(&chunk.length.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&chunk.hash);
        }
        bytes
    }

    fn validate(&self) -> Result<(), PackManifestError> {
        if !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&self.chunk_bytes) {
            return Err(PackManifestError::InvalidChunkSize {
                chunk_bytes: self.chunk_bytes,
            });
        }
        if self.pack_length > MAX_PACK_BYTES {
            return Err(PackManifestError::PackTooLarge {
                length: self.pack_length,
                maximum: MAX_PACK_BYTES,
            });
        }
        manifest_count("operation heads", self.operation_heads.len())?;
        manifest_count("objects", self.objects.len())?;
        manifest_count("chunks", self.chunks.len())?;
        for (field, count, maximum) in [
            (
                "operation heads",
                self.operation_heads.len(),
                MAX_PACK_OPERATION_HEADS,
            ),
            ("objects", self.objects.len(), MAX_PACK_OBJECTS as usize),
            ("chunks", self.chunks.len(), MAX_PACK_CHUNKS),
        ] {
            if count > maximum {
                return Err(PackManifestError::TooManyEntries {
                    field,
                    count,
                    maximum,
                });
            }
        }
        if !strictly_sorted(&self.operation_heads) {
            return Err(PackManifestError::NonCanonicalOrder {
                field: "operation heads",
            });
        }
        if !self
            .objects
            .windows(2)
            .all(|pair| pair[0].key < pair[1].key)
        {
            return Err(PackManifestError::NonCanonicalOrder { field: "objects" });
        }
        validate_ranges(
            "object",
            self.objects
                .iter()
                .map(|entry| (entry.offset, entry.length)),
            self.pack_length,
            None,
        )?;
        validate_ranges(
            "chunk",
            self.chunks
                .iter()
                .map(|entry| (entry.offset, u64::from(entry.length))),
            self.pack_length,
            Some(self.chunk_bytes),
        )
    }
}

fn manifest_count(field: &'static str, count: usize) -> Result<u32, PackManifestError> {
    count
        .try_into()
        .map_err(|_| PackManifestError::CountTooLarge { field, count })
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn validate_ranges(
    field: &'static str,
    ranges: impl IntoIterator<Item = (u64, u64)>,
    pack_length: u64,
    chunk_bytes: Option<u32>,
) -> Result<(), PackManifestError> {
    let ranges = ranges.into_iter().collect::<Vec<_>>();
    let mut expected_offset = 0_u64;
    for (index, (offset, length)) in ranges.iter().copied().enumerate() {
        let invalid_chunk = chunk_bytes.is_some_and(|maximum| {
            length == 0
                || length > u64::from(maximum)
                || (index + 1 < ranges.len() && length != u64::from(maximum))
        });
        if offset != expected_offset || invalid_chunk {
            return Err(PackManifestError::InvalidRange { field, index });
        }
        expected_offset = expected_offset
            .checked_add(length)
            .ok_or(PackManifestError::InvalidRange { field, index })?;
    }
    if expected_offset != pack_length {
        return Err(PackManifestError::LengthMismatch {
            field,
            expected: pack_length,
            actual: expected_offset,
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum PackManifestError {
    #[error("manifest {field} count {count} exceeds the u32 format limit")]
    CountTooLarge { field: &'static str, count: usize },
    #[error("manifest {field} count {count} exceeds the {maximum}-entry format limit")]
    TooManyEntries {
        field: &'static str,
        count: usize,
        maximum: usize,
    },
    #[error("manifest chunk size {chunk_bytes} is outside the canonical range")]
    InvalidChunkSize { chunk_bytes: u32 },
    #[error("manifest pack length {length} exceeds the {maximum}-byte format limit")]
    PackTooLarge { length: u64, maximum: u64 },
    #[error("manifest {field} are not strictly sorted and unique")]
    NonCanonicalOrder { field: &'static str },
    #[error("manifest {field} range {index} is not canonical")]
    InvalidRange { field: &'static str, index: usize },
    #[error("manifest {field} ranges total {actual} bytes, expected {expected}")]
    LengthMismatch {
        field: &'static str,
        expected: u64,
        actual: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_CHUNK_BYTES, ObjectKind};

    fn key(byte: u8) -> ObjectKey {
        ObjectKey {
            kind: ObjectKind::File,
            id: [byte; 64],
        }
    }

    #[test]
    fn constructor_rejects_noncanonical_order_and_ranges() {
        let chunk = ChunkEntry {
            offset: 0,
            length: 1,
            hash: [3; 64],
        };
        assert!(matches!(
            PackManifest::new(
                DEFAULT_CHUNK_BYTES,
                1,
                [4; 64],
                vec![[2; 64], [1; 64]],
                vec![ObjectEntry {
                    key: key(1),
                    offset: 0,
                    length: 1,
                }],
                vec![chunk.clone()],
            ),
            Err(PackManifestError::NonCanonicalOrder {
                field: "operation heads"
            })
        ));
        assert!(matches!(
            PackManifest::new(
                DEFAULT_CHUNK_BYTES,
                1,
                [4; 64],
                Vec::new(),
                vec![
                    ObjectEntry {
                        key: key(2),
                        offset: 0,
                        length: 0,
                    },
                    ObjectEntry {
                        key: key(1),
                        offset: 0,
                        length: 1,
                    },
                ],
                vec![chunk.clone()],
            ),
            Err(PackManifestError::NonCanonicalOrder { field: "objects" })
        ));
        assert!(matches!(
            PackManifest::new(
                DEFAULT_CHUNK_BYTES,
                1,
                [4; 64],
                Vec::new(),
                vec![ObjectEntry {
                    key: key(1),
                    offset: 1,
                    length: 1,
                }],
                vec![chunk],
            ),
            Err(PackManifestError::InvalidRange {
                field: "object",
                index: 0
            })
        ));
    }
}
