use alloc::{vec, vec::Vec};
use core::marker::PhantomData;

use crate::{
    EMPTY_HASH, Hasher, MultiProof, NodeData, TREE_DEPTH,
    leaf_entry::LeafEntry,
    node_key::{NodeKey, get_key_bit, set_key_bit},
    stale_node::StaleNode,
    tree_store::TreeStore,
    tree_update_batch::TreeUpdateBatch,
};

/// A persistent, versioned binary Sparse Merkle Tree with leaf shortcutting.
///
/// Each mutation creates a new version with structural sharing — only nodes on the modified path
/// are written; unmodified subtrees reference nodes from earlier versions. Subtrees with a single
/// occupant are represented as a shortcut leaf at the highest ancestor, reducing effective depth
/// from 256 to O(log n) for n entries.
pub struct VersionedTree<H: Hasher, S: TreeStore> {
    store: S,
    current_version: u64,
    current_root: [u8; 32],
    _hasher: PhantomData<H>,
}

impl<H: Hasher, S: TreeStore> VersionedTree<H, S> {
    /// Creates a new empty tree.
    pub fn new(store: S) -> Self {
        Self { store, current_version: 0, current_root: EMPTY_HASH, _hasher: PhantomData }
    }

    /// Returns the current root hash.
    pub fn root(&self) -> [u8; 32] {
        self.current_root
    }

    /// Returns the current version.
    pub fn version(&self) -> u64 {
        self.current_version
    }

    /// Returns a reference to the underlying store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Returns a mutable reference to the underlying store.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// Applies leaf mutations at a new version.
    ///
    /// `leaf_updates` is `(key, new_value_hash)` pairs. Use `EMPTY_HASH` as the value hash for
    /// deletion. Returns the update batch containing new and stale nodes.
    pub fn update(
        &mut self,
        version: u64,
        leaf_updates: &[([u8; 32], [u8; 32])],
    ) -> TreeUpdateBatch {
        let mut batch = TreeUpdateBatch {
            new_nodes: Vec::new(),
            stale_nodes: Vec::new(),
            root: self.current_root,
            version,
        };

        if leaf_updates.is_empty() {
            return batch;
        }

        // Sort and deduplicate updates by key for consistent left/right splitting.
        let mut sorted: Vec<([u8; 32], [u8; 32])> = leaf_updates.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        sorted.dedup_by(|a, b| a.0 == b.0);

        let refs: Vec<(&[u8; 32], [u8; 32])> = sorted.iter().map(|(k, h)| (k, *h)).collect();

        // Recursively apply updates starting from the root.
        let result = self.update_subtree(&refs, 0, [0u8; 32], version, &mut batch);

        // Write the result at the root position and update the tree state.
        let root_key = NodeKey::root();
        self.mark_stale(&root_key, version, &mut batch);

        let new_root = match &result {
            None => EMPTY_HASH,
            Some(node) => {
                batch.new_nodes.push((root_key, version, node.clone()));
                *node.hash()
            }
        };

        batch.root = new_root;
        self.store.apply_batch(&batch);
        self.current_version = version;
        self.current_root = new_root;

        batch
    }

    /// Core recursive update returning `None` for empty subtrees or the node that occupies this
    /// position.
    ///
    /// **Important:** returned `Leaf` nodes are *not* written to the store by this function — they
    /// bubble up to the caller, which decides whether to write them as a child of an Internal node
    /// or as the root. This enables leaf shortcutting where a single occupant floats to the highest
    /// ancestor.
    fn update_subtree(
        &self,
        updates: &[(&[u8; 32], [u8; 32])],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        batch: &mut TreeUpdateBatch,
    ) -> Option<NodeData> {
        let node_key = NodeKey { bit_pos: bit_pos as u16, path };

        // No updates for this subtree — return existing node unchanged.
        if updates.is_empty() {
            return self.store.get_node(&node_key, version).map(|(_, data)| data);
        }

        // Look up existing node at this position to determine the update strategy.
        let existing = self.store.get_node(&node_key, version).map(|(_, data)| data);

        match existing {
            // Empty subtree: any updates create new leaves.
            None => self.insert_into_empty(updates, bit_pos, path, version, batch),

            // Existing shortcut leaf: may need to split if keys differ.
            Some(NodeData::Leaf { key: existing_key, value_hash: existing_vh, .. }) => self
                .update_at_leaf(updates, bit_pos, path, version, batch, existing_key, existing_vh),

            // Existing internal node: recurse into children.
            Some(NodeData::Internal { .. }) => {
                self.update_at_internal(updates, bit_pos, path, version, batch)
            }
        }
    }

