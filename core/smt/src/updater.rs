use alloc::vec::Vec;

use vprogs_core_codec::Bits;
use vprogs_core_types::ResourceId;

use crate::{Commitment, DEPTH, EMPTY_HASH, HashedNode, Key, Node, StaleNode, Tree, WriteBatch};

/// Applies leaf mutations to the tree and writes resulting nodes into a `WriteBatch`.
pub(crate) struct Updater<'a, S: Tree, W> {
    /// Read-only access to existing tree nodes.
    tree: &'a S,
    /// Read snapshot shared by every `node` lookup in this update, for a consistent base.
    snapshot: S::Snapshot,
    /// Accumulates new/deleted nodes for atomic commit.
    wb: &'a mut W,
    /// Version being written.
    version: u64,
}

impl<'a, S: Tree, W: WriteBatch> Updater<'a, S, W> {
    /// Applies `diffs` as leaf mutations at `version` and returns the new root hash.
    pub(crate) fn apply(
        tree: &'a S,
        wb: &'a mut W,
        version: u64,
        mut commitments: Vec<Commitment>,
    ) -> [u8; 32] {
        // Capture one snapshot for the whole update. Version > 0 is guaranteed by `Tree::update`.
        let mut ctx = Self { tree, snapshot: tree.snapshot(), wb, version };

        // Sort and dedup by key; on a duplicate, last-write-wins by copying the later value_hash.
        commitments.sort_by_key(|a| a.key);
        commitments.dedup_by(|a, b| {
            if a.key == b.key {
                b.value_hash = a.value_hash;
                true
            } else {
                false
            }
        });

        // Apply from the root; update_subtree marks the root, so no separate call here.
        match &ctx.update_subtree(&Key::ROOT, &commitments) {
            // Tree drained: write a tombstone so reads don't fall back to the stale prior root.
            None => {
                ctx.wb.put_node(&Key::ROOT, ctx.version, &Node::Empty);
                EMPTY_HASH
            }
            Some(node) => {
                ctx.wb.put_node(&Key::ROOT, ctx.version, node);
                *node.hash()
            }
        }
    }

    /// Reads the existing node at `key` from the prior version, against the captured snapshot.
    fn node(&self, key: &Key) -> Option<(u64, Node)> {
        self.tree.node(key, self.version - 1, &self.snapshot)
    }

    /// Recursive update for a sorted sub-slice of leaf mutations at `key`, a position of the
    /// tree at the prior version: the root, or a child of an internal node.
    ///
    /// Returns `None` for empty subtrees or the node at this position. Returned `Leaf` nodes are
    /// not written here - they bubble up so the caller can decide where to place them (enables
    /// shortcutting).
    fn update_subtree(&mut self, key: &Key, commitments: &[Commitment]) -> Option<Node> {
        // No commitments for this subtree - return existing node unchanged.
        if commitments.is_empty() {
            // An `Empty` tombstone collapses to `None` so callers can treat both as "no subtree".
            let exists = |node: &Node| !matches!(node, Node::Empty);
            return self.node(key).map(|(_, data)| data).filter(exists);
        }

        // Look up the existing node at this position; whatever it holds goes stale with this
        // update.
        let existing = self.node(key);
        if let Some((node_version, _)) = &existing {
            self.mark(key, *node_version);
        }

        // The existing node determines the update strategy.
        match existing.map(|(_, data)| data) {
            // Empty subtree or tombstone: build the subtree from the commitments.
            None | Some(Node::Empty) => self.build_subtree(key, commitments),

            // Existing shortcut leaf: may need to split if keys differ.
            Some(Node::Leaf { key: existing_key, value_hash: existing_vh, .. }) => {
                self.update_at_leaf(key, commitments, existing_key, existing_vh)
            }

            // Existing internal node: recurse into its children.
            Some(Node::Internal { .. }) => self.split_and_recurse(key, commitments),
        }
    }

    /// Builds the subtree at `key` from `commitments` alone: a shortcut leaf for a single live
    /// entry, an internal node over the two halves for more.
    ///
    /// Nothing below the position is part of the tree at the prior version: it held a leaf, a
    /// tombstone or no node, or lies below such a position. So nothing stored below it is taken
    /// into the tree. A position down there can still hold a node that a deleted or promoted
    /// subtree left behind, and reading it as a child would bring that node back.
    fn build_subtree(&mut self, key: &Key, commitments: &[Commitment]) -> Option<Node> {
        // Filter to live (non-deletion) commitments.
        let mut live = commitments.iter().filter(|u| u.value_hash != EMPTY_HASH);

        // Get the first live entry, or return None if the subtree is empty.
        let first = live.next()?;

        if live.next().is_none() {
            // Exactly one live entry - create a shortcut leaf at this depth.
            return Some(Node::leaf::<S::Hasher>(first.key, first.value_hash));
        }

        // Multiple live entries - split by the current bit and build both halves.
        assert!((key.level as usize) < DEPTH, "exceeded tree depth");
        let mid = commitments.partition_point(|u| !u.key.get_msb(key.level as usize));
        let (left, right) = commitments.split_at(mid);
        let left_child = key.left_child();
        let right_child = key.right_child();
        let left_result = self.build_subtree(&left_child, left);
        let right_result = self.build_subtree(&right_child, right);

        // Two live entries make an internal node: each half holds one, or one half holds both
        // and is an internal node itself, which never bubbles up.
        Some(Node::internal::<S::Hasher>(
            &self.write_child(&left_result, &left_child, false),
            &self.write_child(&right_result, &right_child, false),
        ))
    }

