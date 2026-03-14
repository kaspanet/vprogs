use super::{NodeData, node_key::NodeKey};

/// Read-only access to versioned SMT nodes.
///
/// Provides version-aware node lookups needed by `VersionedTree`. The blanket implementation
/// for `Store` uses descending-version key encoding so that a single `prefix_iter` seek
/// resolves "latest version <= max_version" in O(1) I/O.
pub trait TreeStore {
    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)>;
}

#[cfg(feature = "host")]
impl<S: vprogs_storage_types::Store> TreeStore for S {
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)> {
        let seek = key.encode_seek_key(max_version);
        let mut iter = self.prefix_iter(vprogs_storage_types::StateSpace::SmtNode, &seek);
        let (raw_key, raw_value) = iter.next()?;
        let version = NodeKey::decode_version(&raw_key);
        let data = NodeData::from_bytes(&raw_value);
        Some((version, data))
    }
}
