use alloc::vec::Vec;

use vprogs_core_utils::Bits;

use crate::{
    DEPTH, EMPTY_HASH, Node, commitment::Commitment, key::Key, stale_node::StaleNode, tree::Tree,
    write_batch::WriteBatch,
};

/// Applies leaf mutations to the tree and writes resulting nodes into a `WriteBatch`.
pub(crate) struct Updater<'a, S, W> {
    /// Read-only access to existing tree nodes.
    store: &'a S,
    /// Accumulates new/deleted nodes for atomic commit.
    wb: &'a mut W,
    /// Version to read existing nodes from (version - 1).
    prev_version: u64,
    /// Version being written.
    version: u64,
}

impl<'a, S: Tree, W: WriteBatch> Updater<'a, S, W> {
    /// Applies `diffs` as leaf mutations at `version` and returns the new root hash.
    pub(crate) fn apply<D>(store: &'a S, wb: &'a mut W, version: u64, diffs: &[D]) -> [u8; 32]
    where
        for<'b> Commitment: From<&'b D>,
    {
        // Initialize the update context.
        let prev_version = version.saturating_sub(1);
        let mut ctx = Self { store, wb, prev_version, version };

        // Convert, sort, and deduplicate by key. On duplicate keys, last-write-wins: `dedup_by`
        // removes `a` (later element) and keeps `b`, so we copy `a`'s value_hash into `b` first.
        let mut updates: Vec<Commitment> = diffs.iter().map(Commitment::from).collect();
        updates.sort_by(|a, b| a.key.cmp(&b.key));
        updates.dedup_by(|a, b| {
            if a.key == b.key {
                b.value_hash = a.value_hash;
                true
            } else {
                false
            }
        });

        // Recursively apply updates starting from the root. `update_subtree` marks the existing
        // root stale internally, so no separate `mark_stale` call is needed here.
        let result = ctx.update_subtree(Key::root(), &updates);

        // Write the result at the root position.
        match &result {
            None => EMPTY_HASH,
            Some(node) => {
                ctx.wb.put_node(&Key::root(), ctx.version, node);
                *node.hash()
            }
        }
    }

    /// Recursive update for a sorted sub-slice of leaf mutations at `key`.
    ///
    /// Returns `None` for empty subtrees or the node at this position. Returned `Leaf` nodes are
    /// not written here - they bubble up so the caller can decide where to store them (enables
    /// shortcutting).
    fn update_subtree(&mut self, key: Key, updates: &[Commitment]) -> Option<Node> {
        // No updates for this subtree - return existing node unchanged.
        if updates.is_empty() {
            return self.store.get_node(&key, self.prev_version).map(|(_, data)| data);
        }

        // Look up existing node at this position to determine the update strategy.
        let existing = self.store.get_node(&key, self.prev_version).map(|(_, data)| data);

        match existing {
            // Empty subtree: resolve updates into leaves directly.
            None => self.resolve_leaves(&key, updates),

            // Existing shortcut leaf: may need to split if keys differ.
            Some(Node::Leaf { key: existing_key, value_hash: existing_vh, .. }) => {
                self.update_at_leaf(&key, updates, existing_key, existing_vh)
            }

            // Existing internal node: mark stale and recurse into children.
            Some(Node::Internal { .. }) => {
                self.mark_stale(&key);
                self.split_and_recurse(&key, updates)
            }
        }
    }

    /// Resolves leaf updates into a shortcut leaf (single live entry) or splits and recurses.
    fn resolve_leaves(&mut self, key: &Key, updates: &[Commitment]) -> Option<Node> {
        // Filter to live (non-deletion) updates.
        let mut live = updates.iter().filter(|u| u.value_hash != EMPTY_HASH);

        // Get the first live entry, or return None if the subtree is empty.
        let first = live.next()?;

        if live.next().is_none() {
            // Exactly one live entry - create a shortcut leaf at this depth.
            return Some(Node::leaf::<S::Hasher>(first.key, first.value_hash));
        }

        // Multiple live entries - must split by the current bit and recurse.
        self.split_and_recurse(key, updates)
    }

