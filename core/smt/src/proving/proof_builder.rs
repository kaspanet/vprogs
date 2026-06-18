use alloc::{collections::BTreeMap, vec, vec::Vec};
use core::ops::Range;

use vprogs_core_codec::{Bits, Result, SortUnique};
use vprogs_core_types::{CanonicalChain, ResourceId};
use zerocopy::little_endian::{U16, U32};

use crate::{
    EMPTY_HASH, HashedNode, Key, Node, Tree,
    proving::{Leaf, Membership, Proof, Topology},
};

/// Walks the tree top-down to collect witness leaves, siblings, and one membership per input key.
pub(crate) struct ProofBuilder<'a, S, C> {
    /// Read-only access to existing tree nodes.
    tree: &'a S,
    /// Oracle that filters node reads to the canonical fork.
    canonical: &'a C,
    /// Tree version to generate the proof for.
    version: u64,
    /// Deduplicated keys table. Indices `0..keys_input.len()` are input keys in caller order.
    keys: Vec<[u8; 32]>,
    /// Lookup helper for deduping keys as the walk emits leaves.
    key_to_idx: BTreeMap<[u8; 32], u32>,
    /// Collected witness leaves (each references its key by index into `keys`).
    leaves: Vec<Leaf>,
    /// Collected sibling summaries (kind + hash) for one-sided subtrees.
    siblings: Vec<HashedNode>,
    /// Packed topology bitfield (LSB-first).
    topology: Topology,
    /// Per sorted-input position, the index of the witness leaf that answers it.
    sorted_to_leaf: Vec<u32>,
}

impl<'a, S: Tree, C: CanonicalChain> ProofBuilder<'a, S, C> {
    /// Generates a multi-proof for the given keys at `version`.
    pub(crate) fn build(
        tree: &'a S,
        version: u64,
        keys_input: &[ResourceId],
        canonical: &'a C,
    ) -> Result<Vec<u8>> {
        // Sort keys into canonical leaf order and capture the input -> sorted permutation.
        let (sorted_keys, sort_order) = keys_input.sort_unique()?;

        // Construct the builder, walk the tree, and classify each member.
        let mut this = Self::new(tree, version, keys_input, canonical);
        this.collect(&Key::ROOT, &sorted_keys, 0);
        let memberships = this.classify_memberships(&sort_order);

        // Encode the assembled proof.
        Ok(Proof {
            keys: &this.keys,
            leaves: &this.leaves,
            siblings: &this.siblings,
            topology: &this.topology.bytes,
            memberships: &memberships,
        }
        .encode())
    }

    /// Constructs a builder with pre-allocated collections and seeds the keys table from
    /// `keys_input`.
    fn new(tree: &'a S, version: u64, keys_input: &[ResourceId], canonical: &'a C) -> Self {
        let mut this = Self {
            tree,
            canonical,
            version,
            keys: Vec::with_capacity(keys_input.len()),
            key_to_idx: BTreeMap::new(),
            leaves: Vec::new(),
            siblings: Vec::new(),
            topology: Topology::default(),
            sorted_to_leaf: vec![0u32; keys_input.len()],
        };

        // Seed the keys table and its lookup index from the caller's input.
        for (i, key) in keys_input.iter().enumerate() {
            this.keys.push(**key);
            this.key_to_idx.insert(**key, i as u32);
        }

        this
    }

    /// Recursive proof collection for a sorted sub-slice of input keys.
    fn collect(&mut self, key: &Key, input_keys: &[&ResourceId], offset: usize) {
        // No keys to collect in this subtree.
        if input_keys.is_empty() {
            return;
        }

        // Dispatch based on the node type at this position.
        match self.tree.node(key, self.version, self.canonical) {
            // Empty subtree (no node or explicit tombstone) - all input keys are absent.
            None | Some((_, _, Node::Empty)) => self.collect_empty(input_keys, key.level, offset),

            // Shortcut leaf - witnesses every input key routed into this subtree.
            Some((_, _, Node::Leaf { key: leaf_key, value_hash, .. })) => {
                self.emit_leaf(*leaf_key, value_hash, key.level, offset..offset + input_keys.len());
            }

            // Internal node - split proof keys and recurse into children.
            Some((_, _, Node::Internal { .. })) => self.collect_internal(key, input_keys, offset),
        }
    }

