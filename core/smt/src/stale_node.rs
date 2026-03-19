use crate::key::Key;

/// A node that was superseded by a newer version. Used by pruning to garbage-collect old nodes.
pub struct StaleNode {
    /// The version when this node became stale.
    pub stale_since_version: u64,
    /// The node's position in the tree.
    pub node_key: Key,
    /// The version of the now-stale node.
    pub node_version: u64,
}

impl StaleNode {
    pub fn new(stale_since_version: u64, node_key: Key, node_version: u64) -> Self {
        Self { stale_since_version, node_key, node_version }
    }
}
