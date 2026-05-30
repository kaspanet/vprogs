use vprogs_core_codec::{Bits, Error, Result};
use vprogs_core_hashing::Hasher;

use crate::{EMPTY_HASH, Node, proving::Proof};

/// Mutable cursor state for recursive proof tree traversal.
pub(crate) struct Traversal<'a, F> {
    /// The decoded proof being traversed.
    proof: &'a Proof<'a>,
    /// Returns the leaf hash(es) at the given index.
    leaf_hash_fn: F,
    /// Next sibling to consume (advances sequentially).
    sibling_idx: usize,
    /// Next topology bit to consume (advances sequentially).
    topo_bit: usize,
}

impl<'a, F> Traversal<'a, F> {
    /// Constructs a fresh traversal cursor positioned at the start of `proof`.
    pub(crate) fn new(proof: &'a Proof<'a>, leaf_hash_fn: F) -> Self {
        Self { proof, leaf_hash_fn, sibling_idx: 0, topo_bit: 0 }
    }

    /// Terminal value at this position, or `None` to continue the recursion.
    fn terminal_value<T>(
        &mut self,
        start: usize,
        end: usize,
        level: u16,
        empty: T,
    ) -> Result<Option<T>>
    where
        F: FnMut(usize) -> Result<T>,
    {
        // Empty subtree - emit the empty value.
        if start == end {
            return Ok(Some(empty));
        }

        // Shortcut leaf at its declared depth - delegate to the caller's leaf-hash function.
        if self.at_shortcut_depth(start, end, level)? {
            return Ok(Some((self.leaf_hash_fn)(start)?));
        }

        // Recursion has descended past the tree's max depth - malformed proof.
        if level as usize >= crate::DEPTH {
            return Err(Error::Decode("malformed proof"));
        }

        Ok(None)
    }

    /// True if the single leaf at `start` has reached its shortcut depth (errors if past it).
    fn at_shortcut_depth(&self, start: usize, end: usize, level: u16) -> Result<bool> {
        // Only single-leaf ranges can sit at a shortcut depth.
        if end - start != 1 {
            return Ok(false);
        }

        // Read the leaf's declared depth and compare it to our current recursion level.
        let depth = self.proof.leaves[start].depth.get();
        if level > depth {
            return Err(Error::Decode("malformed proof"));
        }

        Ok(level == depth)
    }

    /// Consumes the next topology bit (LSB-first).
    fn next_topology_bit(&mut self) -> bool {
        self.topo_bit += 1;
        self.proof.topology.get_lsb(self.topo_bit - 1)
    }

    /// Partition point where leaf keys switch from bit=0 (left) to bit=1 (right) at `level`.
    fn split_point(&self, start: usize, end: usize, level: u16) -> usize {
        start
            + self.proof.leaves[start..end].partition_point(|leaf| {
                !self.proof.keys[leaf.key_idx.get() as usize].get_msb(level as usize)
            })
    }

    /// Whether the proof leaf at `start` lies on the left side at `level` (bit = 0).
    fn leaf_goes_left(&self, start: usize, level: u16) -> bool {
        !self.proof.keys[self.proof.leaves[start].key_idx.get() as usize].get_msb(level as usize)
    }

    /// Consumes the next off-path sibling hash, or errs if the wire ran out.
    fn next_sibling(&mut self) -> Result<&'a [u8; 32]> {
        let sibling =
            self.proof.siblings.get(self.sibling_idx).ok_or(Error::Decode("malformed proof"))?;
        self.sibling_idx += 1;
        Ok(sibling)
    }

    /// Combines a child hash with its off-path sibling, ordering by which side the child lies on.
    fn combine_with_sibling<H: Hasher>(
        child: &[u8; 32],
        sibling: &[u8; 32],
        child_goes_left: bool,
    ) -> [u8; 32] {
        if child_goes_left {
            Node::hash_internal::<H>(child, sibling)
        } else {
            Node::hash_internal::<H>(sibling, child)
        }
    }
}

impl<'a, F: FnMut(usize) -> Result<[u8; 32]>> Traversal<'a, F> {
    /// Recursive traversal over the `start..end` range of proof leaf indices.
    pub(crate) fn traverse<H: Hasher>(
        &mut self,
        start: usize,
        end: usize,
        level: u16,
    ) -> Result<[u8; 32]> {
        if let Some(value) = self.terminal_value(start, end, level, EMPTY_HASH)? {
            return Ok(value);
        }

        if self.next_topology_bit() {
            // Both children have proof leaves - split and recurse both sides.
            let mid = self.split_point(start, end, level);
            let left = self.traverse::<H>(start, mid, level + 1)?;
            let right = self.traverse::<H>(mid, end, level + 1)?;

            Ok(Node::hash_internal::<H>(&left, &right))
        } else {
            // One side has proof leaves, the other is summarized by a sibling hash.
            let goes_left = self.leaf_goes_left(start, level);
            let sibling = self.next_sibling()?;
            let child = self.traverse::<H>(start, end, level + 1)?;

            Ok(Self::combine_with_sibling::<H>(&child, sibling, goes_left))
        }
    }
}

impl<'a, F: FnMut(usize) -> Result<([u8; 32], [u8; 32])>> Traversal<'a, F> {
    /// Paired walk returning `(prev, post)` per node; reuses prev when children are unchanged.
    pub(crate) fn traverse_pair<H: Hasher>(
        &mut self,
        start: usize,
        end: usize,
        level: u16,
    ) -> Result<([u8; 32], [u8; 32])> {
        if let Some(value) = self.terminal_value(start, end, level, (EMPTY_HASH, EMPTY_HASH))? {
            return Ok(value);
        }

        if self.next_topology_bit() {
            // Both children have proof leaves.
            let mid = self.split_point(start, end, level);
            let (prev_left, post_left) = self.traverse_pair::<H>(start, mid, level + 1)?;
            let (prev_right, post_right) = self.traverse_pair::<H>(mid, end, level + 1)?;
            let prev = Node::hash_internal::<H>(&prev_left, &prev_right);
            let post = if prev_left == post_left && prev_right == post_right {
                prev
            } else {
                Node::hash_internal::<H>(&post_left, &post_right)
            };

            Ok((prev, post))
        } else {
            // One side has proof leaves, the other is an off-path sibling (unchanged pre/post).
            let goes_left = self.leaf_goes_left(start, level);
            let sibling = self.next_sibling()?;
            let (prev_child, post_child) = self.traverse_pair::<H>(start, end, level + 1)?;
            let prev = Self::combine_with_sibling::<H>(&prev_child, sibling, goes_left);
            let post = if prev_child == post_child {
                prev
            } else {
                Self::combine_with_sibling::<H>(&post_child, sibling, goes_left)
            };

            Ok((prev, post))
        }
    }
}