    /// Collects proof entries for an empty subtree - all input keys are absent.
    fn collect_empty(&mut self, input_keys: &[&ResourceId], level: u16, offset: usize) {
        // Single absent key - emit an empty leaf carrying the input key itself.
        if input_keys.len() == 1 {
            self.emit_leaf(**input_keys[0], EMPTY_HASH, level, offset..offset + 1);
            return;
        }

        // Multiple absent keys - split by bit to produce the topology the verifier needs.
        let mid = input_keys.partition_point(|k| !k.get_msb(level as usize));
        if mid > 0 && mid < input_keys.len() {
            // Both sides have absent keys - emit a split topology bit.
            self.topology.push(true);
            self.collect_empty(&input_keys[..mid], level + 1, offset);
            self.collect_empty(&input_keys[mid..], level + 1, offset + mid);
        } else {
            // All absent keys on one side - emit an empty sibling for the empty side.
            self.topology.push(false);
            self.siblings.push(HashedNode::EMPTY);
            self.collect_empty(input_keys, level + 1, offset);
        }
    }

    /// Collects proof entries at an internal node - splits input keys by the current bit.
    fn collect_internal(&mut self, key: &Key, input_keys: &[&ResourceId], offset: usize) {
        // Split input keys by the current bit.
        let mid = input_keys.partition_point(|k| !k.get_msb(key.level as usize));
        let (left_keys, right_keys) = input_keys.split_at(mid);

        // Construct child keys.
        let left_child = key.left_child();
        let right_child = key.right_child();

        match (left_keys.is_empty(), right_keys.is_empty()) {
            // Both children have input keys - emit a split topology bit and recurse both sides.
            (false, false) => {
                self.topology.push(true);
                self.collect(&left_child, left_keys, offset);
                self.collect(&right_child, right_keys, offset + mid);
            }
            // Input keys are on the right - left subtree becomes a sibling.
            (true, false) => {
                self.topology.push(false);
                self.siblings.push(self.child_summary(&left_child));
                self.collect(&right_child, right_keys, offset);
            }
            // Input keys are on the left - right subtree becomes a sibling.
            (false, true) => {
                self.topology.push(false);
                self.siblings.push(self.child_summary(&right_child));
                self.collect(&left_child, left_keys, offset);
            }
            // Caller's early-return on empty `input_keys` guarantees at least one is non-empty.
            (true, true) => unreachable!(),
        }
    }

    /// Builds the per-input-key `Membership` vector by inverting `sort_order` while classifying.
    fn classify_memberships(&self, sort_order: &[U32]) -> Vec<Membership> {
        let mut memberships = vec![Membership::Absent(U32::new(0)); sort_order.len()];
        for (sorted_pos, &input_idx) in sort_order.iter().enumerate() {
            let leaf_pos = self.sorted_to_leaf[sorted_pos];
            let idx = input_idx.get() as usize;

            memberships[idx] = if self.leaves[leaf_pos as usize].key_idx.get() == idx as u32 {
                Membership::Present(U32::new(leaf_pos))
            } else {
                Membership::Absent(U32::new(leaf_pos))
            };
        }

        memberships
    }

    /// Emits a leaf at `depth` carrying `(key, value_hash)`, and records it as the witness for
    /// `slots`.
    fn emit_leaf(&mut self, key: [u8; 32], value_hash: [u8; 32], depth: u16, slots: Range<usize>) {
        let key_idx = self.intern_key(key);
        let leaf_pos = self.leaves.len() as u32;
        self.leaves.push(Leaf { depth: U16::new(depth), key_idx: U32::new(key_idx), value_hash });
        for slot in &mut self.sorted_to_leaf[slots] {
            *slot = leaf_pos;
        }
    }

    /// Interns `key` into the keys table and returns its index.
    fn intern_key(&mut self, key: [u8; 32]) -> u32 {
        if let Some(&idx) = self.key_to_idx.get(&key) {
            return idx;
        }

        let idx = self.keys.len() as u32;
        self.keys.push(key);
        self.key_to_idx.insert(key, idx);
        idx
    }

    /// Returns the [`HashedNode`] summary of the node at `node_key`, or [`HashedNode::EMPTY`].
    fn child_summary(&self, node_key: &Key) -> HashedNode {
        self.tree
            .node(node_key, self.version, self.canonical)
            .map(|(_, _, d)| HashedNode::from(&d))
            .unwrap_or(HashedNode::EMPTY)
    }
}
