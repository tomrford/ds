use devspace_kernel_git::{ObjectKind, Oid};
use thiserror::Error;

use crate::ObjectKey;
use crate::object_closure::MAX_OBJECT_BYTES;
use crate::pack::{
    Digest, MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_HEADS, MAX_PACK_OBJECTS, MIN_CHUNK_BYTES,
};

const MANIFEST_MAGIC: &[u8; 4] = b"DSPK";
const MANIFEST_VERSION: u16 = 2;
const MANIFEST_HEADER_BYTES: usize = 96;
const OBJECT_ENTRY_BYTES: usize = 44;
const CHUNK_ENTRY_BYTES: usize = 80;
const MAX_PACK_CHUNKS: usize = (MAX_PACK_BYTES / MIN_CHUNK_BYTES as u64) as usize;
pub const MAX_MANIFEST_BYTES: usize = MANIFEST_HEADER_BYTES
    + MAX_PACK_HEADS * Oid::LENGTH
    + MAX_PACK_OBJECTS as usize * OBJECT_ENTRY_BYTES
    + MAX_PACK_CHUNKS * CHUNK_ENTRY_BYTES;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectEntry {
    pub key: ObjectKey,
    pub(crate) offset: u64,
    pub length: u64,
}

impl ObjectEntry {
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkEntry {
    pub offset: u64,
    pub length: u32,
    pub(crate) hash: Digest,
}

impl ChunkEntry {
    pub fn hash(&self) -> Digest {
        self.hash
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackManifest {
    chunk_bytes: u32,
    pack_length: u64,
    pack_hash: Digest,
    head_commits: Vec<Oid>,
    objects: Vec<ObjectEntry>,
    chunks: Vec<ChunkEntry>,
}

impl PackManifest {
    pub(crate) fn new(
        chunk_bytes: u32,
        pack_length: u64,
        pack_hash: Digest,
        head_commits: Vec<Oid>,
        objects: Vec<ObjectEntry>,
        chunks: Vec<ChunkEntry>,
    ) -> Result<Self, PackManifestError> {
        let manifest = Self {
            chunk_bytes,
            pack_length,
            pack_hash,
            head_commits,
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

    pub(crate) fn pack_hash(&self) -> Digest {
        self.pack_hash
    }

    pub fn head_commits(&self) -> &[Oid] {
        &self.head_commits
    }

    pub fn objects(&self) -> &[ObjectEntry] {
        &self.objects
    }

    pub fn chunks(&self) -> &[ChunkEntry] {
        &self.chunks
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            MANIFEST_HEADER_BYTES
                + self.head_commits.len() * Oid::LENGTH
                + self.objects.len() * OBJECT_ENTRY_BYTES
                + self.chunks.len() * CHUNK_ENTRY_BYTES,
        );
        bytes.extend_from_slice(MANIFEST_MAGIC);
        bytes.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&self.chunk_bytes.to_le_bytes());
        bytes.extend_from_slice(&(self.head_commits.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.objects.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.pack_length.to_le_bytes());
        bytes.extend_from_slice(&self.pack_hash);
        debug_assert_eq!(bytes.len(), MANIFEST_HEADER_BYTES);

        for head in &self.head_commits {
            bytes.extend_from_slice(&head.0);
        }
        for object in &self.objects {
            bytes.push(object.key.kind as u8);
            bytes.extend_from_slice(&[0; 7]);
            bytes.extend_from_slice(&object.key.id.0);
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
            return Err(invalid("manifest header is truncated"));
        }
        if &bytes[..4] != MANIFEST_MAGIC {
            return Err(invalid("invalid manifest magic"));
        }
        if read_u16(bytes, 4) != MANIFEST_VERSION {
            return Err(invalid("unsupported manifest version"));
        }
        require_zero(&bytes[6..8], "manifest header reserved bytes")?;
        let chunk_bytes = read_u32(bytes, 8);
        let head_count = read_u32(bytes, 12) as usize;
        let object_count = read_u32(bytes, 16) as usize;
        let chunk_count = read_u32(bytes, 20) as usize;
        let pack_length = read_u64(bytes, 24);
        for (field, count, maximum) in [
            ("head commits", head_count, MAX_PACK_HEADS),
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
            .checked_add(
                head_count
                    .checked_mul(Oid::LENGTH)
                    .ok_or_else(|| invalid("manifest length overflows"))?,
            )
            .and_then(|length| length.checked_add(object_count * OBJECT_ENTRY_BYTES))
            .and_then(|length| length.checked_add(chunk_count * CHUNK_ENTRY_BYTES))
            .ok_or_else(|| invalid("manifest length overflows"))?;
        if bytes.len() != expected_length {
            return Err(invalid("manifest length does not match counts"));
        }

        let pack_hash = bytes[32..96]
            .try_into()
            .expect("fixed-size manifest header");
        let mut offset = MANIFEST_HEADER_BYTES;
        let mut head_commits = Vec::with_capacity(head_count);
        for _ in 0..head_count {
            head_commits.push(Oid(bytes[offset..offset + Oid::LENGTH]
                .try_into()
                .expect("count-checked manifest")));
            offset += Oid::LENGTH;
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
            objects.push(ObjectEntry {
                key: ObjectKey {
                    kind,
                    id: Oid(bytes[offset + 8..offset + 28]
                        .try_into()
                        .expect("count-checked manifest")),
                },
                offset: read_u64(bytes, offset + 28),
                length: read_u64(bytes, offset + 36),
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
                hash: bytes[offset + 16..offset + 80]
                    .try_into()
                    .expect("count-checked manifest"),
            });
            offset += CHUNK_ENTRY_BYTES;
        }
        Self::new(
            chunk_bytes,
            pack_length,
            pack_hash,
            head_commits,
            objects,
            chunks,
        )
    }

    fn validate(&self) -> Result<(), PackManifestError> {
        if !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&self.chunk_bytes) {
            return Err(PackManifestError::InvalidChunkSize(self.chunk_bytes));
        }
        if self.pack_length > MAX_PACK_BYTES {
            return Err(PackManifestError::PackTooLarge(self.pack_length));
        }
        if self.objects.is_empty() {
            return Err(invalid("manifest must contain at least one object"));
        }
        if self.head_commits.len() > MAX_PACK_HEADS
            || self.objects.len() > MAX_PACK_OBJECTS as usize
            || self.chunks.len() > MAX_PACK_CHUNKS
        {
            return Err(invalid("manifest contains too many entries"));
        }
        if !strictly_sorted(&self.head_commits) {
            return Err(invalid("head commits must be sorted and unique"));
        }
        if !self
            .objects
            .windows(2)
            .all(|pair| pair[0].key < pair[1].key)
        {
            return Err(invalid("objects must be sorted and unique"));
        }
        let mut offset = 0_u64;
        for object in &self.objects {
            if object.offset != offset {
                return Err(invalid("object ranges are not contiguous"));
            }
            if object.length > MAX_OBJECT_BYTES {
                return Err(PackManifestError::ObjectTooLarge(object.length));
            }
            offset = offset
                .checked_add(object.length)
                .ok_or_else(|| invalid("object ranges overflow"))?;
        }
        if offset != self.pack_length {
            return Err(invalid("object ranges do not cover the pack"));
        }
        offset = 0;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if chunk.offset != offset || chunk.length == 0 || chunk.length > self.chunk_bytes {
                return Err(invalid("chunk ranges are invalid"));
            }
            if index + 1 < self.chunks.len() && chunk.length != self.chunk_bytes {
                return Err(invalid("only the final chunk may be short"));
            }
            offset = offset
                .checked_add(u64::from(chunk.length))
                .ok_or_else(|| invalid("chunk ranges overflow"))?;
        }
        if offset != self.pack_length {
            return Err(invalid("chunk ranges do not cover the pack"));
        }
        Ok(())
    }
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn invalid(reason: &'static str) -> PackManifestError {
    PackManifestError::InvalidEncoding(reason)
}

fn require_zero(bytes: &[u8], field: &'static str) -> Result<(), PackManifestError> {
    if bytes.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(PackManifestError::ReservedBytes(field))
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("bounded field"))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("bounded field"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("bounded field"))
}

#[derive(Debug, Error)]
pub enum PackManifestError {
    #[error("invalid pack manifest: {0}")]
    InvalidEncoding(&'static str),
    #[error("unsupported non-zero {0}")]
    ReservedBytes(&'static str),
    #[error("unknown object kind {kind} at index {index}")]
    UnknownObjectKind { index: usize, kind: u8 },
    #[error("pack manifest has {count} {field}; maximum is {maximum}")]
    TooManyEntries {
        field: &'static str,
        count: usize,
        maximum: usize,
    },
    #[error("invalid chunk size {0}")]
    InvalidChunkSize(u32),
    #[error("pack length {0} exceeds the limit")]
    PackTooLarge(u64),
    #[error("object length {0} exceeds the limit")]
    ObjectTooLarge(u64),
}

#[cfg(test)]
mod worker_fixture_generation {
    use std::fs;
    use std::path::PathBuf;

    use devspace_kernel_git::{ObjectKind, Oid, validate};
    use serde_json::{Value, json};

    use super::{ChunkEntry, ObjectEntry, PackManifest};
    use crate::object_closure::ObjectKey;
    use crate::pack::{MIN_CHUNK_BYTES, hash};

    #[test]
    #[ignore = "regenerates the checked-in Worker pack fixtures"]
    fn write_worker_git_pack_fixtures() {
        let blob_bytes = b"worker Git fixture\n".to_vec();
        let blob = object(ObjectKind::Blob, blob_bytes.clone());

        let mut tree_bytes = b"100644 fixture.txt\0".to_vec();
        tree_bytes.extend_from_slice(&blob.0.id.0);
        let tree = object(ObjectKind::Tree, tree_bytes);

        let commit_bytes = format!(
            "tree {}\nauthor Worker Fixture <worker@example.invalid> 1700000000 +0000\ncommitter Worker Fixture <worker@example.invalid> 1700000000 +0000\n\nworker fixture\n",
            crate::hex(&tree.0.id.0),
        )
        .into_bytes();
        let commit = object(ObjectKind::Commit, commit_bytes);

        let complete = fixture(
            vec![commit.0.id],
            vec![blob.clone(), tree.clone(), commit.clone()],
        );
        let journal_commits = (0..260)
            .map(|index| {
                object(
                    ObjectKind::Commit,
                    format!(
                        "tree {}\nauthor Journal Fixture <journal@example.invalid> {} +0000\ncommitter Journal Fixture <journal@example.invalid> {} +0000\n\njournal fixture {index}\n",
                        crate::hex(&tree.0.id.0),
                        1_700_001_000 + index,
                        1_700_001_000 + index,
                    )
                    .into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let journal = fixture(
            journal_commits.iter().map(|commit| commit.0.id).collect(),
            [vec![blob.clone(), tree.clone()], journal_commits].concat(),
        );
        let dependency = fixture(Vec::new(), vec![blob.clone()]);
        let missing_reference = fixture(vec![commit.0.id], vec![tree, commit]);

        let (malformed_id, malformed_bytes) = truncated_golden_commit();
        let malformed = fixture(
            vec![malformed_id],
            vec![
                blob,
                (
                    ObjectKey {
                        kind: ObjectKind::Commit,
                        id: malformed_id,
                    },
                    malformed_bytes,
                ),
            ],
        );

        let output = json!({
            "complete": complete,
            "journal": journal,
            "dependency": dependency,
            "missingReference": missing_reference,
            "malformed": malformed,
        });
        let path = repository_root().join("test/fixtures/repository_git.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&output).unwrap()).unwrap();
    }

    fn object(kind: ObjectKind, bytes: Vec<u8>) -> (ObjectKey, Vec<u8>) {
        let id = validate(kind, &bytes).unwrap().id;
        (ObjectKey { kind, id }, bytes)
    }

    fn fixture(mut heads: Vec<Oid>, mut objects: Vec<(ObjectKey, Vec<u8>)>) -> Value {
        heads.sort_unstable();
        objects.sort_unstable_by_key(|(key, _)| *key);
        let mut data = Vec::new();
        let entries = objects
            .into_iter()
            .map(|(key, bytes)| {
                let offset = data.len() as u64;
                let length = bytes.len() as u64;
                data.extend_from_slice(&bytes);
                ObjectEntry {
                    key,
                    offset,
                    length,
                }
            })
            .collect();
        let chunk = ChunkEntry {
            offset: 0,
            length: data.len() as u32,
            hash: hash(&data),
        };
        let manifest = PackManifest::new(
            MIN_CHUNK_BYTES,
            data.len() as u64,
            hash(&data),
            heads,
            entries,
            vec![chunk],
        )
        .unwrap();
        let manifest = manifest.encode();
        json!({
            "id": crate::hex(&hash(&manifest)),
            "manifest": crate::hex(&manifest),
            "chunks": [crate::hex(&data)],
        })
    }

    fn truncated_golden_commit() -> (Oid, Vec<u8>) {
        let golden =
            fs::read_to_string(repository_root().join("crates/kernel-git/tests/git_golden.txt"))
                .unwrap();
        let line = golden
            .lines()
            .find(|line| line.starts_with("commit|"))
            .unwrap();
        let mut fields = line.split('|');
        assert_eq!(fields.next(), Some("commit"));
        let id = Oid::from_hex(fields.next().unwrap().as_bytes()).unwrap();
        let bytes = decode_hex(fields.next().unwrap());
        let terminator = bytes.windows(2).position(|pair| pair == b"\n\n").unwrap();
        (id, bytes[..=terminator].to_vec())
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn repository_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned()
    }
}