    /// Handles updates at a position that currently holds a shortcut leaf.
    fn update_at_leaf(
        &mut self,
        key: &Key,
        updates: &[Commitment],
        existing_key: [u8; 32],
        existing_vh: [u8; 32],
    ) -> Option<Node> {
        // The existing leaf is being superseded regardless of outcome.
        self.mark_stale(key);

        // Fast path: single update replaces the existing leaf in-place.
        if updates.len() == 1 && updates[0].key == existing_key {
            let new_vh = updates[0].value_hash;
            if new_vh == EMPTY_HASH {
                return None; // Deletion - subtree becomes empty.
            }
            return Some(Node::leaf::<S::Hasher>(existing_key, new_vh));
        }

        // General case: merge the existing leaf into the update set and resolve.
        if updates.iter().any(|u| u.key == existing_key) {
            // Existing key is already in the update set - resolve directly.
            self.resolve_leaves(key, updates)
        } else {
            // Existing key not in updates - insert at the correct sorted position and resolve.
            let existing = Commitment::new(existing_key, existing_vh);
            let pos = updates.partition_point(|u| u.key < existing_key);
            let mut merged = Vec::with_capacity(updates.len() + 1);
            merged.extend_from_slice(&updates[..pos]);
            merged.push(existing);
            merged.extend_from_slice(&updates[pos..]);
            self.resolve_leaves(key, &merged)
        }
    }

    /// Splits updates by the current bit and recurses into both children.
    fn split_and_recurse(&mut self, key: &Key, updates: &[Commitment]) -> Option<Node> {
        debug_assert!((key.level as usize) < DEPTH, "exceeded tree depth");

        // Partition by the bit at the current depth. Since updates are sorted MSB-first, all bit=0
        // keys precede bit=1 keys - so `partition_point` finds the exact boundary.
        let mid = updates.partition_point(|u| !u.key.get_msb(key.level as usize));
        let (left, right) = updates.split_at(mid);

        // Construct child keys.
        let left_child = key.left_child();
        let right_child = key.right_child();

        // Recurse into both children independently.
        let left_result = self.update_subtree(left_child.clone(), left);
        let right_result = self.update_subtree(right_child.clone(), right);

        // Determine the result based on child subtree outcomes.
        match (&left_result, &right_result) {
            // Both children empty - this subtree is empty.
            (None, None) => None,

            // One child is a leaf, the other is empty - bubble the leaf up. This is the core
            // shortcutting mechanism: the single occupant doesn't need an internal node above it.
            (Some(Node::Leaf { .. }), None) => left_result,
            (None, Some(Node::Leaf { .. })) => right_result,

            // Otherwise (both non-empty, or at least one Internal) - write children to the store
            // and create an Internal node.
            _ => {
                let left_hash = self.write_child(&left_result, &left_child);
                let right_hash = self.write_child(&right_result, &right_child);
                Some(Node::internal::<S::Hasher>(&left_hash, &right_hash))
            }
        }
    }

    /// Writes a child node to the store and returns its hash, or `EMPTY_HASH` for `None`.
    fn write_child(&mut self, child: &Option<Node>, key: &Key) -> [u8; 32] {
        match child {
            None => EMPTY_HASH,
            Some(node) => {
                let hash = *node.hash();
                self.wb.put_node(key, self.version, node);
                hash
            }
        }
    }

    /// Marks an existing node at the given position as stale (if it exists).
    fn mark_stale(&mut self, node_key: &Key) {
        if let Some((old_version, _)) = self.store.get_node(node_key, self.prev_version) {
            self.wb.put_stale_node(&StaleNode::new(self.version, node_key.clone(), old_version));
        }
    }
}
