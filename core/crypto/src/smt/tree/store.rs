use alloc::vec::Vec;

use super::{
    key::Key, state_commitment::StateCommitment, update::TreeUpdate, write_batch::WriteBatch,
};
use crate::{EMPTY_HASH, Node, smt::proof::builder::ProofBuilder};

/// Authenticated state store backed by a versioned Sparse Merkle Tree.
///
/// Provides version-aware node lookups and tree mutations. Implementors only need to provide
/// `get_node`; all tree operations (commits, proofs) are default methods. Concrete implementations
/// use descending-version key encoding so that a single `prefix_iter` seek resolves "latest
/// version <= max_version" in O(1) I/O.
pub trait Store {
    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn get_node(&self, key: &Key, max_version: u64) -> Option<(u64, Node)>;

    /// Returns the state root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn get_root(&self, version: u64) -> [u8; 32] {
        if version == 0 {
            return EMPTY_HASH;
        }
        self.get_node(&Key::root(), version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }

    /// Commits state changes to the tree at the given version.
    ///
    /// Reads the previous root from the store, applies the state commitments as leaf mutations,
    /// writes the resulting nodes into `wb`, and returns the new root hash. No-op for empty diffs.
    fn commit_state_diffs<D>(&self, wb: &mut impl WriteBatch, version: u64, diffs: &[D]) -> [u8; 32]
    where
        Self: Sized,
        for<'a> StateCommitment: From<&'a D>,
    {
        if diffs.is_empty() {
            return self.get_root(version.saturating_sub(1));
        }
        TreeUpdate::apply(self, wb, version, diffs)
    }

    /// Prunes stale nodes for the given version.
    ///
    /// Iterates all stale markers recorded at `version`, deletes the corresponding superseded nodes
    /// and the stale markers themselves. Implementors use their storage-specific encoding to locate
    /// and remove entries.
    fn prune_version(&self, wb: &mut impl WriteBatch, version: u64);

    /// Rolls back a committed tree update at the given version.
    ///
    /// Deletes all nodes written at `version` and removes the stale markers so the
    /// previously-superseded nodes become current again. Unlike `prune_version` (which deletes
    /// *superseded* nodes), this undoes the version itself.
    fn rollback_version(&self, wb: &mut impl WriteBatch, version: u64);

    /// Generates a multi-proof for the given keys at a specific version.
    ///
    /// Walks the persistent node store to collect sibling hashes and leaf depths. Returns the
    /// proof encoded in the wire format, ready for transmission. Decode with `Proof::decode()`
    /// for verification. The version must not have been pruned.
    fn generate_proof(&self, version: u64, keys: &[[u8; 32]]) -> Vec<u8>
    where
        Self: Sized,
    {
        ProofBuilder::build(self, version, keys)
    }
}
