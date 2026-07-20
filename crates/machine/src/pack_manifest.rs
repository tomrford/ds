use thiserror::Error;

use crate::object_closure::MAX_OBJECT_BYTES;
use crate::object_closure::ObjectId;
use crate::pack::{
    MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_OBJECTS, MAX_PACK_OPERATION_HEADS, MIN_CHUNK_BYTES,
};
use crate::{ObjectKey, ObjectKind};

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

    pub fn decode(bytes: &[u8]) -> Result<Self, PackManifestError> {
        if bytes.len() < MANIFEST_HEADER_BYTES {
            return Err(PackManifestError::InvalidEncoding {
                reason: "manifest header is truncated",
            });
        }
        if &bytes[..4] != MANIFEST_MAGIC {
            return Err(PackManifestError::InvalidEncoding {
                reason: "invalid manifest magic",
            });
        }
        if read_u16(bytes, 4) != MANIFEST_VERSION {
            return Err(PackManifestError::InvalidEncoding {
                reason: "unsupported manifest version",
            });
        }
        require_zero(&bytes[6..8], "manifest header reserved bytes")?;

        let chunk_bytes = read_u32(bytes, 8);
        let head_count = read_u32(bytes, 12) as usize;
        let object_count = read_u32(bytes, 16) as usize;
        let chunk_count = read_u32(bytes, 20) as usize;
        let pack_length = read_u64(bytes, 24);
        for (field, count, maximum) in [
            ("operation heads", head_count, MAX_PACK_OPERATION_HEADS),
            ("objects", object_count, MAX_PACK_OBJECTS as usize),
            ("chunks", chunk_count, MAX_PACK_CHUNKS),
        ] {
            if count > maximum {
                return Err(PackManifestError::TooManyEntries {
                    field,
                    count,
                    maximum,
                });
            }
        }
        let expected_length = MANIFEST_HEADER_BYTES
            .checked_add(head_count * 64)
            .and_then(|length| length.checked_add(object_count * OBJECT_ENTRY_BYTES))
            .and_then(|length| length.checked_add(chunk_count * CHUNK_ENTRY_BYTES))
            .ok_or(PackManifestError::InvalidEncoding {
                reason: "manifest length overflows",
            })?;
        if bytes.len() != expected_length {
            return Err(PackManifestError::InvalidEncoding {
                reason: "manifest length does not match counts",
            });
        }

        let pack_hash = bytes[32..96].try_into().unwrap();
        let mut offset = MANIFEST_HEADER_BYTES;
        let mut operation_heads = Vec::with_capacity(head_count);
        for _ in 0..head_count {
            operation_heads.push(bytes[offset..offset + 64].try_into().unwrap());
            offset += 64;
        }

        let mut objects = Vec::with_capacity(object_count);
        for index in 0..object_count {
            let kind = ObjectKind::try_from(bytes[offset]).map_err(|_| {
                PackManifestError::UnknownObjectKind {
                    index,
                    kind: bytes[offset],
                }
            })?;
            require_zero(
                &bytes[offset + 1..offset + 8],
                "manifest object reserved bytes",
            )?;
            let length = read_u64(bytes, offset + 80);
            if length > MAX_OBJECT_BYTES {
                return Err(PackManifestError::ObjectTooLarge {
                    index,
                    length,
                    maximum: MAX_OBJECT_BYTES,
                });
            }
            objects.push(ObjectEntry {
                key: ObjectKey {
                    kind,
                    id: bytes[offset + 8..offset + 72].try_into().unwrap(),
                },
                offset: read_u64(bytes, offset + 72),
                length,
            });
            offset += OBJECT_ENTRY_BYTES;
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            require_zero(
                &bytes[offset + 12..offset + 16],
                "manifest chunk reserved bytes",
            )?;
            chunks.push(ChunkEntry {
                offset: read_u64(bytes, offset),
                length: read_u32(bytes, offset + 8),
                hash: bytes[offset + 16..offset + 80].try_into().unwrap(),
            });
            offset += CHUNK_ENTRY_BYTES;
        }

        Self::new(
            chunk_bytes,
            pack_length,
            pack_hash,
            operation_heads,
            objects,
            chunks,
        )
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
        if self.objects.is_empty() {
            return Err(PackManifestError::InvalidEncoding {
                reason: "manifest must contain at least one object",
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
        if let Some((index, object)) = self
            .objects
            .iter()
            .enumerate()
            .find(|(_, object)| object.length > MAX_OBJECT_BYTES)
        {
            return Err(PackManifestError::ObjectTooLarge {
                index,
                length: object.length,
                maximum: MAX_OBJECT_BYTES,
            });
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

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn require_zero(bytes: &[u8], reason: &'static str) -> Result<(), PackManifestError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(PackManifestError::InvalidEncoding { reason })
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
    #[error("invalid pack manifest: {reason}")]
    InvalidEncoding { reason: &'static str },
    #[error("manifest object {index} has unknown kind {kind}")]
    UnknownObjectKind { index: usize, kind: u8 },
    #[error("manifest object {index} is {length} bytes, exceeding the {maximum}-byte limit")]
    ObjectTooLarge {
        index: usize,
        length: u64,
        maximum: u64,
    },
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
    use crate::encode_lower_hex as hex;
    use crate::{DEFAULT_CHUNK_BYTES, ObjectKind};
    use blake2::{Blake2b512, Digest};

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

    #[test]
    fn hello_manifest_matches_the_worker_protocol_vector() {
        let object_id = hex_id(
            "e4cfa39a3d37be31c59609e807970799caa68a19bfaa15135f165085e01d41a65ba1e1b146aeb6bd0092b49eac214c103ccfa3a365954bbbe52f74a2b3620c94",
        );
        let manifest = PackManifest::new(
            DEFAULT_CHUNK_BYTES,
            5,
            object_id,
            Vec::new(),
            vec![ObjectEntry {
                key: ObjectKey {
                    kind: ObjectKind::File,
                    id: object_id,
                },
                offset: 0,
                length: 5,
            }],
            vec![ChunkEntry {
                offset: 0,
                length: 5,
                hash: object_id,
            }],
        )
        .unwrap();
        let id: ObjectId = Blake2b512::digest(manifest.encode()).into();
        assert_eq!(
            hex(&id),
            "606591ef0c95a0b8ab99b4ccc8cfd34f05e143f82cf4e7ff0766183d21f0fce42456f1d602deaaef70fcaed78de2ca8cee73a055853d7aff1409c7a26b185733"
        );
        assert_eq!(PackManifest::decode(&manifest.encode()).unwrap(), manifest);
    }

    #[test]
    fn heads_manifest_matches_the_worker_protocol_vector() {
        let manifest = PackManifest::new(
            64 * 1024,
            80 * 1024,
            [0x77; 64],
            vec![[0x11; 64], [0x22; 64]],
            vec![
                ObjectEntry {
                    key: ObjectKey {
                        kind: ObjectKind::File,
                        id: [0x33; 64],
                    },
                    offset: 0,
                    length: 40 * 1024,
                },
                ObjectEntry {
                    key: ObjectKey {
                        kind: ObjectKind::Tree,
                        id: [0x44; 64],
                    },
                    offset: 40 * 1024,
                    length: 40 * 1024,
                },
            ],
            vec![
                ChunkEntry {
                    offset: 0,
                    length: 64 * 1024,
                    hash: [0x55; 64],
                },
                ChunkEntry {
                    offset: 64 * 1024,
                    length: 16 * 1024,
                    hash: [0x66; 64],
                },
            ],
        )
        .unwrap();
        let id: ObjectId = Blake2b512::digest(manifest.encode()).into();
        assert_eq!(
            hex(&id),
            "f1bf19025a446aefff8403fb0fdee17ff43382ecc8d5df0d398f24a741025c06faa832335491098ab8a285fce47d49d13510b94a7d009e4cefa3061a892ce52a"
        );
        assert_eq!(PackManifest::decode(&manifest.encode()).unwrap(), manifest);
    }

    #[test]
    fn decoder_rejects_noncanonical_reserved_bytes() {
        let mut bytes = PackManifest::new(
            DEFAULT_CHUNK_BYTES,
            1,
            [4; 64],
            Vec::new(),
            vec![ObjectEntry {
                key: key(1),
                offset: 0,
                length: 1,
            }],
            vec![ChunkEntry {
                offset: 0,
                length: 1,
                hash: [3; 64],
            }],
        )
        .unwrap()
        .encode();
        bytes[97] = 1;
        assert!(matches!(
            PackManifest::decode(&bytes),
            Err(PackManifestError::InvalidEncoding {
                reason: "manifest object reserved bytes"
            })
        ));
    }

    fn hex_id(value: &str) -> ObjectId {
        let mut id = [0; 64];
        for (index, byte) in id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap();
        }
        id
    }
}
