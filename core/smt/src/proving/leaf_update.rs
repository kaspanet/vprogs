/// Classification of the post-state at a proof-leaf, computed from its members.
pub(crate) enum LeafUpdate<'a> {
    /// No member alters the leaf's value; post-state equals pre-state.
    Unchanged,
    /// Single live `(key, value)` remains; post is `HashedNode::leaf(leaf_key, value)`.
    Single(&'a [u8; 32]),
    /// Multiple live entries; post is `subtree_hash` over the prepared buffer.
    Multiple,
}
