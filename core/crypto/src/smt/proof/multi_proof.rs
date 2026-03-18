use alloc::vec::Vec;

use vprogs_core_utils::{Bits, Bools, Parser, Result};

use super::{leaf::Leaf, traversal::Traversal};
use crate::{Hasher, smt::tree::state_commitment::StateCommitment};

/// Zero-copy view of a multi-proof, borrowing from a flat byte buffer.
///
/// Wire format:
/// - `n_leaves: u32` + `[depth(u16) + key(32) + value_hash(32)] x n_leaves`
/// - `n_siblings: u32` + `[u8; 32] x n_siblings`
/// - `topology_len: u32` + `topology_bytes`
///
/// During verification, traversal stops at each leaf's declared depth and computes `hash_leaf(key,
/// value_hash)` instead of recursing all the way to depth 256. Shortcut leaves at shallow depths
/// mean far fewer hashes.
pub struct Proof<'a> {
    pub leaves: Vec<Leaf<'a>>,
    pub siblings: Vec<&'a [u8; 32]>,
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

    /// Encodes proof components into the wire format.
    pub(crate) fn encode(
        leaves: &[(u16, StateCommitment)],
        siblings: &[[u8; 32]],
        topology_bits: &[bool],
    ) -> Vec<u8> {
        let topology = topology_bits.pack_lsb();
        let total = 4 + leaves.len() * Leaf::SIZE + 4 + siblings.len() * 32 + 4 + topology.len();
        let mut buf = Vec::with_capacity(total);

        buf.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
        for &(depth, StateCommitment { key, value_hash }) in leaves {
            Leaf::encode(&mut buf, depth, &key, &value_hash);
        }

        // Section 2: sibling hashes — each is 32 bytes.
        buf.extend_from_slice(&(siblings.len() as u32).to_le_bytes());
        for sibling in siblings {
            buf.extend_from_slice(sibling);
        }

        // Section 3: topology bitfield — variable length.
        buf.extend_from_slice(&(topology.len() as u32).to_le_bytes());
        buf.extend_from_slice(&topology);

        debug_assert_eq!(buf.len(), total);
        buf
    }

    /// Verifies that the proof leaves produce the expected root hash.
    pub fn verify<H: Hasher>(&self, expected_root: [u8; 32]) -> Result<bool> {
        Traversal::compute_root::<H>(self, |i| self.leaves[i].value_hash)
            .map(|root| root == expected_root)
    }

    /// Recomputes the root using updated value hashes.
    ///
    /// `updated_hashes` must have the same length as `n_leaves()` and provides the new value hash
    /// for each leaf (in the same order as in the proof). This enables computing the post-update
    /// root without re-reading the tree.
    pub fn compute_root<H: Hasher>(&self, updated_hashes: &[[u8; 32]]) -> Result<[u8; 32]> {
        assert_eq!(updated_hashes.len(), self.leaves.len());
        Traversal::compute_root::<H>(self, |i| &updated_hashes[i])
    }

    /// Finds the partition point where keys switch from bit=0 (left) to bit=1 (right).
    pub(super) fn split_point(&self, start: usize, end: usize, level: u16) -> usize {
        let mut mid = start;
        while mid < end && !self.leaves[mid].key.get_msb(level as usize) {
            mid += 1;
        }
        mid
    }
}
