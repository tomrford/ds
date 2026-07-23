use alloc::string::ToString;

use sha1_checked::{Digest, Sha1};

use crate::{HashError, ObjectKind, Oid};

pub fn object_id(kind: ObjectKind, payload: &[u8]) -> Result<Oid, HashError> {
    let mut hasher = Sha1::builder().safe_hash(false).build();
    Digest::update(&mut hasher, kind.as_bytes());
    Digest::update(&mut hasher, b" ");
    Digest::update(&mut hasher, payload.len().to_string().as_bytes());
    Digest::update(&mut hasher, b"\0");
    Digest::update(&mut hasher, payload);
    let result = hasher.try_finalize();
    if result.has_collision() {
        return Err(HashError::CollisionDetected);
    }
    let bytes: [u8; 20] = (*result.hash()).into();
    Ok(Oid(bytes))
}
