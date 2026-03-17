use super::{key::Key, stale_node::StaleNode};
use crate::Node;

/// Write interface for persisting SMT node mutations.
///
/// Pairs with `Store` (the read side). This trait is a supertrait of `WriteBatch`, so all
/// write batch implementations must provide SMT node persistence.
pub trait WriteBatch {
    /// Persists a new or updated node at the given position and version.
    fn put_node(&mut self, key: &Key, version: u64, data: &Node);

    /// Records a stale node marker for later garbage collection.
    fn put_stale_node(&mut self, stale: &StaleNode);
}
