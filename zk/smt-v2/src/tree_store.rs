use crate::{NodeData, node_key::NodeKey, tree_update_batch::TreeUpdateBatch};

/// Storage interface for versioned tree nodes.
///
/// Returns `NodeData` (internal or shortcut leaf) instead of bare hashes, since the tree needs to
/// distinguish node types during updates and proof generation.
pub trait TreeStore {
    /// Returns the node data and version of the latest node at `key` where version <=
    /// `max_version`.
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)>;

    /// Persists a batch of node updates atomically.
    fn apply_batch(&mut self, batch: &TreeUpdateBatch);

    /// Deletes all node versions that became stale at or before `oldest_readable_version`.
    fn prune_stale(&mut self, oldest_readable_version: u64);
}
