//! Mirrors `jj_lib::backend` values and `jj_lib::simple_backend` encoding.

use blake2::Blake2b512;
use prost::Message;

use crate::error::{Context, ValidationError};
use crate::hash::{ContentHash, content_id};
use crate::proto;
use crate::{ObjectKind, ObjectReference, ValidatedObject};

const OBJECT_ID_LENGTH: usize = 64;
const CHANGE_ID_LENGTH: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Id(Vec<u8>);

impl ContentHash for Id {
    fn hash(&self, state: &mut Blake2b512) {
        self.0.hash(state);
    }
}

fn id(label: &str, bytes: Vec<u8>) -> Result<Id, ValidationError> {
    if bytes.len() != OBJECT_ID_LENGTH {
        return Err(ValidationError::new(format!(
            "{label} id must be {OBJECT_ID_LENGTH} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(Id(bytes))
}

fn reference(kind: ObjectKind, value: &Id) -> ObjectReference {
    let mut id = [0; OBJECT_ID_LENGTH];
    id.copy_from_slice(&value.0);
    ObjectReference { kind, id }
}

#[derive(Clone)]
struct Tree(Vec<(String, TreeValue)>);

impl ContentHash for Tree {
    fn hash(&self, state: &mut Blake2b512) {
        self.0.hash(state);
    }
}

#[derive(Clone)]
enum TreeValue {
    File {
        id: Id,
        executable: bool,
        copy_id: Vec<u8>,
    },
    Symlink(Id),
    Tree(Id),
}

impl ContentHash for TreeValue {
    fn hash(&self, state: &mut Blake2b512) {
        match self {
            Self::File {
                id,
                executable,
                copy_id,
            } => {
                0_u32.hash(state);
                id.hash(state);
                executable.hash(state);
                copy_id.hash(state);
            }
            Self::Symlink(id) => {
                1_u32.hash(state);
                id.hash(state);
            }
            Self::Tree(id) => {
                2_u32.hash(state);
                id.hash(state);
            }
        }
    }
}

pub(crate) fn validate_tree(bytes: &[u8]) -> Result<ValidatedObject, ValidationError> {
    let proto = proto::Tree::decode(bytes).context("decode tree object")?;
    let mut entries = Vec::with_capacity(proto.entries.len());
    let mut references = Vec::new();
    let mut previous_name: Option<&str> = None;

    for entry in &proto.entries {
        if entry.name.is_empty()
            || entry.name == "."
            || entry.name == ".."
            || entry.name.contains('/')
        {
            return Err(ValidationError::new(format!(
                "invalid tree entry name {:?}",
                entry.name
            )));
        }
        if previous_name.is_some_and(|previous| previous >= entry.name.as_str()) {
            return Err(ValidationError::new(
                "tree entries must be uniquely sorted by name",
            ));
        }
        let value = entry
            .value
            .as_ref()
            .and_then(|value| value.value.as_ref())
            .ok_or_else(|| {
                ValidationError::new(format!("tree entry {:?} has no value", entry.name))
            })?;
        let value = match value {
            proto::tree_value::Value::File(file) => {
                let id = id("file", file.id.clone())?;
                references.push(reference(ObjectKind::File, &id));
                TreeValue::File {
                    id,
                    executable: file.executable,
                    copy_id: file.copy_id.clone(),
                }
            }
            proto::tree_value::Value::SymlinkId(bytes) => {
                let id = id("symlink", bytes.clone())?;
                references.push(reference(ObjectKind::Symlink, &id));
                TreeValue::Symlink(id)
            }
            proto::tree_value::Value::TreeId(bytes) => {
                let id = id("tree", bytes.clone())?;
                references.push(reference(ObjectKind::Tree, &id));
                TreeValue::Tree(id)
            }
        };
        entries.push((entry.name.clone(), value));
        previous_name = Some(&entry.name);
    }

    if proto.encode_to_vec() != bytes {
        return Err(ValidationError::new(
            "tree object is not canonically encoded",
        ));
    }
    references.sort_unstable();
    references.dedup();
    Ok(ValidatedObject {
        id: content_id(&Tree(entries)),
        references,
    })
}

#[derive(Clone)]
struct Signature {
    name: String,
    email: String,
    timestamp: Timestamp,
}

impl ContentHash for Signature {
    fn hash(&self, state: &mut Blake2b512) {
        self.name.hash(state);
        self.email.hash(state);
        self.timestamp.hash(state);
    }
}

#[derive(Clone, Copy)]
struct Timestamp {
    millis: i64,
    offset: i32,
}

impl ContentHash for Timestamp {
    fn hash(&self, state: &mut Blake2b512) {
        self.millis.hash(state);
        self.offset.hash(state);
    }
}

#[derive(Clone)]
struct SecureSig {
    data: Vec<u8>,
    sig: Vec<u8>,
}

impl ContentHash for SecureSig {
    fn hash(&self, state: &mut Blake2b512) {
        self.data.hash(state);
        self.sig.hash(state);
    }
}

struct Commit {
    parents: Vec<Id>,
    predecessors: Vec<Id>,
    root_tree: Vec<Id>,
    conflict_labels: Vec<String>,
    change_id: Vec<u8>,
    description: String,
    author: Signature,
    committer: Signature,
    secure_sig: Option<SecureSig>,
}

impl ContentHash for Commit {
    fn hash(&self, state: &mut Blake2b512) {
        self.parents.hash(state);
        self.predecessors.hash(state);
        self.root_tree.hash(state);
        self.conflict_labels.hash(state);
        self.change_id.hash(state);
        self.description.hash(state);
        self.author.hash(state);
        self.committer.hash(state);
        self.secure_sig.hash(state);
    }
}

fn signature_from_proto(value: Option<proto::Signature>) -> Signature {
    let value = value.unwrap_or_default();
    let timestamp = value.timestamp.unwrap_or_default();
    Signature {
        name: value.name,
        email: value.email,
        timestamp: Timestamp {
            millis: timestamp.millis_since_epoch,
            offset: timestamp.tz_offset,
        },
    }
}

fn signature_to_proto(value: &Signature) -> proto::Signature {
    proto::Signature {
        name: value.name.clone(),
        email: value.email.clone(),
        timestamp: Some(proto::Timestamp {
            millis_since_epoch: value.timestamp.millis,
            tz_offset: value.timestamp.offset,
        }),
    }
}

pub(crate) fn validate_commit(bytes: &[u8]) -> Result<ValidatedObject, ValidationError> {
    let mut stored = proto::Commit::decode(bytes).context("decode commit object")?;
    if stored.parents.is_empty() {
        return Err(ValidationError::new(
            "root-like commits are synthesized and cannot be stored",
        ));
    }
    let parents = stored
        .parents
        .iter()
        .cloned()
        .map(|value| id("parent commit", value))
        .collect::<Result<Vec<_>, _>>()?;
    let predecessors = stored
        .predecessors
        .iter()
        .cloned()
        .map(|value| id("predecessor commit", value))
        .collect::<Result<Vec<_>, _>>()?;
    if stored.root_tree.is_empty() || stored.root_tree.len().is_multiple_of(2) {
        return Err(ValidationError::new(
            "root tree merge must contain an odd number of terms",
        ));
    }
    let root_tree = stored
        .root_tree
        .iter()
        .cloned()
        .map(|value| id("root tree", value))
        .collect::<Result<Vec<_>, _>>()?;
    if stored.change_id.len() != CHANGE_ID_LENGTH {
        return Err(ValidationError::new(format!(
            "change id must be {CHANGE_ID_LENGTH} bytes, got {}",
            stored.change_id.len()
        )));
    }
    // Mirrors jj's ConflictLabels: only an absent field is unlabeled, and
    // resolved labels must be the empty string, which is never stored.
    let conflict_labels = if stored.conflict_labels.is_empty() {
        vec![String::new()]
    } else {
        if stored.conflict_labels.len() == 1 {
            return Err(ValidationError::new(
                "resolved conflict labels are stored as an absent field",
            ));
        }
        if stored.conflict_labels.len().is_multiple_of(2)
            || stored.conflict_labels.len() != stored.root_tree.len()
        {
            return Err(ValidationError::new(
                "conflict labels must match the odd root tree merge",
            ));
        }
        stored.conflict_labels.clone()
    };

    let sig = stored.secure_sig.take();
    let unsigned_bytes = stored.encode_to_vec();
    let commit = Commit {
        parents,
        predecessors,
        root_tree,
        conflict_labels,
        change_id: stored.change_id.clone(),
        description: stored.description.clone(),
        author: signature_from_proto(stored.author.clone()),
        committer: signature_from_proto(stored.committer.clone()),
        secure_sig: sig.clone().map(|sig| SecureSig {
            data: unsigned_bytes,
            sig,
        }),
    };

    let mut canonical = proto::Commit {
        parents: commit.parents.iter().map(|value| value.0.clone()).collect(),
        predecessors: commit
            .predecessors
            .iter()
            .map(|value| value.0.clone())
            .collect(),
        root_tree: commit
            .root_tree
            .iter()
            .map(|value| value.0.clone())
            .collect(),
        change_id: commit.change_id.clone(),
        description: commit.description.clone(),
        author: Some(signature_to_proto(&commit.author)),
        committer: Some(signature_to_proto(&commit.committer)),
        secure_sig: None,
        conflict_labels: if commit.conflict_labels.len() == 1
            && commit.conflict_labels[0].is_empty()
        {
            Vec::new()
        } else {
            commit.conflict_labels.clone()
        },
    };
    if let Some(signature) = &commit.secure_sig {
        if signature.data != canonical.encode_to_vec() {
            return Err(ValidationError::new(
                "signed commit uses a non-canonical unsigned payload",
            ));
        }
        canonical.secure_sig = Some(signature.sig.clone());
    }
    if canonical.encode_to_vec() != bytes {
        return Err(ValidationError::new(
            "commit object is not canonically encoded",
        ));
    }

    let mut references = Vec::new();
    references.extend(
        commit
            .parents
            .iter()
            .map(|id| reference(ObjectKind::Commit, id)),
    );
    references.extend(
        commit
            .predecessors
            .iter()
            .map(|id| reference(ObjectKind::Commit, id)),
    );
    references.extend(
        commit
            .root_tree
            .iter()
            .map(|id| reference(ObjectKind::Tree, id)),
    );
    references.sort_unstable();
    references.dedup();

    Ok(ValidatedObject {
        id: content_id(&commit),
        references,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_entry_names_like_jj_repo_path_components() {
        for name in [".", "..", "", "a/b"] {
            let proto = proto::Tree {
                entries: vec![proto::TreeEntry {
                    name: name.to_owned(),
                    value: Some(proto::TreeValue {
                        value: Some(proto::tree_value::Value::TreeId(vec![7; 64])),
                    }),
                }],
            };
            assert!(
                validate_tree(&proto.encode_to_vec()).is_err(),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn rejects_tree_value_extensions_outside_jj_simple_store() {
        let mut tree_value = vec![0x2a, 20];
        tree_value.extend([7; 20]);

        let mut entry = vec![0x0a, 6];
        entry.extend(b"vendor");
        entry.extend([0x12, tree_value.len() as u8]);
        entry.extend(tree_value);

        let mut tree = vec![0x0a, entry.len() as u8];
        tree.extend(entry);
        assert!(validate_tree(&tree).is_err());
    }

    #[test]
    fn empty_tree_id_matches_v2_format_constant() {
        let object = validate_tree(&[]).unwrap();
        assert_eq!(
            hex(&object.id),
            "482ae5a29fbe856c7272f2071b8b0f0359ee2d89ff392b8a900643fbd0836eccd067b8bf41909e206c90d45d6e7d8b6686b93ecaee5fe1a9060d87b672101310"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
