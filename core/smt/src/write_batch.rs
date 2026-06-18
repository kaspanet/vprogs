use crate::{Key, Node, StaleNode};

/// Write interface for persisting SMT node mutations. Pairs with `Tree` (the read side).
pub trait WriteBatch {
    /// Persists a new or updated node at the given position, version, and writing fork's
    /// block_hash.
    fn put_node(&mut self, key: &Key, version: u64, block_hash: &[u8; 32], data: &Node);

    /// Records a stale node marker for later garbage collection.
    fn put_stale_node(&mut self, stale: &StaleNode);

    /// Deletes a node at the given position, version, and block_hash.
    fn delete_node(&mut self, key: &Key, version: u64, block_hash: &[u8; 32]);

    /// Deletes a stale node marker.
    fn delete_stale_node(&mut self, stale: &StaleNode);
}
