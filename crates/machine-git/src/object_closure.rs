use std::collections::BTreeSet;

use devspace_kernel_git::{ObjectKind, Oid, ReferenceKind, ValidationError, validate};
use gix::objs::Kind as GitObjectKind;
use thiserror::Error;

use crate::{MachineGitRepository, hex};

pub const MAX_OBJECT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObjectKey {
    pub kind: ObjectKind,
    pub id: Oid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineObject {
    pub key: ObjectKey,
    pub length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectClosure {
    pub head_commits: Vec<Oid>,
    pub objects: Vec<MachineObject>,
}

impl MachineGitRepository {
    /// Discovers the Git object closure, including jj's `jj:trees` references.
    /// Gitlinks name another repository's commit and are not local ODB edges.
    pub fn object_closure(
        &self,
        head_commits: impl IntoIterator<Item = Oid>,
    ) -> Result<ObjectClosure, ObjectClosureError> {
        let mut head_commits = head_commits.into_iter().collect::<Vec<_>>();
        head_commits.sort_unstable();
        head_commits.dedup();
        let mut pending = head_commits
            .iter()
            .copied()
            .map(|id| ObjectKey {
                kind: ObjectKind::Commit,
                id,
            })
            .collect::<BTreeSet<_>>();
        let mut visited = BTreeSet::new();
        let mut objects = Vec::new();

        while let Some(key) = pending.pop_first() {
            if !visited.insert(key) {
                continue;
            }
            let bytes = self.read_object(key)?;
            let validated = validate(key.kind, &bytes)
                .map_err(|source| ObjectClosureError::ValidateObject { key, source })?;
            if validated.id != key.id {
                return Err(ObjectClosureError::ObjectIdMismatch {
                    key,
                    actual: hex(&validated.id.0),
                });
            }
            for reference in validated.references {
                let kind = match reference.kind {
                    ReferenceKind::Blob | ReferenceKind::Executable | ReferenceKind::Symlink => {
                        ObjectKind::Blob
                    }
                    ReferenceKind::Tree => ObjectKind::Tree,
                    ReferenceKind::Commit => ObjectKind::Commit,
                    ReferenceKind::Gitlink => continue,
                };
                pending.insert(ObjectKey {
                    kind,
                    id: reference.id,
                });
            }
            objects.push(MachineObject {
                key,
                length: bytes.len() as u64,
            });
        }
        objects.sort_unstable_by_key(|object| object.key);
        Ok(ObjectClosure {
            head_commits,
            objects,
        })
    }

    pub(crate) fn read_object(&self, key: ObjectKey) -> Result<Vec<u8>, ObjectClosureError> {
        let git_id = gix::ObjectId::from_bytes_or_panic(&key.id.0);
        let git_repo = self.git_repo();
        let object =
            git_repo
                .find_object(git_id)
                .map_err(|source| ObjectClosureError::ReadObject {
                    key,
                    source: Box::new(source),
                })?;
        let actual_kind =
            from_git_kind(object.kind).ok_or(ObjectClosureError::UnsupportedKind {
                key,
                actual: object.kind,
            })?;
        if actual_kind != key.kind {
            return Err(ObjectClosureError::KindMismatch {
                key,
                actual: actual_kind,
            });
        }
        if object.data.len() as u64 > MAX_OBJECT_BYTES {
            return Err(ObjectClosureError::ObjectTooLarge {
                key,
                length: object.data.len() as u64,
                limit: MAX_OBJECT_BYTES,
            });
        }
        Ok(object.data.clone())
    }
}

pub(crate) fn to_git_kind(kind: ObjectKind) -> GitObjectKind {
    match kind {
        ObjectKind::Blob => GitObjectKind::Blob,
        ObjectKind::Tree => GitObjectKind::Tree,
        ObjectKind::Commit => GitObjectKind::Commit,
    }
}

fn from_git_kind(kind: GitObjectKind) -> Option<ObjectKind> {
    match kind {
        GitObjectKind::Blob => Some(ObjectKind::Blob),
        GitObjectKind::Tree => Some(ObjectKind::Tree),
        GitObjectKind::Commit => Some(ObjectKind::Commit),
        GitObjectKind::Tag => None,
    }
}

#[derive(Debug, Error)]
pub enum ObjectClosureError {
    #[error("failed to read Git object {key:?}")]
    ReadObject {
        key: ObjectKey,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Git object {key:?} has unsupported kind {actual:?}")]
    UnsupportedKind {
        key: ObjectKey,
        actual: GitObjectKind,
    },
    #[error("Git object {key:?} has kind {actual:?}")]
    KindMismatch { key: ObjectKey, actual: ObjectKind },
    #[error("Git object {key:?} is {length} bytes; limit is {limit}")]
    ObjectTooLarge {
        key: ObjectKey,
        length: u64,
        limit: u64,
    },
    #[error("Git object {key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: ValidationError,
    },
    #[error("Git object {key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
}
