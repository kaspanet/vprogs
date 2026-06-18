use alloc::vec::Vec;

use vprogs_core_codec::Result;
use vprogs_core_hashing::Hasher;
use vprogs_core_types::{CanonicalChain, ResourceId};

use crate::{
    Commitment, EMPTY_HASH, Key, Node, WriteBatch, proving::ProofBuilder, updater::Updater,
};

/// Number of levels in the tree (256-bit keys).
pub const DEPTH: usize = 256;

/// Versioned, fork-aware Sparse Merkle Tree with shortcut leaves, pruning, and multi-proofs.
///
/// Each node is tagged with the `block_hash` of the fork that wrote it, so competing forks coexist
/// at the same version. Reads take a [`CanonicalChain`] oracle and return the latest node whose
/// `(version, block_hash)` the oracle deems canonical; pass [`NoOpCanonicalChain`] to disable
/// filtering. Implementors provide `node` and `prune`; all other operations are default methods.
///
/// [`NoOpCanonicalChain`]: vprogs_core_types::NoOpCanonicalChain
pub trait Tree: Sized {
    /// The hash function used for node and leaf hashing.
    type Hasher: Hasher;

    // -- Required methods (implementors must provide these) --

    /// Returns the version, block_hash, and data of the latest canonical SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    ///
    /// Candidates are scanned newest-first and skipped unless `canonical.is_canonical(version,
    /// block_hash)` holds, so nodes written by abandoned forks are invisible to reads.
    fn node(
        &self,
        key: &Key,
        max_version: u64,
        canonical: &impl CanonicalChain,
    ) -> Option<(u64, [u8; 32], Node)>;

    /// Finalizes `version`, deleting its superseded nodes plus any non-canonical fork entries
    /// written at it; `canonical` identifies which fork survives.
    fn prune(&self, wb: &mut impl WriteBatch, version: u64, canonical: &impl CanonicalChain);

    // -- Default methods --

    /// Returns the state root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn root(&self, version: u64, canonical: &impl CanonicalChain) -> [u8; 32] {
        // Version 0 is pre-genesis - no tree exists yet.
        if version == 0 {
            return EMPTY_HASH;
        }

        // Look up the root node and extract its hash.
        self.node(&Key::ROOT, version, canonical)
            .map(|(_, _, data)| *data.hash())
            .unwrap_or(EMPTY_HASH)
    }

    /// Commits state diffs to the tree at the given version under the writing fork's `block_hash`,
    /// returning the new root hash.
    ///
    /// No-op for empty diffs - returns the previous version's root. Panics if `version` is 0
    /// (version 0 is reserved as pre-genesis).
    fn update(
        &self,
        wb: &mut impl WriteBatch,
        commitments: Vec<Commitment>,
        version: u64,
        block_hash: [u8; 32],
        canonical: &impl CanonicalChain,
    ) -> [u8; 32] {
        assert!(version > 0, "version 0 is reserved as pre-genesis");

        // Empty commitments produce no tree changes - carry forward the previous root.
        if commitments.is_empty() {
            return self.root(version - 1, canonical);
        }

        // Apply leaf mutations and return the new root hash.
        Updater::apply(self, wb, version, block_hash, commitments, canonical)
    }

    /// Proves the state of the given keys at a specific version.
    ///
    /// Returns the wire-encoded proof (see `Proof::decode()`) or an error for duplicates keys.
    fn prove(
        &self,
        keys: &[ResourceId],
        version: u64,
        canonical: &impl CanonicalChain,
    ) -> Result<Vec<u8>> {
        ProofBuilder::build(self, version, keys, canonical)
    }
}
