use super::{NodeData, node_key::NodeKey};
use crate::EMPTY_HASH;

/// Read-only access to versioned SMT nodes.
///
/// Provides version-aware node lookups needed by `VersionedTree`. This trait is a supertrait of
/// `Store`, so all store implementations must provide SMT node lookups. Concrete implementations
/// use descending-version key encoding so that a single `prefix_iter` seek resolves "latest
/// version <= max_version" in O(1) I/O.
pub trait TreeStore {
    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)>;

    /// Returns the SMT root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn get_root(&self, version: u64) -> [u8; 32] {
        if version == 0 {
            return EMPTY_HASH;
        }
        self.get_node(&NodeKey::root(), version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }
}
