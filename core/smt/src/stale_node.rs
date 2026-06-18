use crate::Key;

/// A node that was superseded by a newer version. Used by pruning to garbage-collect old nodes.
#[derive(Debug)]
pub struct StaleNode {
    /// The version when this node became stale.
    pub stale_since_version: u64,
    /// Block_hash of the fork whose update at `stale_since_version` superseded the node.
    ///
    /// Prune acts on a marker only when this matches the canonical fork at `stale_since_version`,
    /// so a dead fork can't delete a node it superseded that is still canonical history.
    pub superseded_by_block_hash: [u8; 32],
    /// The stale node's position in the tree.
    pub key: Key,
    /// The stale node's version.
    pub version: u64,
    /// The block_hash of the fork that wrote the stale node.
    pub block_hash: [u8; 32],
}

impl StaleNode {
    /// Creates a new stale node marker.
    pub fn new(
        stale_since_version: u64,
        superseded_by_block_hash: [u8; 32],
        key: Key,
        version: u64,
        block_hash: [u8; 32],
    ) -> Self {
        Self { stale_since_version, superseded_by_block_hash, key, version, block_hash }
    }
}
