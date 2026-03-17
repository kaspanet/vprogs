use super::key::Key;

/// A node that was superseded by a newer version.
///
/// Recorded during updates and used by the pruning worker to garbage-collect old node versions
/// that are no longer reachable.
pub struct StaleNode {
    /// The version when this node became stale.
    pub stale_since_version: u64,
    /// The node's position in the tree.
    pub node_key: Key,
    /// The version of the now-stale node.
    pub node_version: u64,
}
