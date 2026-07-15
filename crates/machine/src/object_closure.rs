use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read as _;
use std::path::PathBuf;

use devspace_kernel::{ObjectKind, RawHasher, ValidationError, validate};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_heads_store::OpHeadsStoreError;
use jj_lib::repo::Repo as _;
use thiserror::Error;

use crate::MachineRepository;

pub type ObjectId = [u8; 64];
pub const MAX_OBJECT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObjectKey {
    pub kind: ObjectKind,
    pub id: ObjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineObject {
    pub key: ObjectKey,
    pub path: PathBuf,
    pub length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectClosure {
    pub operation_heads: Vec<ObjectId>,
    pub objects: Vec<MachineObject>,
}

impl MachineRepository {
    /// Discovers canonical object files reachable from the current operation
    /// heads, stopping at operation heads already accepted by the cloud.
    pub async fn object_closure(
        &self,
        accepted_operation_heads: &BTreeSet<ObjectId>,
    ) -> Result<ObjectClosure, ObjectClosureError> {
        let operation_heads = self.current_operation_heads().await?;
        self.object_closure_from_heads(operation_heads, accepted_operation_heads)
    }

    /// Reads the canonical ordered head set directly from jj's stock store.
    pub async fn current_operation_heads(&self) -> Result<Vec<ObjectId>, ObjectClosureError> {
        let mut operation_heads = self
            .repo()
            .op_heads_store()
            .get_op_heads()
            .await?
            .into_iter()
            .map(|id| object_id("operation head", id.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        operation_heads.sort_unstable();
        operation_heads.dedup();
        Ok(operation_heads)
    }

    /// Validates and discovers canonical objects reachable from the supplied
    /// operation heads without changing the stock operation-head store.
    pub fn object_closure_from_heads(
        &self,
        mut operation_heads: Vec<ObjectId>,
        accepted_operation_heads: &BTreeSet<ObjectId>,
    ) -> Result<ObjectClosure, ObjectClosureError> {
        operation_heads.sort_unstable();
        operation_heads.dedup();

        let pending = operation_heads
            .iter()
            .copied()
            .map(|id| ObjectKey {
                kind: ObjectKind::Operation,
                id,
            })
            .collect();
        self.discover_object_closure(operation_heads, pending, accepted_operation_heads)
    }

    /// Discovers canonical commit, tree and leaf objects needed to make public
    /// projection shadows durable even when no operation references them.
    pub fn commit_closure(
        &self,
        commit_heads: &[CommitId],
    ) -> Result<ObjectClosure, ObjectClosureError> {
        let pending = commit_heads
            .iter()
            .map(|id| {
                Ok(ObjectKey {
                    kind: ObjectKind::Commit,
                    id: object_id("commit", id.as_bytes())?,
                })
            })
            .collect::<Result<BTreeSet<_>, ObjectClosureError>>()?;
        self.discover_object_closure(Vec::new(), pending, &BTreeSet::new())
    }

    fn discover_object_closure(
        &self,
        operation_heads: Vec<ObjectId>,
        mut pending: BTreeSet<ObjectKey>,
        accepted_operation_heads: &BTreeSet<ObjectId>,
    ) -> Result<ObjectClosure, ObjectClosureError> {
        let root_commit = object_id(
            "root commit",
            self.repo().store().root_commit_id().as_bytes(),
        )?;
        let mut visited = BTreeSet::new();
        let mut objects = Vec::new();

        while let Some(key) = pending.pop_first() {
            if !visited.insert(key)
                || is_implicit_root(key, root_commit)
                || (key.kind == ObjectKind::Operation && accepted_operation_heads.contains(&key.id))
            {
                continue;
            }

            let path = self.object_path(key);
            let file = File::open(&path).map_err(|source| ObjectClosureError::ReadObject {
                key,
                path: path.clone(),
                source,
            })?;
            let metadata = file
                .metadata()
                .map_err(|source| ObjectClosureError::ReadObject {
                    key,
                    path: path.clone(),
                    source,
                })?;

            if is_structured(key.kind) {
                if metadata.len() > MAX_OBJECT_BYTES {
                    return Err(ObjectClosureError::StructuredObjectTooLarge {
                        key,
                        length: metadata.len(),
                        limit: MAX_OBJECT_BYTES,
                    });
                }
                let mut bytes = Vec::with_capacity(metadata.len() as usize);
                file.take(MAX_OBJECT_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|source| ObjectClosureError::ReadObject {
                        key,
                        path: path.clone(),
                        source,
                    })?;
                if bytes.len() as u64 > MAX_OBJECT_BYTES {
                    return Err(ObjectClosureError::StructuredObjectTooLarge {
                        key,
                        length: bytes.len() as u64,
                        limit: MAX_OBJECT_BYTES,
                    });
                }
                let validated = validate(key.kind, &bytes)
                    .map_err(|source| ObjectClosureError::ValidateObject { key, source })?;
                if validated.id != key.id {
                    return Err(ObjectClosureError::ObjectIdMismatch {
                        key,
                        actual: hex(validated.id),
                    });
                }
                pending.extend(validated.references.into_iter().map(|reference| ObjectKey {
                    kind: reference.kind,
                    id: reference.id,
                }));
            }

            objects.push(MachineObject {
                key,
                path,
                length: metadata.len(),
            });
        }

        objects.sort_unstable_by_key(|object| object.key);
        Ok(ObjectClosure {
            operation_heads,
            objects,
        })
    }

    pub(crate) fn validate_leaf_objects(
        &self,
        closure: &ObjectClosure,
    ) -> Result<(), ObjectClosureError> {
        for object in &closure.objects {
            match object.key.kind {
                ObjectKind::File => {
                    let mut file = File::open(&object.path).map_err(|source| {
                        ObjectClosureError::ReadObject {
                            key: object.key,
                            path: object.path.clone(),
                            source,
                        }
                    })?;
                    let mut hasher = RawHasher::new();
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        let read = file.read(&mut buffer).map_err(|source| {
                            ObjectClosureError::ReadObject {
                                key: object.key,
                                path: object.path.clone(),
                                source,
                            }
                        })?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    require_object_id(object.key, hasher.finalize())?;
                }
                ObjectKind::Symlink => {
                    let mut bytes =
                        Vec::with_capacity(object.length.min(MAX_OBJECT_BYTES) as usize);
                    File::open(&object.path)
                        .map_err(|source| ObjectClosureError::ReadObject {
                            key: object.key,
                            path: object.path.clone(),
                            source,
                        })?
                        .take(MAX_OBJECT_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|source| ObjectClosureError::ReadObject {
                            key: object.key,
                            path: object.path.clone(),
                            source,
                        })?;
                    if bytes.len() as u64 > MAX_OBJECT_BYTES {
                        return Err(ObjectClosureError::StructuredObjectTooLarge {
                            key: object.key,
                            length: bytes.len() as u64,
                            limit: MAX_OBJECT_BYTES,
                        });
                    }
                    let validated = validate(ObjectKind::Symlink, &bytes).map_err(|source| {
                        ObjectClosureError::ValidateObject {
                            key: object.key,
                            source,
                        }
                    })?;
                    require_object_id(object.key, validated.id)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn object_path(&self, key: ObjectKey) -> PathBuf {
        let (store, directory) = match key.kind {
            ObjectKind::File => ("store", "files"),
            ObjectKind::Symlink => ("store", "symlinks"),
            ObjectKind::Tree => ("store", "trees"),
            ObjectKind::Commit => ("store", "commits"),
            ObjectKind::View => ("op_store", "views"),
            ObjectKind::Operation => ("op_store", "operations"),
        };
        self.path().join(store).join(directory).join(hex(key.id))
    }
}

fn is_structured(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Tree | ObjectKind::Commit | ObjectKind::View | ObjectKind::Operation
    )
}

fn is_implicit_root(key: ObjectKey, root_commit: ObjectId) -> bool {
    (key.kind == ObjectKind::Operation && key.id == [0; 64])
        || (key.kind == ObjectKind::View && key.id == [0; 64])
        || (key.kind == ObjectKind::Commit && key.id == root_commit)
}

fn object_id(object: &'static str, bytes: &[u8]) -> Result<ObjectId, ObjectClosureError> {
    bytes
        .try_into()
        .map_err(|_| ObjectClosureError::InvalidObjectId {
            object,
            length: bytes.len(),
        })
}

fn require_object_id(key: ObjectKey, actual: ObjectId) -> Result<(), ObjectClosureError> {
    if actual == key.id {
        Ok(())
    } else {
        Err(ObjectClosureError::ObjectIdMismatch {
            key,
            actual: hex(actual),
        })
    }
}

pub(crate) fn hex(id: ObjectId) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(id.len() * 2);
    for byte in id {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Error)]
pub enum ObjectClosureError {
    #[error(transparent)]
    ReadHeads(#[from] OpHeadsStoreError),
    #[error("{object} ID must be 64 bytes, got {length}")]
    InvalidObjectId { object: &'static str, length: usize },
    #[error("failed to read {key:?} at {path}")]
    ReadObject {
        key: ObjectKey,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("stored {key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: ValidationError,
    },
    #[error("stored {key:?} is {length} bytes, exceeding the {limit}-byte validation limit")]
    StructuredObjectTooLarge {
        key: ObjectKey,
        length: u64,
        limit: u64,
    },
    #[error("stored {key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
}
