use super::node_key::NodeKey;
use crate::NodeData;

/// A versioned SMT node: position + version + data.
pub struct Node {
    /// The node's position in the tree.
    pub key: NodeKey,
    /// The version this node was created at.
    pub version: u64,
    /// The node's data (Internal or Leaf).
    pub data: NodeData,
}