    /// Inserts updates into a currently empty subtree.
    fn insert_into_empty(
        &self,
        updates: &[(&[u8; 32], [u8; 32])],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        batch: &mut TreeUpdateBatch,
    ) -> Option<NodeData> {
        // Filter out deletions — setting EMPTY_HASH in an empty subtree is a no-op.
        let live: Vec<(&[u8; 32], [u8; 32])> =
            updates.iter().filter(|(_, vh)| *vh != EMPTY_HASH).copied().collect();

        if live.is_empty() {
            return None;
        }

        if live.len() == 1 {
            // Single insert — create a shortcut leaf at this depth (no internal nodes needed).
            let (key, value_hash) = live[0];
            let hash = H::hash_leaf(key, &value_hash);
            return Some(NodeData::Leaf { key: *key, value_hash, hash });
        }

        // Multiple inserts — must split by the current bit and recurse.
        self.split_and_recurse(&live, bit_pos, path, version, batch)
    }

    /// Handles updates at a position that currently holds a shortcut leaf.
    fn update_at_leaf(
        &self,
        updates: &[(&[u8; 32], [u8; 32])],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        batch: &mut TreeUpdateBatch,
        existing_key: [u8; 32],
        existing_vh: [u8; 32],
    ) -> Option<NodeData> {
        // The existing leaf is being superseded regardless of outcome.
        let node_key = NodeKey { bit_pos: bit_pos as u16, path };
        self.mark_stale(&node_key, version, batch);

        // Check if any update targets the same key as the existing leaf.
        let same_key_update = updates.iter().find(|(k, _)| **k == existing_key);

        if updates.len() == 1 && same_key_update.is_some() {
            // Simple case: single update replaces the existing leaf in-place.
            let new_vh = same_key_update.unwrap().1;
            if new_vh == EMPTY_HASH {
                return None; // Deletion — subtree becomes empty.
            }
            let hash = H::hash_leaf(&existing_key, &new_vh);
            return Some(NodeData::Leaf { key: existing_key, value_hash: new_vh, hash });
        }

        // Complex case: merge the existing leaf into the update set (unless it's already being
        // updated) and split. This handles the case where a new key is being inserted into a
        // subtree that was previously occupied by a single shortcut leaf.
        let mut merged: Vec<(&[u8; 32], [u8; 32])> = updates.to_vec();
        if same_key_update.is_none() {
            merged.push((&existing_key, existing_vh));
        }

        // Filter out deletions after merging.
        let live: Vec<(&[u8; 32], [u8; 32])> =
            merged.iter().filter(|(_, vh)| *vh != EMPTY_HASH).copied().collect();

        if live.is_empty() {
            return None;
        }

        if live.len() == 1 {
            // Only one key survives — create a new shortcut leaf (may be the existing key
            // or the new key, depending on which ones were deleted).
            let (key, value_hash) = live[0];
            let hash = H::hash_leaf(key, &value_hash);
            return Some(NodeData::Leaf { key: *key, value_hash, hash });
        }

        self.split_and_recurse(&live, bit_pos, path, version, batch)
    }

    /// Handles updates at a position that holds an internal node.
    fn update_at_internal(
        &self,
        updates: &[(&[u8; 32], [u8; 32])],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        batch: &mut TreeUpdateBatch,
    ) -> Option<NodeData> {
        // The existing internal node is superseded — a new one will be created after recursion.
        let node_key = NodeKey { bit_pos: bit_pos as u16, path };
        self.mark_stale(&node_key, version, batch);

        self.split_and_recurse(updates, bit_pos, path, version, batch)
    }

