use alloc::{collections::BTreeMap, vec::Vec};

use crate::{
    EMPTY_LEAF_HASH, TREE_DEPTH,
    defaults::default_hashes,
    multi_proof::{LeafEntry, MultiProof},
};

/// Host-side sparse Merkle tree for maintaining state roots and generating multi-proofs.
pub struct SparseMerkleTree {
    /// Stores non-empty leaf hashes keyed by their 256-bit resource ID.
    leaves: BTreeMap<[u8; 32], [u8; 32]>,
    /// Cached root hash, invalidated on mutations.
    root_cache: Option<[u8; 32]>,
}

impl SparseMerkleTree {
    /// Creates a new empty tree.
    pub fn new() -> Self {
        Self { leaves: BTreeMap::new(), root_cache: None }
    }

    /// Inserts or updates a leaf. `data` is the raw account data (hashed with blake3).
    pub fn insert(&mut self, key: [u8; 32], data: &[u8]) {
        let leaf_hash = *blake3::hash(data).as_bytes();
        self.leaves.insert(key, leaf_hash);
        self.root_cache = None;
    }

    /// Deletes a leaf (sets it to empty).
    pub fn delete(&mut self, key: [u8; 32]) {
        self.leaves.remove(&key);
        self.root_cache = None;
    }

    /// Applies a batch of updates. `None` value means delete.
    pub fn batch_update(&mut self, updates: &[([u8; 32], Option<&[u8]>)]) {
        for &(key, data) in updates {
            match data {
                Some(d) => {
                    self.leaves.insert(key, *blake3::hash(d).as_bytes());
                }
                None => {
                    self.leaves.remove(&key);
                }
            }
        }
        self.root_cache = None;
    }

    /// Computes the current root hash.
    pub fn root(&mut self) -> [u8; 32] {
        if let Some(cached) = self.root_cache {
            return cached;
        }
        let defaults = default_hashes();
        let keys: Vec<[u8; 32]> = self.leaves.keys().copied().collect();
        let root = self.compute_subtree(&keys, 0, TREE_DEPTH, &defaults);
        self.root_cache = Some(root);
        root
    }

    /// Generates a multi-proof for the given set of resource IDs.
    ///
    /// Keys that are not in the tree will be included with `EMPTY_LEAF_HASH`.
    pub fn multi_proof(&self, keys: &[[u8; 32]]) -> MultiProof {
        let defaults = default_hashes();
        let mut sorted_keys: Vec<[u8; 32]> = keys.to_vec();
        sorted_keys.sort();
        sorted_keys.dedup();

        let leaves: Vec<LeafEntry> = sorted_keys
            .iter()
            .map(|k| LeafEntry {
                key: *k,
                leaf_hash: self.leaves.get(k).copied().unwrap_or(EMPTY_LEAF_HASH),
            })
            .collect();

        let mut siblings = Vec::new();
        let mut topology_bits = Vec::new();

        let proof_indices: Vec<usize> = (0..leaves.len()).collect();
        let tree_keys: Vec<[u8; 32]> = self.leaves.keys().copied().collect();
        self.build_proof(
            &leaves,
            &proof_indices,
            &tree_keys,
            0,
            TREE_DEPTH,
            &defaults,
            &mut siblings,
            &mut topology_bits,
        );

        // Pack topology bits into bytes.
        let topology = pack_bits(&topology_bits);

        MultiProof { leaves, siblings, topology }
    }

