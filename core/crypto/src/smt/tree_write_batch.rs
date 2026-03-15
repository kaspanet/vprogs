use super::{node::Node, stale_node::StaleNode};

/// Write interface for persisting SMT node mutations.
///
/// Pairs with `TreeStore` (the read side). This trait is a supertrait of `WriteBatch`, so all
/// write batch implementations must provide SMT node persistence.
pub trait TreeWriteBatch {
    /// Persists a new or updated node.
    fn put_node(&mut self, node: &Node);

    /// Records a stale node marker for later garbage collection.
    fn put_stale_node(&mut self, stale: &StaleNode);
}