    /// Splits updates by the current bit and recurses into both children.
    ///
    /// After recursion, decides the return value based on child results: both empty → None, one
    /// leaf + one empty → bubble the leaf up (shortcutting), otherwise → create Internal node.
    fn split_and_recurse(
        &self,
        updates: &[(&[u8; 32], [u8; 32])],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        batch: &mut TreeUpdateBatch,
    ) -> Option<NodeData> {
        debug_assert!(bit_pos < TREE_DEPTH, "exceeded tree depth");

        // Partition updates by the bit at the current depth: 0 → left, 1 → right.
        let (left_updates, right_updates) = split_updates_by_bit(updates, bit_pos);

        let left_path = path;
        let mut right_path = path;
        set_key_bit(&mut right_path, bit_pos);

        // Recurse into both children independently.
        let left_result =
            self.update_subtree(&left_updates, bit_pos + 1, left_path, version, batch);
        let right_result =
            self.update_subtree(&right_updates, bit_pos + 1, right_path, version, batch);

        match (&left_result, &right_result) {
            // Both children empty — this subtree is empty.
            (None, None) => None,

            // One child is a leaf, the other is empty — bubble the leaf up. This is the core
            // shortcutting mechanism: the single occupant doesn't need an internal node above it.
            (Some(NodeData::Leaf { .. }), None) => left_result,
            (None, Some(NodeData::Leaf { .. })) => right_result,

            // Otherwise (both non-empty, or at least one Internal) — write children to the store
            // and create an Internal node.
            _ => {
                let left_hash =
                    self.write_child(&left_result, bit_pos + 1, left_path, version, batch);
                let right_hash =
                    self.write_child(&right_result, bit_pos + 1, right_path, version, batch);
                let hash = H::hash_internal(&left_hash, &right_hash);
                Some(NodeData::Internal { hash })
            }
        }
    }

    /// Writes a child node to the store and returns its hash.
    ///
    /// If the child is `None`, returns `EMPTY_HASH` without writing.
    fn write_child(
        &self,
        child: &Option<NodeData>,
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        batch: &mut TreeUpdateBatch,
    ) -> [u8; 32] {
        match child {
            None => EMPTY_HASH,
            Some(node) => {
                let key = NodeKey { bit_pos: bit_pos as u16, path };
                let hash = *node.hash();
                batch.new_nodes.push((key, version, node.clone()));
                hash
            }
        }
    }

    /// Marks an existing node at the given position as stale (if it exists).
    fn mark_stale(&self, node_key: &NodeKey, version: u64, batch: &mut TreeUpdateBatch) {
        if let Some((old_version, _)) = self.store.get_node(node_key, version) {
            batch.stale_nodes.push(StaleNode {
                stale_since_version: version,
                node_key: node_key.clone(),
                node_version: old_version,
            });
        }
    }

    // --- Proof generation ---

    /// Generates a multi-proof for the given keys at the current version.
    ///
    /// Shorthand for `multi_proof_at_version(keys, self.version())`.
    pub fn multi_proof(&self, keys: &[[u8; 32]]) -> MultiProof {
        self.multi_proof_at_version(keys, self.current_version)
    }

    /// Generates a multi-proof for the given keys at a specific version.
    ///
    /// Walks the persistent node store to collect sibling hashes and leaf depths. Returns a
    /// `MultiProof` that can be encoded into the v2 wire format. The version must not have been
    /// pruned.
    pub fn multi_proof_at_version(&self, keys: &[[u8; 32]], version: u64) -> MultiProof {
        // Sort and deduplicate proof keys for consistent bit-splitting.
        let mut sorted_keys: Vec<[u8; 32]> = keys.to_vec();
        sorted_keys.sort();
        sorted_keys.dedup();

        let mut leaves: Vec<LeafEntry> = Vec::new();
        let mut siblings: Vec<[u8; 32]> = Vec::new();
        let mut topology_bits: Vec<bool> = Vec::new();

        let indices: Vec<usize> = (0..sorted_keys.len()).collect();

        // Walk the tree top-down, collecting leaves, siblings, and topology.
        self.build_proof(
            &sorted_keys,
            &indices,
            0,
            [0u8; 32],
            version,
            &mut leaves,
            &mut siblings,
            &mut topology_bits,
        );

        let topology = pack_bits(&topology_bits);
        MultiProof { leaves, siblings, topology }
    }

