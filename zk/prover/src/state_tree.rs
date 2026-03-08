use vprogs_zk_smt::{EMPTY_LEAF_HASH, MultiProof, SparseMerkleTree};

/// Host-side state tree wrapper for maintaining the Merkle root and generating proofs.
///
/// Wraps `SparseMerkleTree` and provides batch-oriented operations for the proving pipeline.
pub struct StateTree {
    inner: SparseMerkleTree,
}

impl StateTree {
    /// Creates a new empty state tree.
    pub fn new() -> Self {
        Self { inner: SparseMerkleTree::new() }
    }

    /// Returns the current root hash.
    pub fn root(&mut self) -> [u8; 32] {
        self.inner.root()
    }

    /// Generates a multi-proof for the given set of resource IDs.
    pub fn multi_proof(&self, keys: &[[u8; 32]]) -> MultiProof {
        self.inner.multi_proof(keys)
    }

    /// Applies account mutations after a batch commits.
    ///
    /// Each entry is `(resource_id, new_data)` where `None` means deletion.
    pub fn apply_mutations(&mut self, mutations: &[([u8; 32], Option<&[u8]>)]) {
        self.inner.batch_update(mutations);
    }

    /// Returns a mutable reference to the inner SMT for direct manipulation.
    pub fn inner_mut(&mut self) -> &mut SparseMerkleTree {
        &mut self.inner
    }

    /// Returns the leaf hash for a given key, or `EMPTY_LEAF_HASH` if not present.
    pub fn leaf_hash(&self, key: &[u8; 32]) -> [u8; 32] {
        // Use multi_proof to get the leaf hash (it handles missing keys).
        let proof = self.inner.multi_proof(&[*key]);
        if proof.leaves.is_empty() { EMPTY_LEAF_HASH } else { proof.leaves[0].leaf_hash }
    }
}

impl Default for StateTree {
    fn default() -> Self {
        Self::new()
    }
}
