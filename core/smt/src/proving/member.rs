use crate::{EMPTY_HASH, proving::Leaf};

/// Decoded form of a [`Membership`](super::Membership).
pub struct Member<'a> {
    /// The key.
    pub key: &'a [u8; 32],
    /// The witness leaf.
    pub leaf: &'a Leaf,
    /// True when the leaf is absent in the SMT.
    pub absent: bool,
}

impl<'a> Member<'a> {
    /// The key's value hash, or `EMPTY_HASH` if absent.
    pub fn value_hash(&self) -> &'a [u8; 32] {
        if self.absent { &EMPTY_HASH } else { &self.leaf.value_hash }
    }
}