    /// Recursively build proof, collecting sibling hashes and topology bits.
    ///
    /// `proof_indices` — indices into `leaves` that fall within this subtree.
    /// `tree_keys` — actual tree keys within this subtree (for computing sibling hashes).
    #[allow(clippy::too_many_arguments)]
    fn build_proof(
        &self,
        leaves: &[LeafEntry],
        proof_indices: &[usize],
        tree_keys: &[[u8; 32]],
        bit_pos: usize,
        depth: usize,
        defaults: &[[u8; 32]; TREE_DEPTH + 1],
        siblings: &mut Vec<[u8; 32]>,
        topology_bits: &mut Vec<bool>,
    ) {
        if depth == 0 || proof_indices.is_empty() {
            return;
        }

        // Split proof indices by the current bit.
        let (left_proof, right_proof) = split_by_bit(proof_indices, leaves, bit_pos);
        // Split tree keys by the current bit.
        let (left_tree, right_tree) = split_keys_by_bit(tree_keys, bit_pos);

        if !left_proof.is_empty() && !right_proof.is_empty() {
            // Both sides have proof leaves — mark as split (topology bit = 1).
            topology_bits.push(true);
            self.build_proof(
                leaves,
                &left_proof,
                &left_tree,
                bit_pos + 1,
                depth - 1,
                defaults,
                siblings,
                topology_bits,
            );
            self.build_proof(
                leaves,
                &right_proof,
                &right_tree,
                bit_pos + 1,
                depth - 1,
                defaults,
                siblings,
                topology_bits,
            );
        } else {
            // Only one side has proof leaves — provide sibling hash (topology bit = 0).
            topology_bits.push(false);
            if left_proof.is_empty() {
                // Proof leaves go right, provide left subtree hash as sibling.
                let left_hash = self.compute_subtree(&left_tree, bit_pos + 1, depth - 1, defaults);
                siblings.push(left_hash);
                self.build_proof(
                    leaves,
                    &right_proof,
                    &right_tree,
                    bit_pos + 1,
                    depth - 1,
                    defaults,
                    siblings,
                    topology_bits,
                );
            } else {
                // Proof leaves go left, provide right subtree hash as sibling.
                let right_hash =
                    self.compute_subtree(&right_tree, bit_pos + 1, depth - 1, defaults);
                siblings.push(right_hash);
                self.build_proof(
                    leaves,
                    &left_proof,
                    &left_tree,
                    bit_pos + 1,
                    depth - 1,
                    defaults,
                    siblings,
                    topology_bits,
                );
            }
        }
    }

    /// Recursively compute the hash of a subtree containing the given keys.
    fn compute_subtree(
        &self,
        keys: &[[u8; 32]],
        bit_pos: usize,
        depth: usize,
        defaults: &[[u8; 32]; TREE_DEPTH + 1],
    ) -> [u8; 32] {
        if depth == 0 {
            return if keys.len() == 1 {
                self.leaves.get(&keys[0]).copied().unwrap_or(EMPTY_LEAF_HASH)
            } else if keys.is_empty() {
                EMPTY_LEAF_HASH
            } else {
                panic!("multiple keys at leaf level")
            };
        }

        if keys.is_empty() {
            return defaults[depth];
        }

        let (left_keys, right_keys) = split_keys_by_bit(keys, bit_pos);

        let left = self.compute_subtree(&left_keys, bit_pos + 1, depth - 1, defaults);
        let right = self.compute_subtree(&right_keys, bit_pos + 1, depth - 1, defaults);
        hash_pair(&left, &right)
    }
}

impl Default for SparseMerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the `bit_pos`-th bit of a 256-bit key (0 = MSB).
fn get_key_bit(key: &[u8; 32], bit_pos: usize) -> bool {
    let byte_idx = bit_pos / 8;
    let bit_offset = 7 - (bit_pos % 8);
    (key[byte_idx] >> bit_offset) & 1 == 1
}

/// Split proof leaf indices into left (bit=0) and right (bit=1).
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

