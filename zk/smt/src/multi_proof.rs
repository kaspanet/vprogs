use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::{TREE_DEPTH, defaults::default_hashes};

/// A single leaf in a multi-proof.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct LeafEntry {
    /// The 256-bit key (resource_id).
    pub key: [u8; 32],
    /// The leaf hash (blake3 of data, or EMPTY_LEAF_HASH for empty/deleted).
    pub leaf_hash: [u8; 32],
}

/// A compact multi-proof for a set of leaves in a sparse Merkle tree.
///
/// Contains the leaf entries, the sibling hashes needed for verification, and a topology
/// bitfield encoding how siblings are shared among the included leaves.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct MultiProof {
    /// Leaf entries included in this proof, sorted by key.
    pub leaves: Vec<LeafEntry>,
    /// Sibling hashes needed for verification, consumed in order during traversal.
    pub siblings: Vec<[u8; 32]>,
    /// Topology bitfield: one bit per internal node visited during traversal.
    /// Bit = 1 means "descend into both children" (shared subtree);
    /// Bit = 0 means "consume next sibling hash".
    pub topology: Vec<u8>,
}

impl MultiProof {
    /// Verifies that the given leaves produce the expected root hash.
    pub fn verify(&self, expected_root: [u8; 32]) -> bool {
        self.compute_root_with(|i| self.leaves[i].leaf_hash) == expected_root
    }

    /// Recomputes the root using updated leaf hashes.
    ///
    /// `updated_hashes` must have the same length as `self.leaves` and provides the
    /// new leaf hash for each leaf (in the same order).
    pub fn compute_root(&self, updated_hashes: &[[u8; 32]]) -> [u8; 32] {
        assert_eq!(updated_hashes.len(), self.leaves.len());
        self.compute_root_with(|i| updated_hashes[i])
    }

    /// Shared traversal logic parameterized on which leaf hash to use.
    fn compute_root_with(&self, leaf_hash_fn: impl Fn(usize) -> [u8; 32]) -> [u8; 32] {
        let defaults = default_hashes();

        if self.leaves.is_empty() {
            return defaults[TREE_DEPTH];
        }

        let mut sibling_idx = 0;
        let mut topo_bit = 0usize;
        let indices: Vec<usize> = (0..self.leaves.len()).collect();
        self.traverse(
            &indices,
            0,
            TREE_DEPTH,
            &leaf_hash_fn,
            &defaults,
            &mut sibling_idx,
            &mut topo_bit,
        )
    }

    /// Recursive traversal of the tree.
    ///
    /// `indices` — subset of leaf indices that fall within this subtree.
    /// `bit_pos` — current bit position in the key (0 = MSB, 255 = LSB).
    /// `depth` — remaining depth (256 = root, 0 = leaf level).
    #[allow(clippy::too_many_arguments)]
    fn traverse(
        &self,
        indices: &[usize],
        bit_pos: usize,
        depth: usize,
        leaf_hash_fn: &impl Fn(usize) -> [u8; 32],
        defaults: &[[u8; 32]; TREE_DEPTH + 1],
        sibling_idx: &mut usize,
        topo_bit: &mut usize,
    ) -> [u8; 32] {
        if depth == 0 {
            // At leaf level — should have exactly one leaf.
            debug_assert_eq!(indices.len(), 1);
            return leaf_hash_fn(indices[0]);
        }

        if indices.is_empty() {
            return defaults[depth];
        }

        // Check topology bit to determine if we descend into both children.
        let bit_val = self.get_topo_bit(*topo_bit);
        *topo_bit += 1;

        if bit_val {
            // Both children have leaves — split and recurse.
            let (left_indices, right_indices) = split_by_bit(indices, &self.leaves, bit_pos);
            let left = self.traverse(
                &left_indices,
                bit_pos + 1,
                depth - 1,
                leaf_hash_fn,
                defaults,
                sibling_idx,
                topo_bit,
            );
            let right = self.traverse(
                &right_indices,
                bit_pos + 1,
                depth - 1,
                leaf_hash_fn,
                defaults,
                sibling_idx,
                topo_bit,
            );
            hash_pair(&left, &right)
        } else {
            // Only one child has leaves — use sibling hash for the other.
            let goes_left = !get_key_bit(&self.leaves[indices[0]].key, bit_pos);
            let sibling = self.siblings[*sibling_idx];
            *sibling_idx += 1;

            if goes_left {
                let child = self.traverse(
                    indices,
                    bit_pos + 1,
                    depth - 1,
                    leaf_hash_fn,
                    defaults,
                    sibling_idx,
                    topo_bit,
                );
                hash_pair(&child, &sibling)
            } else {
                let child = self.traverse(
                    indices,
                    bit_pos + 1,
                    depth - 1,
                    leaf_hash_fn,
                    defaults,
                    sibling_idx,
                    topo_bit,
                );
                hash_pair(&sibling, &child)
            }
        }
    }

