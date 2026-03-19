use vprogs_core_codec::{Bits, Error, Result};

use super::proof::Proof;
use crate::{EMPTY_HASH, Hasher, Node};

/// Mutable cursor state for recursive proof tree traversal.
pub(super) struct Traversal<'a, F> {
    /// The decoded proof being traversed.
    proof: &'a Proof<'a>,
    /// Returns the value hash for the leaf at the given index.
    value_hash_fn: F,
    /// Next sibling to consume (advances sequentially).
    sibling_idx: usize,
    /// Next topology bit to consume (advances sequentially).
    topo_bit: usize,
}

impl<'a, 'v, F: Fn(usize) -> &'v [u8; 32]> Traversal<'a, F> {
    /// Computes the root hash by traversing the proof tree.
    pub(super) fn compute_root<H: Hasher>(
        proof: &'a Proof<'a>,
        value_hash_fn: F,
    ) -> Result<[u8; 32]> {
        // Empty proof - no leaves to traverse.
        if proof.leaves.is_empty() {
            return Ok(EMPTY_HASH);
        }

        // Initialize traversal cursor and walk the proof tree from the root.
        let mut ctx = Self { proof, value_hash_fn, sibling_idx: 0, topo_bit: 0 };
        ctx.traverse::<H>(0, proof.leaves.len(), 0)
    }

    /// Recursive traversal over the `start..end` range of proof leaf indices.
    fn traverse<H: Hasher>(&mut self, start: usize, end: usize, level: u16) -> Result<[u8; 32]> {
        // Empty range - no leaves in this subtree.
        if start == end {
            return Ok(EMPTY_HASH);
        }

        // Shortcut leaf check: if there's exactly one leaf, check against its declared depth.
        if end - start == 1 {
            let leaf = &self.proof.leaves[start];
            if level > leaf.depth {
                return Err(Error::Decode("malformed proof"));
            } else if level == leaf.depth {
                return Ok(*Node::leaf::<H>(*leaf.key, *(self.value_hash_fn)(start)).hash());
            }
        }

        // Guard against malformed proofs that would recurse beyond the tree depth. Placed after
        // the shortcut leaf check since valid proofs can reach level 256 for full-depth leaves.
        if level as usize >= crate::DEPTH {
            return Err(Error::Decode("malformed proof"));
        }

        // Read the next topology bit to determine the structure at this level.
        let bit_val = self.proof.topology.get_lsb(self.topo_bit);
        self.topo_bit += 1;

        if bit_val {
            // Topology bit = 1: both children have proof leaves - find the split point and
            // recurse both sides.
            let mid = self.proof.split_point(start, end, level);
            let left = self.traverse::<H>(start, mid, level + 1)?;
            let right = self.traverse::<H>(mid, end, level + 1)?;
            Ok(*Node::internal::<H>(&left, &right).hash())
        } else {
            // Topology bit = 0: only one side has proof leaves - use a sibling hash for the other.
            let goes_left = !self.proof.leaves[start].key.get_msb(level as usize);
            let sibling = self
                .proof
                .siblings
                .get(self.sibling_idx)
                .ok_or(Error::Decode("malformed proof"))?;
            self.sibling_idx += 1;

            let child = self.traverse::<H>(start, end, level + 1)?;
            if goes_left {
                Ok(*Node::internal::<H>(&child, sibling).hash())
            } else {
                Ok(*Node::internal::<H>(sibling, &child).hash())
            }
        }
    }
}
