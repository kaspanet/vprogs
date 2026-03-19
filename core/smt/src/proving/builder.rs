use alloc::vec::Vec;

use vprogs_core_utils::Bits;

use super::proof::Proof;
use crate::{EMPTY_HASH, Node, commitment::Commitment, key::Key, tree::Tree};

/// Walks the tree top-down to collect sibling hashes and leaf depths for a multi-proof.
pub(crate) struct ProofBuilder<'a, S> {
    /// Read-only access to existing tree nodes.
    store: &'a S,
    /// Tree version to generate the proof for.
    version: u64,
    /// Collected proof leaves: (depth, state commitment) pairs.
    leaves: Vec<(u16, Commitment)>,
    /// Collected sibling hashes for one-sided subtrees.
    siblings: Vec<[u8; 32]>,
    /// Topology bits encoding the proof tree structure (1 = split, 0 = sibling).
    topology_bits: Vec<bool>,
}

impl<'a, S: Tree> ProofBuilder<'a, S> {
    /// Generates a multi-proof for the given keys at `version` and returns it in wire format.
    pub(crate) fn build(store: &'a S, version: u64, leaf_keys: &[[u8; 32]]) -> Vec<u8> {
        // Sort and deduplicate keys so the proof leaves are in canonical order.
        let mut sorted_keys: Vec<[u8; 32]> = leaf_keys.to_vec();
        sorted_keys.sort();
        sorted_keys.dedup();

        // Initialize the proof builder.
        let mut ctx = Self {
            store,
            version,
            leaves: Vec::new(),
            siblings: Vec::new(),
            topology_bits: Vec::new(),
        };

        // Walk the tree top-down, collecting proof components.
        ctx.collect(Key::root(), &sorted_keys);

        // Encode collected components into wire format.
        Proof::encode(&ctx.leaves, &ctx.siblings, &ctx.topology_bits)
    }

    /// Recursive proof collection for a sorted sub-slice of leaf keys.
    fn collect(&mut self, key: Key, leaf_keys: &[[u8; 32]]) {
        // No keys to collect in this subtree.
        if leaf_keys.is_empty() {
            return;
        }

        // Dispatch based on the node type at this position.
        match self.store.get_node(&key, self.version) {
            // Empty subtree - all requested keys are absent.
            None => self.collect_empty(leaf_keys, key.level),

            // Shortcut leaf - emit it as the proof entry for this subtree.
            Some((_, Node::Leaf { key: leaf_key, value_hash, .. })) => {
                self.leaves.push((key.level, Commitment::new(leaf_key, value_hash)));
            }

            // Internal node - split proof keys and recurse into children.
            Some((_, Node::Internal { .. })) => {
                self.collect_internal(&key, leaf_keys);
            }
        }
    }

    /// Collects proof entries for an empty subtree - all proof keys are absent.
    fn collect_empty(&mut self, leaf_keys: &[[u8; 32]], level: u16) {
        if leaf_keys.len() == 1 {
            // Single absent key - emit as an empty leaf at this depth.
            self.leaves.push((level, Commitment::new(leaf_keys[0], EMPTY_HASH)));
            return;
        }

        // Multiple absent keys - split by bit to produce the topology the verifier needs.
        let mid = leaf_keys.partition_point(|k| !k.get_msb(level as usize));

        if mid > 0 && mid < leaf_keys.len() {
            // Both sides have absent keys - emit a split topology bit.
            self.topology_bits.push(true);
            self.collect_empty(&leaf_keys[..mid], level + 1);
            self.collect_empty(&leaf_keys[mid..], level + 1);
        } else {
            // All absent keys on one side - emit sibling (EMPTY_HASH) for the empty side.
            self.topology_bits.push(false);
            self.siblings.push(EMPTY_HASH);
            let nonempty = if mid == 0 { &leaf_keys[mid..] } else { &leaf_keys[..mid] };
            self.collect_empty(nonempty, level + 1);
        }
    }

    /// Collects proof entries at an internal node - splits proof keys by the current bit.
    fn collect_internal(&mut self, key: &Key, leaf_keys: &[[u8; 32]]) {
        // Split proof keys by the current bit.
        let mid = leaf_keys.partition_point(|k| !k.get_msb(key.level as usize));
        let (left_keys, right_keys) = leaf_keys.split_at(mid);

        // Construct child keys.
        let left_child = key.left_child();
        let right_child = key.right_child();

        if !left_keys.is_empty() && !right_keys.is_empty() {
            // Both children have proof keys - emit a split topology bit and recurse both sides.
            self.topology_bits.push(true);
            self.collect(left_child, left_keys);
            self.collect(right_child, right_keys);
        } else {
            // Only one side has proof keys - emit the other side's hash as a sibling.
            self.topology_bits.push(false);

            if left_keys.is_empty() {
                // Proof keys are on the right - left subtree hash becomes a sibling.
                self.siblings.push(self.node_hash(&left_child));
                self.collect(right_child, right_keys);
            } else {
                // Proof keys are on the left - right subtree hash becomes a sibling.
                self.siblings.push(self.node_hash(&right_child));
                self.collect(left_child, left_keys);
            }
        }
    }

    /// Returns the hash of the node at the given key, or `EMPTY_HASH` if absent.
    fn node_hash(&self, node_key: &Key) -> [u8; 32] {
        self.store
            .get_node(node_key, self.version)
            .map(|(_, data)| *data.hash())
            .unwrap_or(EMPTY_HASH)
    }
}
