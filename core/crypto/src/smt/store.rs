use alloc::{vec, vec::Vec};

use super::{
    key::{Key, get_key_bit, set_key_bit},
    leaf_entry::LeafEntry,
    proof::LEAF_ENTRY_SIZE,
    stale_node::StaleNode,
    state_commitment::StateCommitment,
    write_batch::WriteBatch,
};
use crate::{Blake3Hasher, EMPTY_HASH, Hasher, Node, TREE_DEPTH};

/// Key-value pair representing a single leaf mutation: `(key, value_hash)`.
type LeafUpdate = ([u8; 32], [u8; 32]);

/// Authenticated state store backed by a versioned Sparse Merkle Tree.
///
/// Provides version-aware node lookups and tree mutations. Implementors only need to provide
/// `get_node`; all tree operations (commits, proofs) are default methods. Concrete implementations
/// use descending-version key encoding so that a single `prefix_iter` seek resolves "latest
/// version <= max_version" in O(1) I/O.
pub trait Store {
    /// Returns the node data and version of the latest SMT node at `key` where
    /// version <= `max_version`, or `None` if no such node exists.
    fn get_node(&self, key: &Key, max_version: u64) -> Option<(u64, Node)>;

    /// Returns the state root hash at the given version, or `EMPTY_HASH` if no root exists.
    fn get_root(&self, version: u64) -> [u8; 32] {
        if version == 0 {
            return EMPTY_HASH;
        }
        self.get_node(&Key::root(), version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
    }

    /// Commits state changes to the tree at the given version.
    ///
    /// Reads the previous root from the store, applies the state commitments as leaf mutations,
    /// writes the resulting nodes into `wb`, and returns the new root hash. No-op for empty diffs.
    fn commit_state_diffs<D: StateCommitment>(
        &self,
        wb: &mut impl WriteBatch,
        version: u64,
        diffs: &[D],
    ) -> [u8; 32]
    where
        Self: Sized,
    {
        if diffs.is_empty() {
            return self.get_root(version.saturating_sub(1));
        }
        let prev_version = version.saturating_sub(1);
        update(self, wb, prev_version, version, diffs)
    }

    /// Generates a multi-proof for the given keys at a specific version.
    ///
    /// Walks the persistent node store to collect sibling hashes and leaf depths. Returns the
    /// proof encoded in the v2 wire format, ready for transmission. Decode with
    /// `Proof::decode()` for verification. The version must not have been pruned.
    fn generate_proof(&self, version: u64, keys: &[[u8; 32]]) -> Vec<u8>
    where
        Self: Sized,
    {
        let mut sorted_keys: Vec<[u8; 32]> = keys.to_vec();
        sorted_keys.sort();
        sorted_keys.dedup();

        let mut leaves: Vec<LeafEntry> = Vec::new();
        let mut siblings: Vec<[u8; 32]> = Vec::new();
        let mut topology_bits: Vec<bool> = Vec::new();

        let indices: Vec<usize> = (0..sorted_keys.len()).collect();

        build_proof(
            self,
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
        encode_proof(&leaves, &siblings, &topology)
    }
}

// --- Tree update (private) ---

/// Applies leaf mutations to the tree and writes all resulting nodes directly into `wb`.
///
/// Returns the new root hash.
fn update<S: Store, D: StateCommitment>(
    store: &S,
    wb: &mut impl WriteBatch,
    prev_version: u64,
    version: u64,
    leaf_updates: &[D],
) -> [u8; 32] {
    // Materialize (key, value_hash) pairs once, then sort and deduplicate by key. All recursive
    // methods below work with indices into this vec — no further copies of the 64-byte tuples
    // are made on the common path.
    let mut updates: Vec<LeafUpdate> =
        leaf_updates.iter().map(|d| (d.key(), d.value_hash())).collect();
    updates.sort_by(|a, b| a.0.cmp(&b.0));
    updates.dedup_by(|a, b| a.0 == b.0);

    let indices: Vec<usize> = (0..updates.len()).collect();

    // Recursively apply updates starting from the root.
    let result = update_subtree(store, &updates, &indices, 0, [0u8; 32], prev_version, version, wb);

    // Write the result at the root position and update the tree state.
    let root_key = Key::root();
    mark_stale(store, &root_key, prev_version, version, wb);

    match &result {
        None => EMPTY_HASH,
        Some(node) => {
            wb.put_node(&root_key, version, node);
            *node.hash()
        }
    }
}

/// Core recursive update returning `None` for empty subtrees or the node that occupies this
/// position.
///
/// `updates` is the full sorted array of leaf mutations; `indices` selects which elements apply to
/// this subtree. Only indices are copied during splits — the underlying data is shared.
///
/// **Important:** returned `Leaf` nodes are *not* written to the store by this function — they
/// bubble up to the caller, which decides whether to write them as a child of an Internal node or
/// as the root. This enables leaf shortcutting where a single occupant floats to the highest
/// ancestor.
#[allow(clippy::too_many_arguments)]
fn update_subtree<S: Store>(
    store: &S,
    updates: &[LeafUpdate],
    indices: &[usize],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    let node_key = Key { bit_pos: bit_pos as u16, path };

    // No updates for this subtree — return existing node unchanged.
    if indices.is_empty() {
        return store.get_node(&node_key, prev_version).map(|(_, data)| data);
    }

    // Look up existing node at this position to determine the update strategy.
    let existing = store.get_node(&node_key, prev_version).map(|(_, data)| data);

    match existing {
        // Empty subtree: any updates create new leaves.
        None => {
            insert_into_empty(store, updates, indices, bit_pos, path, prev_version, version, wb)
        }

        // Existing shortcut leaf: may need to split if keys differ.
        Some(Node::Leaf { key: existing_key, value_hash: existing_vh, .. }) => update_at_leaf(
            store,
            updates,
            indices,
            bit_pos,
            path,
            prev_version,
            version,
            wb,
            existing_key,
            existing_vh,
        ),

        // Existing internal node: recurse into children.
        Some(Node::Internal { .. }) => {
            update_at_internal(store, updates, indices, bit_pos, path, prev_version, version, wb)
        }
    }
}

/// Inserts updates into a currently empty subtree.
#[allow(clippy::too_many_arguments)]
fn insert_into_empty<S: Store>(
    store: &S,
    updates: &[LeafUpdate],
    indices: &[usize],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    // Filter out deletions — setting EMPTY_HASH in an empty subtree is a no-op.
    let live: Vec<usize> =
        indices.iter().filter(|&&i| updates[i].1 != EMPTY_HASH).copied().collect();

    if live.is_empty() {
        return None;
    }

    if live.len() == 1 {
        // Single insert — create a shortcut leaf at this depth (no internal nodes needed).
        let (key, value_hash) = updates[live[0]];
        let hash = Blake3Hasher::hash_leaf(&key, &value_hash);
        return Some(Node::Leaf { key, value_hash, hash });
    }

    // Multiple inserts — must split by the current bit and recurse.
    split_and_recurse(store, updates, &live, bit_pos, path, prev_version, version, wb)
}

/// Handles updates at a position that currently holds a shortcut leaf.
#[allow(clippy::too_many_arguments)]
fn update_at_leaf<S: Store>(
    store: &S,
    updates: &[LeafUpdate],
    indices: &[usize],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
    existing_key: [u8; 32],
    existing_vh: [u8; 32],
) -> Option<Node> {
    // The existing leaf is being superseded regardless of outcome.
    let node_key = Key { bit_pos: bit_pos as u16, path };
    mark_stale(store, &node_key, prev_version, version, wb);

    // Check if any update targets the same key as the existing leaf.
    let same_key_idx = indices.iter().copied().find(|&i| updates[i].0 == existing_key);

    if let Some(idx) = same_key_idx {
        if indices.len() == 1 {
            // Simple case: single update replaces the existing leaf in-place.
            let new_vh = updates[idx].1;
            if new_vh == EMPTY_HASH {
                return None; // Deletion — subtree becomes empty.
            }
            let hash = Blake3Hasher::hash_leaf(&existing_key, &new_vh);
            return Some(Node::Leaf { key: existing_key, value_hash: new_vh, hash });
        }
    }

    // Complex case: merge the existing leaf into the update set (unless it's already being
    // updated) and split. This is the one place where we materialize a new vec — unavoidable
    // because we need to add the existing key to the set.
    let mut merged: Vec<LeafUpdate> = indices.iter().map(|&i| updates[i]).collect();
    if same_key_idx.is_none() {
        merged.push((existing_key, existing_vh));
    }

    // Filter out deletions after merging.
    let live: Vec<usize> = (0..merged.len()).filter(|&i| merged[i].1 != EMPTY_HASH).collect();

    if live.is_empty() {
        return None;
    }

    if live.len() == 1 {
        // Only one key survives — create a new shortcut leaf (may be the existing key or the new
        // key, depending on which ones were deleted).
        let (key, value_hash) = merged[live[0]];
        let hash = Blake3Hasher::hash_leaf(&key, &value_hash);
        return Some(Node::Leaf { key, value_hash, hash });
    }

    split_and_recurse(store, &merged, &live, bit_pos, path, prev_version, version, wb)
}

/// Handles updates at a position that holds an internal node.
#[allow(clippy::too_many_arguments)]
fn update_at_internal<S: Store>(
    store: &S,
    updates: &[LeafUpdate],
    indices: &[usize],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    // The existing internal node is superseded — a new one will be created after recursion.
    let node_key = Key { bit_pos: bit_pos as u16, path };
    mark_stale(store, &node_key, prev_version, version, wb);

    split_and_recurse(store, updates, indices, bit_pos, path, prev_version, version, wb)
}

/// Splits updates by the current bit and recurses into both children.
///
/// After recursion, decides the return value based on child results: both empty -> None, one
/// leaf + one empty -> bubble the leaf up (shortcutting), otherwise -> create Internal node.
#[allow(clippy::too_many_arguments)]
fn split_and_recurse<S: Store>(
    store: &S,
    updates: &[LeafUpdate],
    indices: &[usize],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    debug_assert!(bit_pos < TREE_DEPTH, "exceeded tree depth");

    // Partition indices by the bit at the current depth: 0 -> left, 1 -> right.
    let (left, right) = split_by_bit(indices, |i| &updates[i].0, bit_pos);

    let left_path = path;
    let mut right_path = path;
    set_key_bit(&mut right_path, bit_pos);

    // Recurse into both children independently.
    let left_result =
        update_subtree(store, updates, &left, bit_pos + 1, left_path, prev_version, version, wb);
    let right_result =
        update_subtree(store, updates, &right, bit_pos + 1, right_path, prev_version, version, wb);

    match (&left_result, &right_result) {
        // Both children empty — this subtree is empty.
        (None, None) => None,

        // One child is a leaf, the other is empty — bubble the leaf up. This is the core
        // shortcutting mechanism: the single occupant doesn't need an internal node above it.
        (Some(Node::Leaf { .. }), None) => left_result,
        (None, Some(Node::Leaf { .. })) => right_result,

        // Otherwise (both non-empty, or at least one Internal) — write children to the store and
        // create an Internal node.
        _ => {
            let left_hash = write_child(&left_result, bit_pos + 1, left_path, version, wb);
            let right_hash = write_child(&right_result, bit_pos + 1, right_path, version, wb);
            let hash = Blake3Hasher::hash_internal(&left_hash, &right_hash);
            Some(Node::Internal { hash })
        }
    }
}

/// Writes a child node to the store and returns its hash.
///
/// If the child is `None`, returns `EMPTY_HASH` without writing.
fn write_child(
    child: &Option<Node>,
    bit_pos: usize,
    path: [u8; 32],
    version: u64,
    wb: &mut impl WriteBatch,
) -> [u8; 32] {
    match child {
        None => EMPTY_HASH,
        Some(node) => {
            let key = Key { bit_pos: bit_pos as u16, path };
            let hash = *node.hash();
            wb.put_node(&key, version, node);
            hash
        }
    }
}

/// Marks an existing node at the given position as stale (if it exists).
fn mark_stale<S: Store>(
    store: &S,
    node_key: &Key,
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) {
    if let Some((old_version, _)) = store.get_node(node_key, prev_version) {
        wb.put_stale_node(&StaleNode {
            stale_since_version: version,
            node_key: node_key.clone(),
            node_version: old_version,
        });
    }
}

// --- Proof generation (private) ---

/// Recursive multi-proof builder.
///
/// Walks the persistent store top-down. At each position it looks up the stored node: **None**
/// (empty subtree) means all proof keys are absent; **Leaf** emits the shortcut leaf as a proof
/// entry; **Internal** splits proof keys by current bit and recurses.
#[allow(clippy::too_many_arguments)]
fn build_proof<S: Store>(
    store: &S,
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

    let node_key = Key { bit_pos: bit_pos as u16, path };
    let existing = store.get_node(&node_key, version);

    match existing {
        // Empty subtree — all requested keys are absent.
        None => {
            build_proof_empty(keys, indices, bit_pos, leaves, siblings, topology_bits);
        }

        // Shortcut leaf — emit it as the proof entry for this subtree.
        Some((_, Node::Leaf { key: leaf_key, value_hash, .. })) => {
            leaves.push(LeafEntry { depth: bit_pos as u16, key: leaf_key, value_hash });
        }

        // Internal node — split proof keys and recurse into children.
        Some((_, Node::Internal { .. })) => {
            build_proof_internal(
                store,
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
/// Emits each absent key as an empty leaf. If there are multiple absent keys, uses topology bits
/// to separate them by bit position so the verifier can reconstruct the correct subtree structure.
fn build_proof_empty(
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
    let (left, right) = split_by_bit(indices, |i| &keys[i], bit_pos);

    if !left.is_empty() && !right.is_empty() {
        // Both sides have absent keys — emit a split topology bit.
        topology_bits.push(true);
        build_proof_empty(keys, &left, bit_pos + 1, leaves, siblings, topology_bits);
        build_proof_empty(keys, &right, bit_pos + 1, leaves, siblings, topology_bits);
    } else {
        // All absent keys on one side — emit sibling (EMPTY_HASH) for the empty side.
        topology_bits.push(false);
        siblings.push(EMPTY_HASH);
        let nonempty = if left.is_empty() { &right } else { &left };
        build_proof_empty(keys, nonempty, bit_pos + 1, leaves, siblings, topology_bits);
    }
}

/// Builds proof entries when the current position is an internal node.
///
/// Splits proof key indices by the current bit and recurses. If only one side has proof keys, the
/// other side's subtree hash is emitted as a sibling.
#[allow(clippy::too_many_arguments)]
fn build_proof_internal<S: Store>(
    store: &S,
    keys: &[[u8; 32]],
    indices: &[usize],
    bit_pos: usize,
    path: [u8; 32],
    version: u64,
    leaves: &mut Vec<LeafEntry>,
    siblings: &mut Vec<[u8; 32]>,
    topology_bits: &mut Vec<bool>,
) {
    let (left_indices, right_indices) = split_by_bit(indices, |i| &keys[i], bit_pos);

    let left_path = path;
    let mut right_path = path;
    set_key_bit(&mut right_path, bit_pos);

    if !left_indices.is_empty() && !right_indices.is_empty() {
        // Both children have proof keys — emit a split topology bit and recurse both sides.
        topology_bits.push(true);
        build_proof(
            store,
            keys,
            &left_indices,
            bit_pos + 1,
            left_path,
            version,
            leaves,
            siblings,
            topology_bits,
        );
        build_proof(
            store,
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
            let left_key = Key { bit_pos: (bit_pos + 1) as u16, path: left_path };
            let sibling_hash = node_hash(store, &left_key, version);
            siblings.push(sibling_hash);
            build_proof(
                store,
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
            let right_key = Key { bit_pos: (bit_pos + 1) as u16, path: right_path };
            let sibling_hash = node_hash(store, &right_key, version);
            siblings.push(sibling_hash);
            build_proof(
                store,
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
fn node_hash<S: Store>(store: &S, node_key: &Key, version: u64) -> [u8; 32] {
    store.get_node(node_key, version).map(|(_, data)| *data.hash()).unwrap_or(EMPTY_HASH)
}

// --- Shared helpers ---

/// Splits indices into left (bit=0) and right (bit=1) by the current bit position.
///
/// The `key_fn` closure returns a reference to the 32-byte key for a given index, avoiding copies.
/// Works with both leaf updates (`&updates[i].0`) and proof keys (`&keys[i]`).
fn split_by_bit<'a>(
    indices: &[usize],
    key_fn: impl Fn(usize) -> &'a [u8; 32],
    bit_pos: usize,
) -> (Vec<usize>, Vec<usize>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for &i in indices {
        if get_key_bit(key_fn(i), bit_pos) {
            right.push(i);
        } else {
            left.push(i);
        }
    }
    (left, right)
}

/// Encodes proof components into the v2 wire format.
fn encode_proof(leaves: &[LeafEntry], siblings: &[[u8; 32]], topology: &[u8]) -> Vec<u8> {
    let total = 4 + leaves.len() * LEAF_ENTRY_SIZE + 4 + siblings.len() * 32 + 4 + topology.len();
    let mut buf = Vec::with_capacity(total);

    // Section 1: leaf entries — each is depth(2) + key(32) + value_hash(32) = 66 bytes.
    buf.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
    for leaf in leaves {
        buf.extend_from_slice(&leaf.depth.to_le_bytes());
        buf.extend_from_slice(&leaf.key);
        buf.extend_from_slice(&leaf.value_hash);
    }

    // Section 2: sibling hashes — each is 32 bytes.
    buf.extend_from_slice(&(siblings.len() as u32).to_le_bytes());
    for sibling in siblings {
        buf.extend_from_slice(sibling);
    }

    // Section 3: topology bitfield — variable length.
    buf.extend_from_slice(&(topology.len() as u32).to_le_bytes());
    buf.extend_from_slice(topology);

    debug_assert_eq!(buf.len(), total);
    buf
}

/// Packs a slice of bools into a byte vector (LSB-first within each byte).
///
/// This matches the topology bitfield format read by `Proof::get_topo_bit`.
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
