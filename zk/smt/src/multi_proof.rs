use alloc::vec::Vec;

use crate::{TREE_DEPTH, defaults::default_hashes};

/// Size of a single leaf entry in the wire format: key(32) + leaf_hash(32).
pub const LEAF_ENTRY_SIZE: usize = 64;

/// Zero-copy view of a multi-proof, borrowing from a flat byte buffer.
///
/// Wire format:
/// - `n_leaves: u32` + `[key(32) + leaf_hash(32)] × n_leaves`
/// - `n_siblings: u32` + `[u8; 32] × n_siblings`
/// - `topology_len: u32` + `topology_bytes`
pub struct MultiProof<'a> {
    buf: &'a [u8],
    n_leaves: u32,
    siblings_offset: usize,
    topology_offset: usize,
    topology_len: usize,
}

impl<'a> MultiProof<'a> {
    /// Decodes a multi-proof from a flat byte buffer.
    pub fn decode(buf: &'a [u8]) -> Self {
        let n_leaves = u32::from_le_bytes(buf[0..4].try_into().expect("truncated n_leaves"));
        let leaves_end = 4 + (n_leaves as usize) * LEAF_ENTRY_SIZE;

        let n_siblings = u32::from_le_bytes(
            buf[leaves_end..leaves_end + 4].try_into().expect("truncated n_siblings"),
        );
        let siblings_offset = leaves_end + 4;
        let siblings_end = siblings_offset + (n_siblings as usize) * 32;

        let topology_len = u32::from_le_bytes(
            buf[siblings_end..siblings_end + 4].try_into().expect("truncated topology_len"),
        ) as usize;
        let topology_offset = siblings_end + 4;

        Self { buf, n_leaves, siblings_offset, topology_offset, topology_len }
    }

    /// Returns the number of leaves.
    pub fn n_leaves(&self) -> usize {
        self.n_leaves as usize
    }

    /// Returns the key of the leaf at index `i`.
    pub fn leaf_key(&self, i: usize) -> &[u8; 32] {
        let offset = 4 + i * LEAF_ENTRY_SIZE;
        self.buf[offset..offset + 32].try_into().expect("truncated leaf key")
    }

    /// Returns the leaf hash at index `i`.
    pub fn leaf_hash(&self, i: usize) -> &[u8; 32] {
        let offset = 4 + i * LEAF_ENTRY_SIZE + 32;
        self.buf[offset..offset + 32].try_into().expect("truncated leaf hash")
    }

    /// Returns the sibling hash at index `i`.
    fn sibling(&self, i: usize) -> &[u8; 32] {
        let offset = self.siblings_offset + i * 32;
        self.buf[offset..offset + 32].try_into().expect("truncated sibling")
    }

    /// Returns the topology bytes.
    fn topology(&self) -> &[u8] {
        &self.buf[self.topology_offset..self.topology_offset + self.topology_len]
    }

    /// Verifies that the leaves produce the expected root hash.
    pub fn verify(&self, expected_root: [u8; 32]) -> bool {
        self.compute_root_with(|i| *self.leaf_hash(i)) == expected_root
    }

    /// Recomputes the root using updated leaf hashes.
    ///
    /// `updated_hashes` must have the same length as `n_leaves()` and provides the
    /// new leaf hash for each leaf (in the same order).
    pub fn compute_root(&self, updated_hashes: &[[u8; 32]]) -> [u8; 32] {
        assert_eq!(updated_hashes.len(), self.n_leaves());
        self.compute_root_with(|i| updated_hashes[i])
    }