    /// Handles commitments at a position that currently holds a shortcut leaf.
    fn update_at_leaf(
        &mut self,
        key: &Key,
        commitments: &[Commitment],
        existing_key: ResourceId,
        existing_vh: [u8; 32],
    ) -> Option<Node> {
        // Fast path: single update replaces the existing leaf in-place.
        if commitments.len() == 1 && commitments[0].key == existing_key {
            let new_vh = commitments[0].value_hash;
            if new_vh == EMPTY_HASH {
                return None; // Deletion - subtree becomes empty.
            }
            return Some(Node::leaf::<S::Hasher>(existing_key, new_vh));
        }

        // General case: merge the existing leaf into the update set and build the subtree.
        let pos = commitments.partition_point(|u| u.key < existing_key);
        if commitments.get(pos).is_some_and(|u| u.key == existing_key) {
            // Existing key is already in the update set - build directly.
            self.build_subtree(key, commitments)
        } else {
            // Existing key not in commitments - insert at the correct sorted position and build.
            let existing = Commitment::new(existing_key, existing_vh);
            let mut merged = Vec::with_capacity(commitments.len() + 1);
            merged.extend_from_slice(&commitments[..pos]);
            merged.push(existing);
            merged.extend_from_slice(&commitments[pos..]);
            self.build_subtree(key, &merged)
        }
    }

    /// Splits commitments by the current bit and recurses into both children of the internal
    /// node at `key`.
    fn split_and_recurse(&mut self, key: &Key, commitments: &[Commitment]) -> Option<Node> {
        assert!((key.level as usize) < DEPTH, "exceeded tree depth");

        // Partition by the current-depth bit; sorted MSB-first, so bit=0 keys precede bit=1.
        let mid = commitments.partition_point(|u| !u.key.get_msb(key.level as usize));
        let (left, right) = commitments.split_at(mid);

        // Construct child keys.
        let left_child = key.left_child();
        let right_child = key.right_child();

        // Recurse into both children independently.
        let left_result = self.update_subtree(&left_child, left);
        let right_result = self.update_subtree(&right_child, right);

        // Determine the result based on child subtree outcomes.
        match (&left_result, &right_result) {
            // Both children empty - this subtree is empty.
            (None, None) => None,

            // One child a leaf, the other empty: bubble it up (shortcut, no internal node). A
            // leaf that got no commitment is read from its position and leaves that node
            // behind, which goes stale with the move.
            (Some(Node::Leaf { .. }), None) => {
                if left.is_empty() {
                    self.mark_stale(&left_child);
                }
                left_result
            }
            (None, Some(Node::Leaf { .. })) => {
                if right.is_empty() {
                    self.mark_stale(&right_child);
                }
                right_result
            }

            // Otherwise (both non-empty, or one Internal): write children, make an Internal node.
            // A child that got no commitment is unchanged.
            _ => Some(Node::internal::<S::Hasher>(
                &self.write_child(&left_result, &left_child, left.is_empty()),
                &self.write_child(&right_result, &right_child, right.is_empty()),
            )),
        }
    }

    /// Returns the [`HashedNode`] summary of a child node, writing the child to the tree unless
    /// it is `unchanged`.
    ///
    /// Every node an update writes below the root is written here, as a child of an internal
    /// node built at the same version. Pruning an orphaned version relies on that to find the
    /// version's nodes.
    ///
    /// An unchanged child is the node its position already stores, and that row stays in use.
    /// Storing the node again at this version would leave the previous row behind without a
    /// stale marker: one more write and one more row for every sibling an update passes, and
    /// nothing to reclaim the row by.
    fn write_child(&mut self, child: &Option<Node>, key: &Key, unchanged: bool) -> HashedNode {
        if unchanged {
            return child.as_ref().map_or(HashedNode::EMPTY, HashedNode::from);
        }

        match child {
            None => {
                // Tombstone an empty position that still holds a node, so a later read can't
                // resurface it: the node deleted by this update, or below a rebuilt position
                // one that a deleted or promoted subtree left behind. A marker is keyed by
                // version and position, so the tombstone's replaces the one this update put
                // at the position of a node it deleted. That one marker reclaims both: pruning
                // a tombstone removes every node at its position up to the tombstone.
                let exists = |(_, node)| !matches!(node, Node::Empty);
                if self.node(key).is_some_and(exists) {
                    self.wb.put_node(key, self.version, &Node::Empty);
                    self.mark(key, self.version);
                }
                HashedNode::EMPTY
            }
            Some(node) => {
                self.wb.put_node(key, self.version, node);
                HashedNode::from(node)
            }
        }
    }

    /// Marks the node at the given position stale at this version, if the position holds one.
    fn mark_stale(&mut self, node_key: &Key) {
        if let Some((node_version, _)) = self.node(node_key) {
            self.mark(node_key, node_version);
        }
    }

    /// Marks the node at `node_version` at the given position stale at this version.
    fn mark(&mut self, node_key: &Key, node_version: u64) {
        self.wb.put_stale_node(&StaleNode::new(self.version, *node_key, node_version));
    }
}
