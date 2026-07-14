//! Mirrors `jj_lib::content_hash`, which defines every object ID.

use blake2::digest::Update;
use blake2::{Blake2b512, Digest};

pub(crate) trait ContentHash {
    fn hash(&self, state: &mut Blake2b512);
}

pub(crate) fn content_id(value: &impl ContentHash) -> [u8; 64] {
    let mut state = Blake2b512::default();
    value.hash(&mut state);
    state.finalize().into()
}

pub(crate) fn raw_id(bytes: &[u8]) -> [u8; 64] {
    Blake2b512::digest(bytes).into()
}

impl ContentHash for bool {
    fn hash(&self, state: &mut Blake2b512) {
        (*self as u8).hash(state);
    }
}

impl ContentHash for u8 {
    fn hash(&self, state: &mut Blake2b512) {
        Update::update(state, &[*self]);
    }
}

impl ContentHash for u32 {
    fn hash(&self, state: &mut Blake2b512) {
        Update::update(state, &self.to_le_bytes());
    }
}

impl ContentHash for i32 {
    fn hash(&self, state: &mut Blake2b512) {
        Update::update(state, &self.to_le_bytes());
    }
}

impl ContentHash for u64 {
    fn hash(&self, state: &mut Blake2b512) {
        Update::update(state, &self.to_le_bytes());
    }
}

impl ContentHash for i64 {
    fn hash(&self, state: &mut Blake2b512) {
        Update::update(state, &self.to_le_bytes());
    }
}

impl<T: ContentHash> ContentHash for [T] {
    fn hash(&self, state: &mut Blake2b512) {
        (self.len() as u64).hash(state);
        for item in self {
            item.hash(state);
        }
    }
}

impl<T: ContentHash> ContentHash for Vec<T> {
    fn hash(&self, state: &mut Blake2b512) {
        self.as_slice().hash(state);
    }
}

impl ContentHash for str {
    fn hash(&self, state: &mut Blake2b512) {
        self.as_bytes().hash(state);
    }
}

impl ContentHash for String {
    fn hash(&self, state: &mut Blake2b512) {
        self.as_str().hash(state);
    }
}

impl<T: ContentHash> ContentHash for Option<T> {
    fn hash(&self, state: &mut Blake2b512) {
        match self {
            None => 0_u32.hash(state),
            Some(value) => {
                1_u32.hash(state);
                value.hash(state);
            }
        }
    }
}

impl<A: ContentHash, B: ContentHash> ContentHash for (A, B) {
    fn hash(&self, state: &mut Blake2b512) {
        self.0.hash(state);
        self.1.hash(state);
    }
}

pub(crate) fn hash_map<'a, K: ContentHash + 'a, V: ContentHash + 'a>(
    entries: impl ExactSizeIterator<Item = (&'a K, &'a V)>,
    state: &mut Blake2b512,
) {
    (entries.len() as u64).hash(state);
    for (key, value) in entries {
        key.hash(state);
        value.hash(state);
    }
}