/// Split raw keys into left (bit=0) and right (bit=1).
fn split_keys_by_bit(keys: &[[u8; 32]], bit_pos: usize) -> (Vec<[u8; 32]>, Vec<[u8; 32]>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for key in keys {
        if get_key_bit(key, bit_pos) {
            right.push(*key);
        } else {
            left.push(*key);
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

/// Pack a slice of bools into a byte vector (LSB first within each byte).
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let n_bytes = bits.len().div_ceil(8);
    let mut bytes = vec![0u8; n_bytes];
    for (i, &bit) in bits.iter().enumerate() {
        if bit {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_root() {
        let mut tree = SparseMerkleTree::new();
        assert_eq!(tree.root(), default_hashes()[TREE_DEPTH]);
    }

    #[test]
    fn single_insert_and_delete() {
        let mut tree = SparseMerkleTree::new();
        let key = [0u8; 32];

        tree.insert(key, b"hello");
        let root_after_insert = tree.root();
        assert_ne!(root_after_insert, default_hashes()[TREE_DEPTH]);

        tree.delete(key);
        assert_eq!(tree.root(), default_hashes()[TREE_DEPTH]);
    }

    #[test]
    fn multi_proof_single_leaf_verify() {
        let mut tree = SparseMerkleTree::new();
        let key = [0u8; 32];
        tree.insert(key, b"test data");

        let root = tree.root();
        let proof = tree.multi_proof(&[key]);

        assert!(proof.verify(root));
        assert_eq!(proof.leaves.len(), 1);
        assert_eq!(proof.leaves[0].key, key);
        assert_eq!(proof.leaves[0].leaf_hash, *blake3::hash(b"test data").as_bytes());
    }

    #[test]
    fn multi_proof_compute_root_after_update() {
        let mut tree = SparseMerkleTree::new();
        let key = [0u8; 32];
        tree.insert(key, b"old data");

        let proof = tree.multi_proof(&[key]);

        // Update the tree and get new root.
        tree.insert(key, b"new data");
        let expected_new_root = tree.root();

        // compute_root with updated hash should match.
        let new_hash = *blake3::hash(b"new data").as_bytes();
        let computed_root = proof.compute_root(&[new_hash]);
        assert_eq!(computed_root, expected_new_root);
    }

    #[test]
    fn multi_proof_two_leaves() {
        let mut tree = SparseMerkleTree::new();
        let mut key1 = [0u8; 32];
        key1[0] = 0x00; // bit 0 = 0 (goes left)
        let mut key2 = [0u8; 32];
        key2[0] = 0x80; // bit 0 = 1 (goes right)

        tree.insert(key1, b"data1");
        tree.insert(key2, b"data2");

        let root = tree.root();
        let proof = tree.multi_proof(&[key1, key2]);
        assert!(proof.verify(root));

        // Update both and verify.
        let new_hash1 = *blake3::hash(b"new1").as_bytes();
        let new_hash2 = *blake3::hash(b"new2").as_bytes();
        let computed = proof.compute_root(&[new_hash1, new_hash2]);

        tree.insert(key1, b"new1");
        tree.insert(key2, b"new2");
        assert_eq!(computed, tree.root());
    }

    #[test]
    fn multi_proof_nonexistent_key() {
        let mut tree = SparseMerkleTree::new();
        let key1 = [0u8; 32];
        tree.insert(key1, b"data");

        let nonexistent = [0xFFu8; 32];
        let root = tree.root();
        let proof = tree.multi_proof(&[key1, nonexistent]);

        assert!(proof.verify(root));
        assert_eq!(proof.leaves[1].leaf_hash, EMPTY_LEAF_HASH);
    }

    #[test]
    fn batch_update() {
        let mut tree = SparseMerkleTree::new();
        let k1 = [1u8; 32];
        let k2 = [2u8; 32];

        tree.batch_update(&[(k1, Some(b"a")), (k2, Some(b"b"))]);
        let root1 = tree.root();

        tree.batch_update(&[(k1, None), (k2, Some(b"c"))]);
        let root2 = tree.root();
        assert_ne!(root1, root2);

        // Verify k1 is deleted.
        let proof = tree.multi_proof(&[k1]);
        assert_eq!(proof.leaves[0].leaf_hash, EMPTY_LEAF_HASH);
    }

    #[test]
    fn adjacent_keys() {
        let mut tree = SparseMerkleTree::new();
        let mut k1 = [0u8; 32];
        let mut k2 = [0u8; 32];
        // Keys that differ only in the last bit.
        k1[31] = 0x00;
        k2[31] = 0x01;

        tree.insert(k1, b"left");
        tree.insert(k2, b"right");

        let root = tree.root();
        let proof = tree.multi_proof(&[k1, k2]);
        assert!(proof.verify(root));
    }
}
