use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::{Oid, TreeError};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TreeEntryKind {
    File,
    Executable,
    Symlink,
    Tree,
    Gitlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry<'a> {
    pub kind: TreeEntryKind,
    pub name: &'a [u8],
    pub oid: Oid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tree<'a> {
    pub entries: Vec<TreeEntry<'a>>,
}

pub fn parse_tree(payload: &[u8]) -> Result<Tree<'_>, TreeError> {
    let mut entries: Vec<TreeEntry<'_>> = Vec::new();
    let mut offset = 0_usize;
    while offset < payload.len() {
        let entry_offset = offset;
        let remainder = payload
            .get(offset..)
            .ok_or(TreeError::MissingMode { offset })?;
        let mode_end = remainder
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or(TreeError::MissingMode { offset })?;
        let mode = remainder
            .get(..mode_end)
            .ok_or(TreeError::MissingMode { offset })?;
        let kind = parse_mode(mode).ok_or(TreeError::InvalidMode { offset })?;

        offset = offset
            .checked_add(mode_end)
            .and_then(|value| value.checked_add(1))
            .ok_or(TreeError::MissingNameTerminator {
                offset: entry_offset,
            })?;
        let remainder = payload
            .get(offset..)
            .ok_or(TreeError::MissingNameTerminator {
                offset: entry_offset,
            })?;
        let name_end = remainder.iter().position(|byte| *byte == 0).ok_or(
            TreeError::MissingNameTerminator {
                offset: entry_offset,
            },
        )?;
        let name = remainder
            .get(..name_end)
            .ok_or(TreeError::MissingNameTerminator {
                offset: entry_offset,
            })?;
        if !valid_name(name) {
            return Err(TreeError::InvalidName {
                offset: entry_offset,
            });
        }

        offset = offset
            .checked_add(name_end)
            .and_then(|value| value.checked_add(1))
            .ok_or(TreeError::TruncatedObjectId {
                offset: entry_offset,
            })?;
        let oid_end = offset
            .checked_add(Oid::LENGTH)
            .ok_or(TreeError::TruncatedObjectId {
                offset: entry_offset,
            })?;
        let oid_bytes = payload
            .get(offset..oid_end)
            .ok_or(TreeError::TruncatedObjectId {
                offset: entry_offset,
            })?;
        let oid = Oid::from_bytes(oid_bytes).ok_or(TreeError::TruncatedObjectId {
            offset: entry_offset,
        })?;
        offset = oid_end;
        if let Some(previous) = entries.last() {
            if previous.name == name {
                return Err(TreeError::DuplicateName {
                    offset: entry_offset,
                });
            }
            if compare_entries(previous.name, previous.kind, name, kind) != Ordering::Less {
                return Err(TreeError::NonCanonicalOrder {
                    offset: entry_offset,
                });
            }
        }
        entries.push(TreeEntry { kind, name, oid });
    }
    Ok(Tree { entries })
}

fn parse_mode(mode: &[u8]) -> Option<TreeEntryKind> {
    match mode {
        b"100644" => Some(TreeEntryKind::File),
        b"100755" => Some(TreeEntryKind::Executable),
        b"120000" => Some(TreeEntryKind::Symlink),
        b"40000" | b"040000" => Some(TreeEntryKind::Tree),
        b"160000" => Some(TreeEntryKind::Gitlink),
        _ => None,
    }
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name != b"."
        && name != b".."
        && !name.iter().any(|byte| *byte == b'/' || *byte == 0)
}

fn compare_entries(
    left_name: &[u8],
    left_kind: TreeEntryKind,
    right_name: &[u8],
    right_kind: TreeEntryKind,
) -> Ordering {
    let shared = left_name.len().min(right_name.len());
    match left_name[..shared].cmp(&right_name[..shared]) {
        Ordering::Equal => entry_terminator(left_name, left_kind, shared)
            .cmp(&entry_terminator(right_name, right_kind, shared)),
        ordering => ordering,
    }
}

fn entry_terminator(name: &[u8], kind: TreeEntryKind, index: usize) -> u8 {
    name.get(index)
        .copied()
        .unwrap_or(if kind == TreeEntryKind::Tree { b'/' } else { 0 })
}
