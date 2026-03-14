//! `TreeStore` implementation backed by `Store` (RocksDB-compatible).
//!
//! Uses the SmtNode column family with descending-version key encoding so that a single
//! `prefix_iter` seek resolves "latest version <= max_version" in O(1) I/O.

use vprogs_storage_types::{StateSpace, Store};

use crate::{
    NodeData, node_key::NodeKey, tree_store::TreeStore, tree_update_batch::TreeUpdateBatch,
};

/// A `TreeStore` backed by a `Store` (RocksDB-compatible persistence).
///
/// Node lookups use the SmtNode column family with descending-version key encoding. Writing is
/// handled externally via `SmtCommit` — `apply_batch` and `prune_stale` are not used in the
/// production commit path (they exist for trait compliance).
pub struct RocksDbTreeStore<'a, S: Store> {
    store: &'a S,
}

impl<'a, S: Store> RocksDbTreeStore<'a, S> {
    /// Creates a new store backed by the given `Store`.
    pub fn new(store: &'a S) -> Self {
        Self { store }
    }
}

impl<S: Store> TreeStore for RocksDbTreeStore<'_, S> {
    /// Returns the node data and version of the latest node at `key` where version <=
    /// `max_version`.
    ///
    /// Builds a 42-byte seek key and does a single `prefix_iter` seek on the SmtNode CF. The
    /// descending-version encoding means the first result is the answer.
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)> {
        let seek = key.encode_seek_key(max_version);
        let mut iter = self.store.prefix_iter(StateSpace::SmtNode, &seek);
        let (raw_key, raw_value) = iter.next()?;

        let version = NodeKey::decode_version(&raw_key);
        let data = NodeData::from_bytes(&raw_value);
        Some((version, data))
    }

    /// Not used in the production commit path — SMT writes go through `SmtCommit`.
    fn apply_batch(&mut self, _batch: &TreeUpdateBatch) {
        // Writes are handled externally via SmtCommit::write_all.
    }

    /// Not used in the production commit path — pruning goes through the scheduler's prune loop.
    fn prune_stale(&mut self, _oldest_readable_version: u64) {
        // Pruning is handled externally by the scheduler's pruning worker.
    }
}
