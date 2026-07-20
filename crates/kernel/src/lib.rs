//! A maintained mini-fork of jj's storage format: the simple backend and
//! simple op-store canonical encodings and their `ContentHash` scheme,
//! reimplemented without jj-lib so validation is no-I/O and panic-free.
//! Mirrors the format as of jj-lib 0.42.0, audited against that source;
//! every jj format change (new fields, hash inputs, validity rules) must be
//! mirrored here per the AGENTS.md parity procedure. ID parity is guarded by
//! `tests/jj_golden.txt`.

mod backend;
mod error;
mod hash;
mod op_store;
mod proto;

pub use error::ValidationError;
pub use hash::RawHasher;
use hash::raw_id;

/// Monotonic canonical-encoding epoch. Bump only when a jj upgrade changes the
/// canonical bytes this kernel accepts for existing object shapes (see the
/// AGENTS.md jj bump procedure). Clients advertise it in `x-devspace-client`
/// so the Worker can refuse stale fleets with an upgrade error instead of a
/// canonicality failure.
pub const ENCODING_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum ObjectKind {
    File = 0,
    Symlink = 1,
    Tree = 2,
    Commit = 3,
    View = 4,
    Operation = 5,
}

impl TryFrom<u8> for ObjectKind {
    type Error = ValidationError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::File),
            1 => Ok(Self::Symlink),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Commit),
            4 => Ok(Self::View),
            5 => Ok(Self::Operation),
            _ => Err(ValidationError::new(format!(
                "unknown object kind: {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObjectReference {
    pub kind: ObjectKind,
    pub id: [u8; 64],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedObject {
    pub id: [u8; 64],
    pub references: Vec<ObjectReference>,
}

pub fn validate(kind: ObjectKind, bytes: &[u8]) -> Result<ValidatedObject, ValidationError> {
    match kind {
        ObjectKind::File => Ok(ValidatedObject {
            id: raw_id(bytes),
            references: Vec::new(),
        }),
        ObjectKind::Symlink => {
            std::str::from_utf8(bytes)
                .map_err(|_| ValidationError::new("symlink target is not valid UTF-8"))?;
            Ok(ValidatedObject {
                id: raw_id(bytes),
                references: Vec::new(),
            })
        }
        ObjectKind::Tree => backend::validate_tree(bytes),
        ObjectKind::Commit => backend::validate_commit(bytes),
        ObjectKind::View => op_store::validate_view(bytes),
        ObjectKind::Operation => op_store::validate_operation(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_bytes_never_panic() {
        let mut cases = vec![Vec::new(), vec![0xff; 512], (0_u8..=255).collect()];
        for length in 1..128 {
            cases.push(
                (0..length)
                    .map(|index| (index as u8).wrapping_mul(31).wrapping_add(length as u8))
                    .collect(),
            );
        }
        for kind in [
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::View,
            ObjectKind::Operation,
        ] {
            for bytes in &cases {
                let _ = validate(kind, bytes);
            }
        }
    }

    #[test]
    fn raw_ids_are_stable_and_symlinks_require_utf8() {
        assert_eq!(
            validate(ObjectKind::File, b"hello").unwrap().id,
            validate(ObjectKind::File, b"hello").unwrap().id
        );
        assert!(validate(ObjectKind::Symlink, &[0xff]).is_err());
    }
}
