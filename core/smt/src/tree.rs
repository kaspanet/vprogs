use alloc::vec::Vec;

use crate::{
    EMPTY_HASH, Hasher, Node, commitment::Commitment, key::Key, proving::builder::ProofBuilder,
    updater::Updater, write_batch::WriteBatch,
};

/// Versioned Sparse Merkle Tree backed by an authenticated state store.
///
/// Implementors only need to provide `get_node`, `prune`, and `rollback`; all tree operations
/// (commits, proofs, root lookups) are default methods.
/// Number of levels in the tree (256-bit keys).
pub const DEPTH: usize = 256;

pub trait Tree {
    /// The hash function used for node and leaf hashing.
    type Hasher: Hasher;

    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn get_node(&self, key: &Key, max_version: u64) -> Option<(u64, Node)>;

    /// Returns the state root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn get_root(&self, version: u64) -> [u8; 32] {
        // Version 0 is pre-genesis - no tree exists yet.
        if version == 0 {
            return EMPTY_HASH;
        }

        // Look up the root node and extract its hash.
        self.get_node(&Key::root(), version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }

    /// Commits state diffs to the tree at the given version, returning the new root hash.
    ///
    /// No-op for empty diffs - returns the previous version's root.
    fn commit_diffs<D>(&self, wb: &mut impl WriteBatch, version: u64, diffs: &[D]) -> [u8; 32]
    where
        Self: Sized,
        for<'a> Commitment: From<&'a D>,
    {
        // Empty diffs produce no tree changes - carry forward the previous root.
        if diffs.is_empty() {
            return self.get_root(version.saturating_sub(1));
        }

        // Apply leaf mutations and return the new root hash.
        Updater::apply(self, wb, version, diffs)
    }

    /// Prunes stale nodes for the given version, deleting superseded nodes and their stale markers.
    fn prune(&self, wb: &mut impl WriteBatch, version: u64);

    /// Rolls back a committed tree update at the given version.
    ///
    /// Unlike `prune` (which deletes superseded nodes), this undoes the version itself: deletes
    /// nodes written at `version` and removes stale markers so old nodes become current again.
    fn rollback(&self, wb: &mut impl WriteBatch, version: u64);

    /// Proves the state of the given keys at a specific version, returning a wire-encoded proof.
    ///
    /// Decode with `Proof::decode()` for verification. The version must not have been pruned.
    fn prove(&self, keys: &[[u8; 32]], version: u64) -> Vec<u8>
    where
        Self: Sized,
    {
        ProofBuilder::build(self, version, keys)
    }
}