    /// Shared traversal logic parameterized on which leaf hash to use.
    fn compute_root_with(&self, leaf_hash_fn: impl Fn(usize) -> [u8; 32]) -> [u8; 32] {
        let defaults = default_hashes();

        if self.n_leaves() == 0 {
            return defaults[TREE_DEPTH];
        }

        let mut sibling_idx = 0;
        let mut topo_bit = 0usize;
        let indices: Vec<usize> = (0..self.n_leaves()).collect();
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
    /// `level` — current bit position in the key (0 = MSB, 255 = LSB).
    /// `depth` — remaining depth (256 = root, 0 = leaf level).
    #[allow(clippy::too_many_arguments)]
    fn traverse(
        &self,
        indices: &[usize],
        level: usize,
        depth: usize,
        leaf_hash_fn: &impl Fn(usize) -> [u8; 32],
        defaults: &[[u8; 32]; TREE_DEPTH + 1],
        sibling_idx: &mut usize,
        topo_bit: &mut usize,
    ) -> [u8; 32] {
        if depth == 0 {
            debug_assert_eq!(indices.len(), 1);
            return leaf_hash_fn(indices[0]);
        }

        if indices.is_empty() {
            return defaults[depth];
        }

        let bit_val = self.get_topo_bit(*topo_bit);
        *topo_bit += 1;

        if bit_val {
            // Both children have leaves — split and recurse.
            let (left_indices, right_indices) = self.split_by_bit(indices, level);
            let left = self.traverse(
                &left_indices,
                level + 1,
                depth - 1,
                leaf_hash_fn,
                defaults,
                sibling_idx,
                topo_bit,
            );
            let right = self.traverse(
                &right_indices,
                level + 1,
                depth - 1,
                leaf_hash_fn,
                defaults,
                sibling_idx,
                topo_bit,
            );
            hash_pair(&left, &right)
        } else {
            // Only one child has leaves — use sibling hash for the other.
            let goes_left = !get_key_bit(self.leaf_key(indices[0]), level);
            let sibling = *self.sibling(*sibling_idx);
            *sibling_idx += 1;

            let child = self.traverse(
                indices,
                level + 1,
                depth - 1,
                leaf_hash_fn,
                defaults,
                sibling_idx,
                topo_bit,
            );
            if goes_left { hash_pair(&child, &sibling) } else { hash_pair(&sibling, &child) }
        }
    }

    fn split_by_bit(&self, indices: &[usize], level: usize) -> (Vec<usize>, Vec<usize>) {
        let mut left = Vec::new();
        let mut right = Vec::new();
        for &i in indices {
            if get_key_bit(self.leaf_key(i), level) {
                right.push(i);
            } else {
                left.push(i);
            }
        }
        (left, right)
    }

    fn get_topo_bit(&self, bit_index: usize) -> bool {
        let byte_idx = bit_index / 8;
        let bit_offset = bit_index % 8;
        let topology = self.topology();
        if byte_idx >= topology.len() {
            return false;
        }
        (topology[byte_idx] >> bit_offset) & 1 == 1
    }
}

/// Encodes a multi-proof into a flat byte buffer.
///
/// `leaves` is a slice of `(key, leaf_hash)` pairs, sorted by key.
pub fn encode_multi_proof(
    leaves: &[([u8; 32], [u8; 32])],
    siblings: &[[u8; 32]],
    topology: &[u8],
) -> Vec<u8> {
    let total = 4 + leaves.len() * LEAF_ENTRY_SIZE + 4 + siblings.len() * 32 + 4 + topology.len();
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
    for (key, leaf_hash) in leaves {
        buf.extend_from_slice(key);
        buf.extend_from_slice(leaf_hash);
    }

    buf.extend_from_slice(&(siblings.len() as u32).to_le_bytes());
    for sibling in siblings {
        buf.extend_from_slice(sibling);
    }

    buf.extend_from_slice(&(topology.len() as u32).to_le_bytes());
    buf.extend_from_slice(topology);

    debug_assert_eq!(buf.len(), total);
    buf
}

/// Get the `level`-th bit of a 256-bit key (0 = MSB).
fn get_key_bit(key: &[u8; 32], level: usize) -> bool {
    let byte_idx = level / 8;
    let bit_offset = 7 - (level % 8);
    (key[byte_idx] >> bit_offset) & 1 == 1
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
        let encoded = encode_multi_proof(&[], &[], &[]);
        let proof = MultiProof::decode(&encoded);
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

        let encoded = encode_multi_proof(&[(key, data_hash)], &siblings, &topology);
        let proof = MultiProof::decode(&encoded);

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

        let encoded = encode_multi_proof(&[(key, data_hash)], &siblings, &topology);
        let proof = MultiProof::decode(&encoded);

        // Deleting the only leaf should produce the empty tree root.
        let new_root = proof.compute_root(&[EMPTY_LEAF_HASH]);
        assert_eq!(new_root, defaults[TREE_DEPTH]);
    }
}
