use super::{
    NodeData, node_key::NodeKey, state_commitment::StateCommitment,
    tree_write_batch::TreeWriteBatch, versioned_tree::VersionedTree,
};
use crate::{Blake3Hasher, EMPTY_HASH};

/// Authenticated state store backed by a versioned Sparse Merkle Tree.
///
/// Provides version-aware node lookups and tree mutations. This trait is a supertrait of `Store`,
/// so all store implementations must provide SMT node lookups. Concrete implementations use
/// descending-version key encoding so that a single `prefix_iter` seek resolves "latest version
/// <= max_version" in O(1) I/O.
pub trait TreeStore {
    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)>;

    /// Returns the state root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn get_root(&self, version: u64) -> [u8; 32] {
        if version == 0 {
            return EMPTY_HASH;
        }
        self.get_node(&NodeKey::root(), version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }

    /// Commits state changes to the tree at the given version.
    ///
    /// Reads the previous root from the store, applies the state commitments as leaf mutations,
    /// writes the resulting nodes into `wb`, and returns the new root hash. No-op for empty diffs.
    fn commit_state_diffs<D: StateCommitment>(
        &self,
        wb: &mut impl TreeWriteBatch,
        version: u64,
        diffs: &[D],
    ) -> [u8; 32]
    where
        Self: Sized,
    {
        if diffs.is_empty() {
            return self.get_root(version.saturating_sub(1));
        }
        let prev_version = version.saturating_sub(1);
        let prev_root = self.get_root(prev_version);
        let mut tree = VersionedTree::<Blake3Hasher, _>::new_with(self, prev_version, prev_root);
        tree.update(wb, version, diffs)
    }
}
