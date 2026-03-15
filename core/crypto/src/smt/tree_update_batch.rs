use alloc::vec::Vec;

use super::{node::Node, stale_node::StaleNode, tree_write_batch::TreeWriteBatch};

/// Result of a tree update operation.
///
/// Contains the new nodes to persist and the nodes that became stale (superseded). Produced by
/// `VersionedTree::update()` and consumed via `write()`.
pub struct TreeUpdateBatch {
    /// New nodes to write.
    pub new_nodes: Vec<Node>,
    /// Nodes made stale by this update.
    pub stale_nodes: Vec<StaleNode>,
    /// Root hash after the update.
    pub root: [u8; 32],
    /// Version this batch was created for.
    pub version: u64,
}

impl TreeUpdateBatch {
    /// Writes all new nodes and stale markers into the given write batch.
    pub fn write(&self, wb: &mut impl TreeWriteBatch) {
        for node in &self.new_nodes {
            wb.put_node(node);
        }
        for stale in &self.stale_nodes {
            wb.put_stale_node(stale);
        }
    }
}