    /// Recursive multi-proof builder.
    ///
    /// Walks the persistent store top-down. At each position it looks up the stored node: **None**
    /// (empty subtree) means all proof keys are absent; **Leaf** emits the shortcut leaf as a proof
    /// entry; **Internal** splits proof keys by current bit and recurses.
    #[allow(clippy::too_many_arguments)]
    fn build_proof(
        &self,
        keys: &[[u8; 32]],
        indices: &[usize],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        leaves: &mut Vec<LeafEntry>,
        siblings: &mut Vec<[u8; 32]>,
        topology_bits: &mut Vec<bool>,
    ) {
        if indices.is_empty() {
            return;
        }

        let node_key = NodeKey { bit_pos: bit_pos as u16, path };
        let existing = self.store.get_node(&node_key, version);

        match existing {
            // Empty subtree — all requested keys are absent.
            None => {
                self.build_proof_empty(keys, indices, bit_pos, leaves, siblings, topology_bits);
            }

            // Shortcut leaf — emit it as the proof entry for this subtree.
            Some((_, NodeData::Leaf { key: leaf_key, value_hash, .. })) => {
                self.build_proof_leaf(
                    keys,
                    indices,
                    bit_pos,
                    path,
                    version,
                    leaves,
                    siblings,
                    topology_bits,
                    leaf_key,
                    value_hash,
                );
            }

            // Internal node — split proof keys and recurse into children.
            Some((_, NodeData::Internal { .. })) => {
                self.build_proof_internal(
                    keys,
                    indices,
                    bit_pos,
                    path,
                    version,
                    leaves,
                    siblings,
                    topology_bits,
                );
            }
        }
    }

    /// Builds proof entries for an empty subtree — all proof keys are absent.
    ///
    /// Emits each absent key as an empty leaf. If there are multiple absent keys, uses topology
    /// bits to separate them by bit position so the verifier can reconstruct the correct subtree
    /// structure.
    #[allow(clippy::too_many_arguments)]
    fn build_proof_empty(
        &self,
        keys: &[[u8; 32]],
        indices: &[usize],
        bit_pos: usize,
        leaves: &mut Vec<LeafEntry>,
        siblings: &mut Vec<[u8; 32]>,
        topology_bits: &mut Vec<bool>,
    ) {
        if indices.len() == 1 {
            // Single absent key — emit as an empty leaf at this depth.
            leaves.push(LeafEntry {
                depth: bit_pos as u16,
                key: keys[indices[0]],
                value_hash: EMPTY_HASH,
            });
            return;
        }

        // Multiple absent keys — split by bit to produce the topology the verifier needs.
        let (left, right) = split_indices_by_bit(indices, keys, bit_pos);

        if !left.is_empty() && !right.is_empty() {
            // Both sides have absent keys — emit a split topology bit.
            topology_bits.push(true);
            self.build_proof_empty(keys, &left, bit_pos + 1, leaves, siblings, topology_bits);
            self.build_proof_empty(keys, &right, bit_pos + 1, leaves, siblings, topology_bits);
        } else {
            // All absent keys on one side — emit sibling (EMPTY_HASH) for the empty side.
            topology_bits.push(false);
            siblings.push(EMPTY_HASH);
            let nonempty = if left.is_empty() { &right } else { &left };
            self.build_proof_empty(keys, nonempty, bit_pos + 1, leaves, siblings, topology_bits);
        }
    }

    /// Builds a proof entry when we encounter a shortcut leaf.
    ///
    /// The shortcut leaf is always emitted as the proof entry at this depth, regardless of whether
    /// the proof keys match it. Callers detect non-existence by comparing `proof.leaf_key(i) !=
    /// queried_key`.
    #[allow(clippy::too_many_arguments)]
    fn build_proof_leaf(
        &self,
        keys: &[[u8; 32]],
        indices: &[usize],
        bit_pos: usize,
        _path: [u8; 32],
        _version: u64,
        leaves: &mut Vec<LeafEntry>,
        _siblings: &mut Vec<[u8; 32]>,
        _topology_bits: &mut Vec<bool>,
        leaf_key: [u8; 32],
        leaf_vh: [u8; 32],
    ) {
        // Emit the shortcut leaf as the proof entry at this depth, regardless of whether the
        // proof keys match it. Callers detect non-existence by comparing `leaf_key != queried_key`.
        leaves.push(LeafEntry { depth: bit_pos as u16, key: leaf_key, value_hash: leaf_vh });
    }

