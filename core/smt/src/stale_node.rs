use crate::key::Key;

/// A node that was superseded by a newer version. Used by pruning to garbage-collect old nodes.
#[derive(Debug)]
pub struct StaleNode {
    /// The version when this node became stale.
    pub stale_since_version: u64,
    /// The stale node's position in the tree.
    pub key: Key,
    /// The stale node's version.
    pub version: u64,
}

impl StaleNode {
    /// Creates a new stale node marker.
    pub fn new(stale_since_version: u64, key: Key, version: u64) -> Self {
        Self { stale_since_version, key, version }
    }
}
