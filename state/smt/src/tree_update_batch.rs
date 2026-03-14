use alloc::vec::Vec;

use crate::{NodeData, node_key::NodeKey, stale_node::StaleNode};

/// Result of a tree update operation.
///
/// Contains the new nodes to persist and the nodes that became stale (superseded). Produced by
/// `VersionedTree::update()` and consumed by `TreeStore::apply_batch()`.
pub struct TreeUpdateBatch {
    /// New nodes to write: `(position, version, node data)`.
    pub new_nodes: Vec<(NodeKey, u64, NodeData)>,
    /// Nodes made stale by this update.
    pub stale_nodes: Vec<StaleNode>,
    /// Root hash after the update.
    pub root: [u8; 32],
    /// Version this batch was created for.
    pub version: u64,
}
