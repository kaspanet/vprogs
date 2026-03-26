use alloc::vec::Vec;

use vprogs_core_codec::Result;
use vprogs_core_types::ResourceId;

use crate::{
    Commitment, EMPTY_HASH, Hasher, Key, Node, WriteBatch, proving::ProofBuilder, updater::Updater,
};

/// Number of levels in the tree (256-bit keys).
pub const DEPTH: usize = 256;

/// Versioned Sparse Merkle Tree with shortcut leaves, pruning, and multi-proofs.
///
/// Implementors only need to provide `node`, `prune`, and `rollback`; all tree operations
/// (commits, proofs, root lookups) are default methods.
pub trait Tree: Sized {
    /// The hash function used for node and leaf hashing.
    type Hasher: Hasher;

    // -- Required methods (implementors must provide these) --

    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn node(&self, key: &Key, max_version: u64) -> Option<(u64, Node)>;

    /// Prunes stale nodes for the given version, deleting superseded nodes and their stale markers.
    fn prune(&self, wb: &mut impl WriteBatch, version: u64);

    /// Rolls back a committed tree update at the given version.
    ///
    /// Unlike `prune` (which deletes superseded nodes), this undoes the version itself: deletes
    /// nodes written at `version` and removes stale markers so old nodes become current again.
    fn rollback(&self, wb: &mut impl WriteBatch, version: u64);

    // -- Default methods --

    /// Returns the state root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn root(&self, version: u64) -> [u8; 32] {
        // Version 0 is pre-genesis - no tree exists yet.
        if version == 0 {
            return EMPTY_HASH;
        }

        // Look up the root node and extract its hash.
        self.node(&Key::ROOT, version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }

    /// Commits state diffs to the tree at the given version, returning the new root hash.
    ///
    /// No-op for empty diffs - returns the previous version's root. Panics if `version` is 0
    /// (version 0 is reserved as pre-genesis).
    fn update(
        &self,
        wb: &mut impl WriteBatch,
        commitments: Vec<Commitment>,
        version: u64,
    ) -> [u8; 32] {
        assert!(version > 0, "version 0 is reserved as pre-genesis");

        // Empty commitments produce no tree changes - carry forward the previous root.
        if commitments.is_empty() {
            return self.root(version - 1);
        }

        // Apply leaf mutations and return the new root hash.
        Updater::apply(self, wb, version, commitments)
    }

    /// Proves the state of the given keys at a specific version.
    ///
    /// Returns the wire-encoded proof (decode with `Proof::decode()`) and a leaf order mapping
    /// where `leaf_order[leaf_pos]` is the original input index of that leaf. The version must
    /// not have been pruned. Returns an error if keys contain duplicates.
    fn prove(&self, keys: &[ResourceId], version: u64) -> Result<(Vec<u8>, Vec<u32>)> {
        ProofBuilder::build(self, version, keys)
    }
}
