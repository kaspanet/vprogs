use alloc::vec::Vec;

use vprogs_core_codec::{Bits, Reader, Result};

use super::{leaf::Leaf, traversal::Traversal};
use crate::{Hasher, commitment::Commitment};

/// Zero-copy view of a multi-proof, borrowing from a flat byte buffer.
///
/// Wire format: `n_leaves(4) + leaves(66 each) + n_siblings(4) + siblings(32 each) +
/// topology_len(4) + topology_bytes`. All counts are LE u32.
pub struct Proof<'a> {
    /// Shortcut leaves with their declared depths and key/value hashes.
    pub leaves: Vec<Leaf<'a>>,
    /// Sibling hashes consumed sequentially during traversal.
    pub siblings: Vec<&'a [u8; 32]>,
    /// Packed topology bitfield encoding the proof tree structure.
    pub topology: &'a [u8],
}

impl<'a> Proof<'a> {
    /// Decodes a multi-proof from a flat byte buffer.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            leaves: buf.many("leaves", Leaf::decode)?,
            siblings: buf.many("siblings", |buf| buf.array::<32>("sibling"))?,
            topology: buf.blob("topology")?,
        })
    }

    /// Computes the root hash from the proof's own leaf value hashes.
    pub fn root<H: Hasher>(&self) -> Result<[u8; 32]> {
        Traversal::compute_root::<H>(self, |i| self.leaves[i].value_hash)
    }

    /// Computes the root hash using caller-provided value hashes (e.g. after mutations).
    ///
    /// The closure is called with leaf indices in `0..self.leaves.len()` during the tree
    /// traversal. Zero-allocation — no intermediate vec needed.
    pub fn compute_root<'v, H: Hasher>(
        &self,
        value_hash: impl Fn(usize) -> &'v [u8; 32],
    ) -> Result<[u8; 32]> {
        Traversal::compute_root::<H>(self, value_hash)
    }

    /// Encodes proof components into the wire format.
    pub(crate) fn encode(
        leaves: &[(u16, Commitment)],
        siblings: &[[u8; 32]],
        topology: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(
            4 + leaves.len() * Leaf::SIZE + 4 + siblings.len() * 32 + 4 + topology.len(),
        );

        // Section 1: leaf entries - each is depth(2) + key(32) + value_hash(32).
        buf.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
        for &(depth, Commitment { key, value_hash }) in leaves {
            Leaf::encode(&mut buf, depth, &key, &value_hash);
        }

        // Section 2: sibling hashes - each is 32 bytes.
        buf.extend_from_slice(&(siblings.len() as u32).to_le_bytes());
        for sibling in siblings {
            buf.extend_from_slice(sibling);
        }

        // Section 3: topology bitfield - variable length.
        buf.extend_from_slice(&(topology.len() as u32).to_le_bytes());
        buf.extend_from_slice(topology);

        buf
    }

    /// Finds the partition point where keys switch from bit=0 (left) to bit=1 (right).
    ///
    /// Proof leaves are sorted by key, so binary search via `partition_point` is valid.
    pub(super) fn split_point(&self, start: usize, end: usize, level: u16) -> usize {
        start + self.leaves[start..end].partition_point(|leaf| !leaf.key.get_msb(level as usize))
    }
}
