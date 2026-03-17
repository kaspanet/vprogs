use vprogs_core_utils::Bits;

use super::multi_proof::Proof;
use crate::{EMPTY_HASH, Hasher};

/// Holds mutable cursor state for recursive proof tree traversal.
///
/// Siblings and topology bits are consumed sequentially during traversal. This struct tracks the
/// current position in both sequences, avoiding `&mut` parameters threaded through every call.
pub(super) struct Traversal<'a, F> {
    proof: &'a Proof<'a>,
    value_hash_fn: F,
    sibling_idx: usize,
    topo_bit: usize,
}

impl<'a, 'v, F: Fn(usize) -> &'v [u8; 32]> Traversal<'a, F> {
    /// Computes the root hash by traversing the proof tree.
    pub(super) fn compute_root<H: Hasher>(proof: &'a Proof<'a>, value_hash_fn: F) -> [u8; 32] {
        if proof.leaves.is_empty() {
            return EMPTY_HASH;
        }
        let mut ctx = Self { proof, value_hash_fn, sibling_idx: 0, topo_bit: 0 };
        ctx.traverse::<H>(0, proof.leaves.len(), 0)
    }

    /// Recursive traversal of the shortcut-aware proof tree.
    ///
    /// `start..end` is the range of leaf indices that fall within this subtree. Because proof
    /// leaves are sorted by key and MSB-first bit ordering matches lexicographic order, splitting
    /// by any bit always produces two contiguous sub-ranges — so ranges suffice (no Vec allocations
    /// needed).
    fn traverse<H: Hasher>(&mut self, start: usize, end: usize, level: u16) -> [u8; 32] {
        if start == end {
            return EMPTY_HASH;
        }

        // Shortcut leaf check: if there's exactly one leaf and we've reached its declared depth,
        // compute the leaf hash directly instead of recursing further.
        if end - start == 1 {
            let leaf = &self.proof.leaves[start];
            if level == leaf.depth {
                return H::hash_leaf(leaf.key, (self.value_hash_fn)(start));
            }
        }

        // Read the next topology bit to determine the structure at this level.
        let bit_val = self.proof.topology.get_lsb(self.topo_bit);
        self.topo_bit += 1;

        if bit_val {
            // Topology bit = 1: both children have proof leaves — find the split point and
            // recurse both sides.
            let mid = self.proof.split_point(start, end, level);
            let left = self.traverse::<H>(start, mid, level + 1);
            let right = self.traverse::<H>(mid, end, level + 1);
            H::hash_internal(&left, &right)
        } else {
            // Topology bit = 0: only one side has proof leaves — use a sibling hash for the other.
            let goes_left = !self.proof.leaves[start].key.get_msb(level as usize);
            let sibling = *self.proof.siblings[self.sibling_idx];
            self.sibling_idx += 1;

            let child = self.traverse::<H>(start, end, level + 1);
            if goes_left {
                H::hash_internal(&child, &sibling)
            } else {
                H::hash_internal(&sibling, &child)
            }
        }
    }
}
