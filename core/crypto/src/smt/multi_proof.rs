use alloc::vec::Vec;

use super::{leaf_entry::LeafEntry, node_key::get_key_bit};
use crate::{EMPTY_HASH, Hasher};

/// Size of a single leaf entry in the v2 wire format: depth(2) + key(32) + value_hash(32).
const LEAF_ENTRY_SIZE: usize = 66;

/// Owned multi-proof containing structured leaf entries, sibling hashes, and topology.
///
/// Produced by `VersionedTree::multi_proof()` on the host side. Provides direct access to proof
/// data and verification methods. Serialized into the v2 wire format via `encode()` for
/// transmission to the guest, where `DecodedMultiProof` (in `zk/abi`) provides zero-copy
/// verification.
pub struct MultiProof {
    /// Proof leaves sorted by key, each with its depth in the tree.
    pub leaves: Vec<LeafEntry>,
    /// Sibling hashes consumed in DFS order during verification.
    pub siblings: Vec<[u8; 32]>,
    /// Packed topology bitfield (LSB-first within each byte).
    pub topology: Vec<u8>,
}

impl MultiProof {
    /// Returns the number of leaves in the proof.
    pub fn n_leaves(&self) -> usize {
        self.leaves.len()
    }

    /// Returns the key of the leaf at index `i`.
    pub fn leaf_key(&self, i: usize) -> &[u8; 32] {
        &self.leaves[i].key
    }

    /// Returns the value hash of the leaf at index `i`.
    pub fn leaf_value_hash(&self, i: usize) -> &[u8; 32] {
        &self.leaves[i].value_hash
    }

    /// Returns the depth of the leaf at index `i`.
    pub fn leaf_depth(&self, i: usize) -> u16 {
        self.leaves[i].depth
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
        assert_eq!(updated_hashes.len(), self.n_leaves());
        self.compute_root_with::<H>(|i| updated_hashes[i])
    }

    /// Shared traversal logic parameterized on which value hash to use for each leaf.
    fn compute_root_with<H: Hasher>(&self, value_hash_fn: impl Fn(usize) -> [u8; 32]) -> [u8; 32] {
        if self.leaves.is_empty() {
            return EMPTY_HASH;
        }

        let mut sibling_idx = 0;
        let mut topo_bit = 0usize;
        self.traverse::<H>(0, self.leaves.len(), 0, &value_hash_fn, &mut sibling_idx, &mut topo_bit)
    }

    /// Recursive traversal of the shortcut-aware proof tree.
    fn traverse<H: Hasher>(
        &self,
        start: usize,
        end: usize,
        bit_pos: usize,
        value_hash_fn: &impl Fn(usize) -> [u8; 32],
        sibling_idx: &mut usize,
        topo_bit: &mut usize,
    ) -> [u8; 32] {
        if start == end {
            return EMPTY_HASH;
        }

        // Shortcut leaf check: if there's exactly one leaf and we've reached its declared depth,
        // compute the leaf hash directly instead of recursing further.
        if end - start == 1 {
            let depth = self.leaves[start].depth as usize;
            if bit_pos == depth {
                let key = &self.leaves[start].key;
                let vh = value_hash_fn(start);
                return H::hash_leaf(key, &vh);
            }
        }

        // Read the next topology bit to determine the structure at this level.
        let bit_val = get_topo_bit(&self.topology, *topo_bit);
        *topo_bit += 1;

        if bit_val {
            // Topology bit = 1: both children have proof leaves — find the split point.
            let mid = self.split_point(start, end, bit_pos);
            let left =
                self.traverse::<H>(start, mid, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            let right =
                self.traverse::<H>(mid, end, bit_pos + 1, value_hash_fn, sibling_idx, topo_bit);
            H::hash_internal(&left, &right)
        } else {
            // Topology bit = 0: only one side has proof leaves — use a sibling hash for the other.
            let goes_left = !get_key_bit(&self.leaves[start].key, bit_pos);
            let sibling = self.siblings[*sibling_idx];
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

    /// Finds the partition point where keys switch from bit=0 to bit=1.
    fn split_point(&self, start: usize, end: usize, bit_pos: usize) -> usize {
        let mut mid = start;
        while mid < end && !get_key_bit(&self.leaves[mid].key, bit_pos) {
            mid += 1;
        }
        mid
    }

    /// Encodes the multi-proof into a flat byte buffer in the v2 wire format.
    ///
    /// The output format matches the wire format consumed by `DecodedMultiProof::decode`.
    pub fn encode(&self) -> Vec<u8> {
        // Pre-compute the total buffer size to avoid reallocations.
        let total = 4
            + self.leaves.len() * LEAF_ENTRY_SIZE
            + 4
            + self.siblings.len() * 32
            + 4
            + self.topology.len();
        let mut buf = Vec::with_capacity(total);

        // Section 1: leaf entries — each is depth(2) + key(32) + value_hash(32) = 66 bytes.
        buf.extend_from_slice(&(self.leaves.len() as u32).to_le_bytes());
        for leaf in &self.leaves {
            buf.extend_from_slice(&leaf.depth.to_le_bytes());
            buf.extend_from_slice(&leaf.key);
            buf.extend_from_slice(&leaf.value_hash);
        }

        // Section 2: sibling hashes — each is 32 bytes.
        buf.extend_from_slice(&(self.siblings.len() as u32).to_le_bytes());
        for sibling in &self.siblings {
            buf.extend_from_slice(sibling);
        }

        // Section 3: topology bitfield — variable length.
        buf.extend_from_slice(&(self.topology.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.topology);

        debug_assert_eq!(buf.len(), total);
        buf
    }
}

/// Reads a single bit from a topology bitfield (LSB-first packing within each byte).
fn get_topo_bit(topology: &[u8], bit_index: usize) -> bool {
    let byte_idx = bit_index / 8;
    let bit_offset = bit_index % 8;
    if byte_idx >= topology.len() {
        return false;
    }
    (topology[byte_idx] >> bit_offset) & 1 == 1
}