    fn get_topo_bit(&self, bit_index: usize) -> bool {
        let byte_idx = bit_index / 8;
        let bit_offset = bit_index % 8;
        if byte_idx >= self.topology.len() {
            return false;
        }
        (self.topology[byte_idx] >> bit_offset) & 1 == 1
    }
}

/// Get the `bit_pos`-th bit of a 256-bit key (0 = MSB).
fn get_key_bit(key: &[u8; 32], bit_pos: usize) -> bool {
    let byte_idx = bit_pos / 8;
    let bit_offset = 7 - (bit_pos % 8);
    (key[byte_idx] >> bit_offset) & 1 == 1
}

/// Split indices into left (bit=0) and right (bit=1) children.
fn split_by_bit(
    indices: &[usize],
    leaves: &[LeafEntry],
    bit_pos: usize,
) -> (Vec<usize>, Vec<usize>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for &i in indices {
        if get_key_bit(&leaves[i].key, bit_pos) {
            right.push(i);
        } else {
            left.push(i);
        }
    }
    (left, right)
}

/// Compute blake3(left || right).
fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    *blake3::hash(&buf).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EMPTY_LEAF_HASH;

    #[test]
    fn empty_proof_returns_default_root() {
        let defaults = default_hashes();
        let proof = MultiProof { leaves: Vec::new(), siblings: Vec::new(), topology: Vec::new() };
        assert_eq!(proof.compute_root_with(|_| unreachable!()), defaults[TREE_DEPTH]);
    }

    #[test]
    fn single_leaf_proof() {
        let defaults = default_hashes();
        let data_hash = *blake3::hash(b"hello").as_bytes();
        let key = [0u8; 32];

        // Build proof: 256 sibling hashes consumed top-down (root level first).
        // For all-zeros key, leaf always goes left. Sibling at depth d is
        // DEFAULT_HASHES[d-1] (an empty subtree of height d-1).
        let mut siblings = Vec::new();
        for i in (0..TREE_DEPTH).rev() {
            siblings.push(defaults[i]);
        }
        // Topology: 256 bits, all 0 (never split, always use sibling).
        let topology = vec![0u8; TREE_DEPTH.div_ceil(8)];

        let proof = MultiProof {
            leaves: vec![LeafEntry { key, leaf_hash: data_hash }],
            siblings,
            topology,
        };

        // Compute root manually bottom-up: hash with defaults[0], then [1], etc.
        let mut current = data_hash;
        for default in defaults.iter().take(TREE_DEPTH) {
            current = hash_pair(&current, default);
        }

        assert!(proof.verify(current));

        // Update leaf hash and verify new root.
        let new_hash = *blake3::hash(b"world").as_bytes();
        let new_root = proof.compute_root(&[new_hash]);

        let mut expected = new_hash;
        for default in defaults.iter().take(TREE_DEPTH) {
            expected = hash_pair(&expected, default);
        }
        assert_eq!(new_root, expected);
    }

    #[test]
    fn single_leaf_delete() {
        let defaults = default_hashes();
        let data_hash = *blake3::hash(b"data").as_bytes();
        let key = [0u8; 32];

        let mut siblings = Vec::new();
        for i in (0..TREE_DEPTH).rev() {
            siblings.push(defaults[i]);
        }
        let topology = vec![0u8; TREE_DEPTH.div_ceil(8)];

        let proof = MultiProof {
            leaves: vec![LeafEntry { key, leaf_hash: data_hash }],
            siblings,
            topology,
        };

        // Deleting the only leaf should produce the empty tree root.
        let new_root = proof.compute_root(&[EMPTY_LEAF_HASH]);
        assert_eq!(new_root, defaults[TREE_DEPTH]);
    }
}
