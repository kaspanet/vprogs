use alloc::vec::Vec;

use vprogs_core_utils::{Bits, BitsArray};

use super::{key::Key, proof::Proof, state_commitment::StateCommitment, store::Store};
use crate::{EMPTY_HASH, Node};

/// Collects sibling hashes and leaf depths to produce an encoded multi-proof.
///
/// Walks the persistent store top-down, accumulating proof components. At each position it looks
/// up the stored node: **None** (empty subtree) means all proof keys are absent; **Leaf** emits
/// the shortcut leaf as a proof entry; **Internal** splits proof keys by current bit and recurses.
pub(super) struct ProofBuilder<'a, S> {
    store: &'a S,
    version: u64,
    leaves: Vec<(u16, StateCommitment)>,
    siblings: Vec<[u8; 32]>,
    topology_bits: Vec<bool>,
}

impl<'a, S: Store> ProofBuilder<'a, S> {
    /// Generates a multi-proof for the given keys at `version` and returns it in wire format.
    pub(super) fn build(store: &'a S, version: u64, leaf_keys: &[[u8; 32]]) -> Vec<u8> {
        let mut sorted_keys: Vec<[u8; 32]> = leaf_keys.to_vec();
        sorted_keys.sort();
        sorted_keys.dedup();

        let mut ctx = Self {
            store,
            version,
            leaves: Vec::new(),
            siblings: Vec::new(),
            topology_bits: Vec::new(),
        };

        ctx.collect(Key::root(), &sorted_keys);

        Proof::encode(&ctx.leaves, &ctx.siblings, &ctx.topology_bits)
    }

    /// Recursive proof collection for a sorted sub-slice of leaf keys.
    fn collect(&mut self, key: Key, leaf_keys: &[[u8; 32]]) {
        if leaf_keys.is_empty() {
            return;
        }

        let existing = self.store.get_node(&key, self.version);

        match existing {
            // Empty subtree — all requested keys are absent.
            None => self.collect_empty(leaf_keys, key.level),

            // Shortcut leaf — emit it as the proof entry for this subtree.
            Some((_, Node::Leaf { key: leaf_key, value_hash, .. })) => {
                self.leaves.push((key.level, StateCommitment { key: leaf_key, value_hash }));
            }

            // Internal node — split proof keys and recurse into children.
            Some((_, Node::Internal { .. })) => {
                self.collect_internal(&key, leaf_keys);
            }
        }
    }

    /// Collects proof entries for an empty subtree — all proof keys are absent.
    ///
    /// Emits each absent key as an empty leaf. If there are multiple absent keys, uses topology
    /// bits to separate them by bit position so the verifier can reconstruct the correct subtree
    /// structure.
    fn collect_empty(&mut self, leaf_keys: &[[u8; 32]], level: u16) {
        if leaf_keys.len() == 1 {
            // Single absent key — emit as an empty leaf at this depth.
            self.leaves
                .push((level, StateCommitment { key: leaf_keys[0], value_hash: EMPTY_HASH }));
            return;
        }

        // Multiple absent keys — split by bit to produce the topology the verifier needs.
        let mid = leaf_keys.partition_point(|k| !k.get_msb(level as usize));

        if mid > 0 && mid < leaf_keys.len() {
            // Both sides have absent keys — emit a split topology bit.
            self.topology_bits.push(true);
            self.collect_empty(&leaf_keys[..mid], level + 1);
            self.collect_empty(&leaf_keys[mid..], level + 1);
        } else {
            // All absent keys on one side — emit sibling (EMPTY_HASH) for the empty side.
            self.topology_bits.push(false);
            self.siblings.push(EMPTY_HASH);
            let nonempty = if mid == 0 { &leaf_keys[mid..] } else { &leaf_keys[..mid] };
            self.collect_empty(nonempty, level + 1);
        }
    }

    /// Collects proof entries when the current position is an internal node.
    ///
    /// Splits proof keys by the current bit and recurses. If only one side has proof keys, the
    /// other side's subtree hash is emitted as a sibling.
    fn collect_internal(&mut self, key: &Key, leaf_keys: &[[u8; 32]]) {
        let mid = leaf_keys.partition_point(|k| !k.get_msb(key.level as usize));
        let (left_keys, right_keys) = leaf_keys.split_at(mid);

        let left_child = Key { level: key.level + 1, path: key.path };
        let right_child =
            Key { level: key.level + 1, path: key.path.with_bit_set(key.level as usize) };

        if !left_keys.is_empty() && !right_keys.is_empty() {
            // Both children have proof keys — emit a split topology bit and recurse both sides.
            self.topology_bits.push(true);
            self.collect(left_child, left_keys);
            self.collect(right_child, right_keys);
        } else {
            // Only one side has proof keys — emit the other side's hash as a sibling.
            self.topology_bits.push(false);

            if left_keys.is_empty() {
                // Proof keys are on the right — left subtree hash becomes a sibling.
                self.siblings.push(self.node_hash(&left_child));
                self.collect(right_child, right_keys);
            } else {
                // Proof keys are on the left — right subtree hash becomes a sibling.
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
