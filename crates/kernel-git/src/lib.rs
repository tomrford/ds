#![no_std]
//! A no-I/O validation kernel for Git blob, tree, and commit payloads.
//!
//! The parser is hand-written and has no `gix` or `jj-lib` dependency. It
//! validates the Git shapes used by jj-lib 0.42.0's Git backend, preserves
//! commit headers and arbitrary message bytes, and reports object references
//! without imposing Devspace's product policy on Gitlinks.
//!
//! Object identity uses `sha1-checked` 0.10.0 with default features disabled.
//! It was selected over `sha1collisiondetection` 0.3.4 in July 2026: both are
//! pure Rust and `no_std`, but the port's last release and repository activity
//! were in March 2024. `sha1-checked` is maintained in the active RustCrypto
//! hashes repository, had about 24.4 million downloads, published a 0.11.0
//! release candidate in January 2026, and received repository updates through
//! March 2026. Version 0.10.0 was released in March 2024, satisfying the
//! minimum release-age rule. Plain SHA-1 crates were excluded because they do
//! not detect collisions. The hasher reports detected collisions explicitly;
//! this kernel rejects them.

extern crate alloc;

mod commit;
mod error;
mod hash;
mod tree;

use alloc::vec::Vec;

pub use commit::{Commit, CommitHeader, Signature, parse_commit};
pub use error::{CommitError, HashError, TreeError, ValidationError};
pub use hash::object_id;
pub use tree::{Tree, TreeEntry, TreeEntryKind, parse_tree};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Blob<'a> {
    pub data: &'a [u8],
}

pub const fn parse_blob(payload: &[u8]) -> Blob<'_> {
    Blob { data: payload }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum ObjectKind {
    Blob = 0,
    Tree = 1,
    Commit = 2,
}

impl ObjectKind {
    const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Blob => b"blob",
            Self::Tree => b"tree",
            Self::Commit => b"commit",
        }
    }
}

impl TryFrom<u8> for ObjectKind {
    type Error = ValidationError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Blob),
            1 => Ok(Self::Tree),
            2 => Ok(Self::Commit),
            _ => Err(ValidationError::UnknownObjectKind(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Oid(pub [u8; Self::LENGTH]);

impl Oid {
    pub const LENGTH: usize = 20;

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; Self::LENGTH] = bytes.try_into().ok()?;
        Some(Self(bytes))
    }

    pub fn from_hex(hex: &[u8]) -> Option<Self> {
        if hex.len() != Self::LENGTH.checked_mul(2)? {
            return None;
        }
        let mut bytes = [0_u8; Self::LENGTH];
        for (index, pair) in hex.chunks_exact(2).enumerate() {
            let high = hex_digit(*pair.first()?)?;
            let low = hex_digit(*pair.get(1)?)?;
            let slot = bytes.get_mut(index)?;
            *slot = high.checked_shl(4)? | low;
        }
        Some(Self(bytes))
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ReferenceKind {
    Blob,
    Executable,
    Symlink,
    Tree,
    Commit,
    Gitlink,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectReference {
    pub kind: ReferenceKind,
    pub id: Oid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedObject {
    pub id: Oid,
    pub references: Vec<ObjectReference>,
}

pub fn validate(kind: ObjectKind, payload: &[u8]) -> Result<ValidatedObject, ValidationError> {
    let mut references = match kind {
        ObjectKind::Blob => {
            let _ = parse_blob(payload);
            Vec::new()
        }
        ObjectKind::Tree => parse_tree(payload)?
            .entries
            .into_iter()
            .map(|entry| ObjectReference {
                kind: match entry.kind {
                    TreeEntryKind::File => ReferenceKind::Blob,
                    TreeEntryKind::Executable => ReferenceKind::Executable,
                    TreeEntryKind::Symlink => ReferenceKind::Symlink,
                    TreeEntryKind::Tree => ReferenceKind::Tree,
                    TreeEntryKind::Gitlink => ReferenceKind::Gitlink,
                },
                id: entry.oid,
            })
            .collect(),
        ObjectKind::Commit => {
            let commit = parse_commit(payload)?;
            let mut references = Vec::with_capacity(
                1_usize
                    .checked_add(commit.parents.len())
                    .and_then(|value| value.checked_add(commit.jj_trees.len()))
                    .unwrap_or(0),
            );
            references.push(ObjectReference {
                kind: ReferenceKind::Tree,
                id: commit.tree,
            });
            references.extend(commit.parents.into_iter().map(|id| ObjectReference {
                kind: ReferenceKind::Commit,
                id,
            }));
            references.extend(commit.jj_trees.into_iter().map(|id| ObjectReference {
                kind: ReferenceKind::Tree,
                id,
            }));
            references
        }
    };
    references.sort_unstable();
    references.dedup();
    Ok(ValidatedObject {
        id: object_id(kind, payload)?,
        references,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_kind_is_checked() {
        assert_eq!(ObjectKind::try_from(0), Ok(ObjectKind::Blob));
        assert!(ObjectKind::try_from(3).is_err());
    }

    #[test]
    fn invalid_tree_names_and_modes_are_typed_errors() {
        assert!(matches!(
            parse_tree(b"100600 file\0xxxxxxxxxxxxxxxxxxxx"),
            Err(TreeError::InvalidMode { .. })
        ));
        assert!(matches!(
            parse_tree(b"100644 ..\0xxxxxxxxxxxxxxxxxxxx"),
            Err(TreeError::InvalidName { .. })
        ));
    }

    #[test]
    fn malformed_commits_return_errors() {
        assert!(matches!(
            parse_commit(b"tree 0000000000000000000000000000000000000000\n"),
            Err(CommitError::MissingHeaderTerminator)
        ));
        assert!(matches!(
            parse_commit(b" continuation\n\n"),
            Err(CommitError::UnexpectedContinuation { .. })
        ));
    }
}
