use alloc::vec::Vec;

use crate::{DecodedMultiProof, Hasher, decoded_multi_proof::LEAF_ENTRY_SIZE, leaf_entry::LeafEntry};

/// Owned multi-proof containing structured leaf entries, sibling hashes, and topology.
///
/// Produced by `VersionedTree::multi_proof()` on the host side. Provides direct access to proof
/// data and verification methods. Serialized into the v2 wire format via `encode()` for
/// transmission to the guest, where `DecodedMultiProof` provides zero-copy verification.
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
        let encoded = self.encode();
        DecodedMultiProof::<H>::decode(&encoded).verify(expected_root)
    }

    /// Recomputes the root using updated value hashes.
    ///
    /// `updated_hashes` must have the same length as `n_leaves()` and provides the new value hash
    /// for each leaf (in the same order as in the proof). This enables computing the post-update
    /// root without re-reading the tree.
    pub fn compute_root<H: Hasher>(&self, updated_hashes: &[[u8; 32]]) -> [u8; 32] {
        let encoded = self.encode();
        DecodedMultiProof::<H>::decode(&encoded).compute_root(updated_hashes)
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
