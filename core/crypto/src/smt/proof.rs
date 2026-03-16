use alloc::vec::Vec;

use vprogs_core_utils::{Bits, Bools, DecodeError, Parser};

use super::{leaf::Leaf, state_commitment::StateCommitment};
use crate::{EMPTY_HASH, Hasher};

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
    pub fn decode(mut buf: &'a [u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            leaves: buf.consume_n("leaves", Leaf::decode)?,
            siblings: buf.consume_n("siblings", |buf| buf.consume_array::<32>("sibling"))?,
            topology: buf.consume_blob("topology")?,
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

        // Section 1: leaf entries — each is depth(2) + key(32) + value_hash(32) = 66 bytes.
        buf.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
        for &(depth, StateCommitment { key, value_hash }) in leaves {
            buf.extend_from_slice(&depth.to_le_bytes());
            buf.extend_from_slice(&key);
            buf.extend_from_slice(&value_hash);
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
    pub fn verify<H: Hasher>(&self, expected_root: [u8; 32]) -> bool {
        self.compute_root_with::<H>(|i| self.leaves[i].value_hash) == expected_root
    }

    /// Recomputes the root using updated value hashes.
    ///
    /// `updated_hashes` must have the same length as `n_leaves()` and provides the new value hash
    /// for each leaf (in the same order as in the proof). This enables computing the post-update
    /// root without re-reading the tree.
    pub fn compute_root<H: Hasher>(&self, updated_hashes: &[[u8; 32]]) -> [u8; 32] {
        assert_eq!(updated_hashes.len(), self.leaves.len());
        self.compute_root_with::<H>(|i| &updated_hashes[i])
    }

    /// Shared traversal logic parameterized on which value hash to use for each leaf.
    fn compute_root_with<'b, H: Hasher>(
        &self,
        value_hash_fn: impl Fn(usize) -> &'b [u8; 32],
    ) -> [u8; 32] {
        if self.leaves.is_empty() {
            return EMPTY_HASH;
        }

        let mut sibling_idx = 0;
        let mut topo_bit = 0usize;
        self.traverse::<H>(0, self.leaves.len(), 0, &value_hash_fn, &mut sibling_idx, &mut topo_bit)
    }

    /// Recursive traversal of the shortcut-aware proof tree.
    ///
    /// `start..end` is the range of leaf indices that fall within this subtree. Because proof
    /// leaves are sorted by key and MSB-first bit ordering matches lexicographic order, splitting
    /// by any bit always produces two contiguous sub-ranges — so ranges suffice (no Vec allocations
    /// needed).
    fn traverse<'b, H: Hasher>(
        &self,
        start: usize,
        end: usize,
        bit_pos: usize,
        value_hash_fn: &impl Fn(usize) -> &'b [u8; 32],
        sibling_idx: &mut usize,
        topo_bit: &mut usize,
    ) -> [u8; 32] {
        if start == end {
            return EMPTY_HASH;
        }

        // Shortcut leaf check: if there's exactly one leaf and we've reached its declared depth,
        // compute the leaf hash directly instead of recursing further.
        if end - start == 1 {
            let leaf = &self.leaves[start];
            if bit_pos == leaf.depth as usize {
                return H::hash_leaf(leaf.key, value_hash_fn(start));
            }
        }

        // Read the next topology bit to determine the structure at this level.
        let bit_val = self.topology.get_lsb(*topo_bit);
        *topo_bit += 1;

        if bit_val {
            // Topology bit = 1: both children have proof leaves — find the split point and
            // recurse both sides.
            let mid = self.split_point(start, end, bit_pos);
            let left =
                self.traverse::<H>(start, mid, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            let right =
                self.traverse::<H>(mid, end, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            H::hash_internal(&left, &right)
        } else {
            // Topology bit = 0: only one side has proof leaves — use a sibling hash for the other.
            let goes_left = !self.leaves[start].key.get_msb(bit_pos);
            let sibling = *self.siblings[*sibling_idx];
            *sibling_idx += 1;

            let child =
                self.traverse::<H>(start, end, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            if goes_left {
                H::hash_internal(&child, &sibling)
            } else {
                H::hash_internal(&sibling, &child)
            }
        }
    }

    /// Finds the partition point where keys switch from bit=0 (left) to bit=1 (right).
    fn split_point(&self, start: usize, end: usize, bit_pos: usize) -> usize {
        let mut mid = start;
        while mid < end && !self.leaves[mid].key.get_msb(bit_pos) {
            mid += 1;
        }
        mid
    }
}
