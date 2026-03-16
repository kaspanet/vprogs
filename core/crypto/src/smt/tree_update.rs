use alloc::vec::Vec;

use vprogs_core_utils::{Bits, BitsArray};

use super::{
    key::Key, stale_node::StaleNode, state_commitment::StateCommitment, store::Store,
    write_batch::WriteBatch,
};
use crate::{Blake3Hasher, EMPTY_HASH, Hasher, Node, TREE_DEPTH};

/// Applies leaf mutations to the tree and writes resulting nodes into a `WriteBatch`.
///
/// Holds the shared state that threads through all recursive update calls: the store to read
/// existing nodes, the write batch to accumulate new nodes, and the version pair.
pub(super) struct TreeUpdate<'a, S, W> {
    store: &'a S,
    wb: &'a mut W,
    prev_version: u64,
    version: u64,
}

impl<'a, S: Store, W: WriteBatch> TreeUpdate<'a, S, W> {
    /// Applies `diffs` as leaf mutations at `version` and returns the new root hash.
    pub(super) fn apply<D>(store: &'a S, wb: &'a mut W, version: u64, diffs: &[D]) -> [u8; 32]
    where
        for<'b> StateCommitment: From<&'b D>,
    {
        let prev_version = version.saturating_sub(1);
        let mut ctx = Self { store, wb, prev_version, version };

        // Convert, sort, and deduplicate by key. All recursive methods below operate on sub-slices
        // of this vec — no further allocations on the common (internal-node) path.
        let mut updates: Vec<StateCommitment> = diffs.iter().map(StateCommitment::from).collect();
        updates.sort_by(|a, b| a.key.cmp(&b.key));
        updates.dedup_by(|a, b| a.key == b.key);

        // Recursively apply updates starting from the root.
        let result = ctx.update_subtree(Key::root(), &updates);

        // Write the result at the root position.
        let root_key = Key::root();
        ctx.mark_stale(&root_key);

        match &result {
            None => EMPTY_HASH,
            Some(node) => {
                ctx.wb.put_node(&root_key, ctx.version, node);
                *node.hash()
            }
        }
    }

    /// Core recursive update returning `None` for empty subtrees or the node that occupies this
    /// position.
    ///
    /// `updates` is a sorted sub-slice of leaf mutations that apply to this subtree. Splitting
    /// uses `partition_point` + `split_at` for zero-allocation descent.
    ///
    /// **Important:** returned `Leaf` nodes are *not* written to the store by this method — they
    /// bubble up to the caller, which decides whether to write them as a child of an Internal node
    /// or as the root. This enables leaf shortcutting where a single occupant floats to the highest
    /// ancestor.
    fn update_subtree(&mut self, key: Key, updates: &[StateCommitment]) -> Option<Node> {
        // No updates for this subtree — return existing node unchanged.
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

    /// Resolves a sorted set of leaf updates into the appropriate tree structure.
    ///
    /// Filters out deletions (`EMPTY_HASH`), then creates a shortcut leaf for a single live entry
    /// or splits and recurses for multiple.
    fn resolve_leaves(&mut self, key: &Key, updates: &[StateCommitment]) -> Option<Node> {
        let mut live = updates.iter().filter(|u| u.value_hash != EMPTY_HASH);

        let first = live.next()?;

        if live.next().is_none() {
            // Exactly one live entry — create a shortcut leaf at this depth.
            let hash = Blake3Hasher::hash_leaf(&first.key, &first.value_hash);
            return Some(Node::Leaf { key: first.key, value_hash: first.value_hash, hash });
        }

        // Multiple live entries — must split by the current bit and recurse.
        self.split_and_recurse(key, updates)
    }

    /// Handles updates at a position that currently holds a shortcut leaf.
    fn update_at_leaf(
        &mut self,
        key: &Key,
        updates: &[StateCommitment],
        existing_key: [u8; 32],
        existing_vh: [u8; 32],
    ) -> Option<Node> {
        // The existing leaf is being superseded regardless of outcome.
        self.mark_stale(key);

        // Fast path: single update replaces the existing leaf in-place.
        if updates.len() == 1 && updates[0].key == existing_key {
            let new_vh = updates[0].value_hash;
            if new_vh == EMPTY_HASH {
                return None; // Deletion — subtree becomes empty.
            }
            let hash = Blake3Hasher::hash_leaf(&existing_key, &new_vh);
            return Some(Node::Leaf { key: existing_key, value_hash: new_vh, hash });
        }

        // General case: merge the existing leaf into the update set (unless it's already being
        // updated) and resolve. Only allocates a new vec when the existing key is not in updates.
        if updates.iter().any(|u| u.key == existing_key) {
            self.resolve_leaves(key, updates)
        } else {
            let existing = StateCommitment { key: existing_key, value_hash: existing_vh };
            let pos = updates.partition_point(|u| u.key < existing_key);
            let mut merged = Vec::with_capacity(updates.len() + 1);
            merged.extend_from_slice(&updates[..pos]);
            merged.push(existing);
            merged.extend_from_slice(&updates[pos..]);
            self.resolve_leaves(key, &merged)
        }
    }

    /// Splits updates by the current bit and recurses into both children.
    ///
    /// Uses `partition_point` to find the split in O(log n) with zero allocation. After recursion,
    /// decides the return value: both empty -> None, one leaf + one empty -> bubble the leaf up
    /// (shortcutting), otherwise -> create Internal node.
    fn split_and_recurse(&mut self, key: &Key, updates: &[StateCommitment]) -> Option<Node> {
        debug_assert!((key.level as usize) < TREE_DEPTH, "exceeded tree depth");

        // Partition by the bit at the current depth. Since updates are sorted MSB-first, all bit=0
        // keys precede bit=1 keys — so `partition_point` finds the exact boundary.
        let mid = updates.partition_point(|u| !u.key.get_msb(key.level as usize));
        let (left, right) = updates.split_at(mid);

        let left_child = Key { level: key.level + 1, path: key.path };
        let right_child =
            Key { level: key.level + 1, path: key.path.with_bit_set(key.level as usize) };

        // Recurse into both children independently.
        let left_result = self.update_subtree(left_child.clone(), left);
        let right_result = self.update_subtree(right_child.clone(), right);

        match (&left_result, &right_result) {
            // Both children empty — this subtree is empty.
            (None, None) => None,

            // One child is a leaf, the other is empty — bubble the leaf up. This is the core
            // shortcutting mechanism: the single occupant doesn't need an internal node above it.
            (Some(Node::Leaf { .. }), None) => left_result,
            (None, Some(Node::Leaf { .. })) => right_result,

            // Otherwise (both non-empty, or at least one Internal) — write children to the store
            // and create an Internal node.
            _ => {
                let left_hash = self.write_child(&left_result, &left_child);
                let right_hash = self.write_child(&right_result, &right_child);
                let hash = Blake3Hasher::hash_internal(&left_hash, &right_hash);
                Some(Node::Internal { hash })
            }
        }
    }

    /// Writes a child node to the store and returns its hash.
    ///
    /// If the child is `None`, returns `EMPTY_HASH` without writing.
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
            self.wb.put_stale_node(&StaleNode {
                stale_since_version: self.version,
                node_key: node_key.clone(),
                node_version: old_version,
            });
        }
    }
}