    /// Builds proof entries when the current position is an internal node.
    ///
    /// Splits proof key indices by the current bit and recurses. If only one side has proof keys,
    /// the other side's subtree hash is emitted as a sibling.
    #[allow(clippy::too_many_arguments)]
    fn build_proof_internal(
        &self,
        keys: &[[u8; 32]],
        indices: &[usize],
        bit_pos: usize,
        path: [u8; 32],
        version: u64,
        leaves: &mut Vec<LeafEntry>,
        siblings: &mut Vec<[u8; 32]>,
        topology_bits: &mut Vec<bool>,
    ) {
        let (left_indices, right_indices) = split_indices_by_bit(indices, keys, bit_pos);

        let left_path = path;
        let mut right_path = path;
        set_key_bit(&mut right_path, bit_pos);

        if !left_indices.is_empty() && !right_indices.is_empty() {
            // Both children have proof keys — emit a split topology bit and recurse both sides.
            topology_bits.push(true);
            self.build_proof(
                keys,
                &left_indices,
                bit_pos + 1,
                left_path,
                version,
                leaves,
                siblings,
                topology_bits,
            );
            self.build_proof(
                keys,
                &right_indices,
                bit_pos + 1,
                right_path,
                version,
                leaves,
                siblings,
                topology_bits,
            );
        } else {
            // Only one side has proof keys — emit the other side's hash as a sibling.
            topology_bits.push(false);

            if left_indices.is_empty() {
                // Proof keys are on the right — left subtree hash becomes a sibling.
                let left_key = NodeKey { bit_pos: (bit_pos + 1) as u16, path: left_path };
                let sibling_hash = self.node_hash(&left_key, version);
                siblings.push(sibling_hash);
                self.build_proof(
                    keys,
                    &right_indices,
                    bit_pos + 1,
                    right_path,
                    version,
                    leaves,
                    siblings,
                    topology_bits,
                );
            } else {
                // Proof keys are on the left — right subtree hash becomes a sibling.
                let right_key = NodeKey { bit_pos: (bit_pos + 1) as u16, path: right_path };
                let sibling_hash = self.node_hash(&right_key, version);
                siblings.push(sibling_hash);
                self.build_proof(
                    keys,
                    &left_indices,
                    bit_pos + 1,
                    left_path,
                    version,
                    leaves,
                    siblings,
                    topology_bits,
                );
            }
        }
    }

    /// Returns the hash of the node at the given key, or `EMPTY_HASH` if absent.
    fn node_hash(&self, node_key: &NodeKey, version: u64) -> [u8; 32] {
        self.store.get_node(node_key, version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }

    // --- Version management ---

    /// Rolls back to the given version.
    ///
    /// Nodes from later versions become garbage and can be cleaned up with `prune()`. This is an
    /// O(1) operation — it just switches the root pointer.
    pub fn rollback_to(&mut self, version: u64) {
        let node_key = NodeKey::root();
        self.current_root = self
            .store
            .get_node(&node_key, version)
            .map(|(_, data)| *data.hash())
            .unwrap_or(EMPTY_HASH);
        self.current_version = version;
    }

    /// Prunes all node versions that became stale at or before `oldest_readable_version`.
    pub fn prune(&mut self, oldest_readable_version: u64) {
        self.store.prune_stale(oldest_readable_version);
    }
}

// --- Free functions ---

type UpdateRef<'a> = (&'a [u8; 32], [u8; 32]);

/// Splits update refs into left (bit=0) and right (bit=1) by the current bit position.
fn split_updates_by_bit<'a>(
    updates: &[UpdateRef<'a>],
    bit_pos: usize,
) -> (Vec<UpdateRef<'a>>, Vec<UpdateRef<'a>>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for &(key, hash) in updates {
        if get_key_bit(key, bit_pos) {
            right.push((key, hash));
        } else {
            left.push((key, hash));
        }
    }
    (left, right)
}

/// Splits proof leaf indices into left (bit=0) and right (bit=1) by the current bit position.
fn split_indices_by_bit(
    indices: &[usize],
    keys: &[[u8; 32]],
    bit_pos: usize,
) -> (Vec<usize>, Vec<usize>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for &i in indices {
        if get_key_bit(&keys[i], bit_pos) {
            right.push(i);
        } else {
            left.push(i);
        }
    }
    (left, right)
}

/// Packs a slice of bools into a byte vector (LSB-first within each byte).
///
/// This matches the topology bitfield format read by `DecodedMultiProof::get_topo_bit`.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let n_bytes = bits.len().div_ceil(8);
    let mut bytes = vec![0u8; n_bytes];
    for (i, &bit) in bits.iter().enumerate() {
        if bit {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    bytes
}
