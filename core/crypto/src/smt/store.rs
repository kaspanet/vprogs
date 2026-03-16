use alloc::vec::Vec;

use super::{
    bits::{Bits, BitsArray},
    key::Key,
    proof::Proof,
    stale_node::StaleNode,
    state_commitment::StateCommitment,
    write_batch::WriteBatch,
};
use crate::{Blake3Hasher, EMPTY_HASH, Hasher, Node, TREE_DEPTH};

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
    fn commit_state_diffs<D>(&self, wb: &mut impl WriteBatch, version: u64, diffs: &[D]) -> [u8; 32]
    where
        Self: Sized,
        for<'a> StateCommitment: From<&'a D>,
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

        let mut leaves: Vec<(u16, StateCommitment)> = Vec::new();
        let mut siblings: Vec<[u8; 32]> = Vec::new();
        let mut topology_bits: Vec<bool> = Vec::new();

        build_proof(
            self,
            &sorted_keys,
            0,
            [0u8; 32],
            version,
            &mut leaves,
            &mut siblings,
            &mut topology_bits,
        );

        Proof::encode(&leaves, &siblings, &topology_bits)
    }
}

// --- Tree update (private) ---

/// Applies leaf mutations to the tree and writes all resulting nodes directly into `wb`.
///
/// Returns the new root hash.
fn update<S: Store, D>(
    store: &S,
    wb: &mut impl WriteBatch,
    prev_version: u64,
    version: u64,
    leaf_updates: &[D],
) -> [u8; 32]
where
    for<'a> StateCommitment: From<&'a D>,
{
    // Convert, sort, and deduplicate by key. All recursive methods below operate on sub-slices
    // of this vec — no further allocations on the common (internal-node) path.
    let mut updates: Vec<StateCommitment> =
        leaf_updates.iter().map(StateCommitment::from).collect();
    updates.sort_by(|a, b| a.key.cmp(&b.key));
    updates.dedup_by(|a, b| a.key == b.key);

    // Recursively apply updates starting from the root.
    let result = update_subtree(store, &updates, 0, [0u8; 32], prev_version, version, wb);

    // Write the result at the root position.
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
/// `updates` is a sorted sub-slice of leaf mutations that apply to this subtree. Splitting uses
/// `partition_point` + `split_at` for zero-allocation descent.
///
/// **Important:** returned `Leaf` nodes are *not* written to the store by this function — they
/// bubble up to the caller, which decides whether to write them as a child of an Internal node or
/// as the root. This enables leaf shortcutting where a single occupant floats to the highest
/// ancestor.
#[allow(clippy::too_many_arguments)]
fn update_subtree<S: Store>(
    store: &S,
    updates: &[StateCommitment],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    let node_key = Key { bit_pos: bit_pos as u16, path };

    // No updates for this subtree — return existing node unchanged.
    if updates.is_empty() {
        return store.get_node(&node_key, prev_version).map(|(_, data)| data);
    }

    // Look up existing node at this position to determine the update strategy.
    let existing = store.get_node(&node_key, prev_version).map(|(_, data)| data);

    match existing {
        // Empty subtree: resolve updates into leaves directly.
        None => resolve_leaves(store, updates, bit_pos, path, prev_version, version, wb),

        // Existing shortcut leaf: may need to split if keys differ.
        Some(Node::Leaf { key: existing_key, value_hash: existing_vh, .. }) => update_at_leaf(
            store,
            updates,
            bit_pos,
            path,
            prev_version,
            version,
            wb,
            existing_key,
            existing_vh,
        ),

        // Existing internal node: mark stale and recurse into children.
        Some(Node::Internal { .. }) => {
            mark_stale(store, &node_key, prev_version, version, wb);
            split_and_recurse(store, updates, bit_pos, path, prev_version, version, wb)
        }
    }
}

/// Resolves a sorted set of leaf updates into the appropriate tree structure.
///
/// Filters out deletions (`EMPTY_HASH`), then creates a shortcut leaf for a single live entry or
/// splits and recurses for multiple. Deletions in empty subtrees are harmless no-ops and propagate
/// down to be filtered at leaf boundaries.
#[allow(clippy::too_many_arguments)]
fn resolve_leaves<S: Store>(
    store: &S,
    updates: &[StateCommitment],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    let mut live = updates.iter().filter(|u| u.value_hash != EMPTY_HASH);

    let first = live.next()?;

    if live.next().is_none() {
        // Exactly one live entry — create a shortcut leaf at this depth.
        let hash = Blake3Hasher::hash_leaf(&first.key, &first.value_hash);
        return Some(Node::Leaf { key: first.key, value_hash: first.value_hash, hash });
    }

    // Multiple live entries — must split by the current bit and recurse.
    split_and_recurse(store, updates, bit_pos, path, prev_version, version, wb)
}

/// Handles updates at a position that currently holds a shortcut leaf.
#[allow(clippy::too_many_arguments)]
fn update_at_leaf<S: Store>(
    store: &S,
    updates: &[StateCommitment],
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
        resolve_leaves(store, updates, bit_pos, path, prev_version, version, wb)
    } else {
        let existing = StateCommitment { key: existing_key, value_hash: existing_vh };
        let pos = updates.partition_point(|u| u.key < existing_key);
        let mut merged = Vec::with_capacity(updates.len() + 1);
        merged.extend_from_slice(&updates[..pos]);
        merged.push(existing);
        merged.extend_from_slice(&updates[pos..]);
        resolve_leaves(store, &merged, bit_pos, path, prev_version, version, wb)
    }
}

/// Splits updates by the current bit and recurses into both children.
///
/// Uses `partition_point` to find the split in O(log n) with zero allocation. After recursion,
/// decides the return value: both empty → None, one leaf + one empty → bubble the leaf up
/// (shortcutting), otherwise → create Internal node.
#[allow(clippy::too_many_arguments)]
fn split_and_recurse<S: Store>(
    store: &S,
    updates: &[StateCommitment],
    bit_pos: usize,
    path: [u8; 32],
    prev_version: u64,
    version: u64,
    wb: &mut impl WriteBatch,
) -> Option<Node> {
    debug_assert!(bit_pos < TREE_DEPTH, "exceeded tree depth");

    // Partition by the bit at the current depth. Since updates are sorted MSB-first, all bit=0
    // keys precede bit=1 keys — so `partition_point` finds the exact boundary.
    let mid = updates.partition_point(|u| !u.key.get_msb(bit_pos));
    let (left, right) = updates.split_at(mid);

    let left_path = path;
    let right_path = path.with_bit_set(bit_pos);

    // Recurse into both children independently.
    let left_result =
        update_subtree(store, left, bit_pos + 1, left_path, prev_version, version, wb);
    let right_result =
        update_subtree(store, right, bit_pos + 1, right_path, prev_version, version, wb);

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
/// Walks the persistent store top-down. `keys` is a sorted sub-slice narrowed at each level. At
/// each position it looks up the stored node: **None** (empty subtree) means all proof keys are
/// absent; **Leaf** emits the shortcut leaf as a proof entry; **Internal** splits proof keys by
/// current bit and recurses.
#[allow(clippy::too_many_arguments)]
fn build_proof<S: Store>(
    store: &S,
    keys: &[[u8; 32]],
    bit_pos: usize,
    path: [u8; 32],
    version: u64,
    leaves: &mut Vec<(u16, StateCommitment)>,
    siblings: &mut Vec<[u8; 32]>,
    topology_bits: &mut Vec<bool>,
) {
    if keys.is_empty() {
        return;
    }

    let node_key = Key { bit_pos: bit_pos as u16, path };
    let existing = store.get_node(&node_key, version);

    match existing {
        // Empty subtree — all requested keys are absent.
        None => build_proof_empty(keys, bit_pos, leaves, siblings, topology_bits),

        // Shortcut leaf — emit it as the proof entry for this subtree.
        Some((_, Node::Leaf { key: leaf_key, value_hash, .. })) => {
            leaves.push((bit_pos as u16, StateCommitment { key: leaf_key, value_hash }));
        }

        // Internal node — split proof keys and recurse into children.
        Some((_, Node::Internal { .. })) => {
            build_proof_internal(
                store,
                keys,
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
    bit_pos: usize,
    leaves: &mut Vec<(u16, StateCommitment)>,
    siblings: &mut Vec<[u8; 32]>,
    topology_bits: &mut Vec<bool>,
) {
    if keys.len() == 1 {
        // Single absent key — emit as an empty leaf at this depth.
        leaves.push((bit_pos as u16, StateCommitment { key: keys[0], value_hash: EMPTY_HASH }));
        return;
    }

    // Multiple absent keys — split by bit to produce the topology the verifier needs.
    let mid = keys.partition_point(|k| !k.get_msb(bit_pos));

    if mid > 0 && mid < keys.len() {
        // Both sides have absent keys — emit a split topology bit.
        topology_bits.push(true);
        build_proof_empty(&keys[..mid], bit_pos + 1, leaves, siblings, topology_bits);
        build_proof_empty(&keys[mid..], bit_pos + 1, leaves, siblings, topology_bits);
    } else {
        // All absent keys on one side — emit sibling (EMPTY_HASH) for the empty side.
        topology_bits.push(false);
        siblings.push(EMPTY_HASH);
        let nonempty = if mid == 0 { &keys[mid..] } else { &keys[..mid] };
        build_proof_empty(nonempty, bit_pos + 1, leaves, siblings, topology_bits);
    }
}

/// Builds proof entries when the current position is an internal node.
///
/// Splits proof keys by the current bit and recurses. If only one side has proof keys, the
/// other side's subtree hash is emitted as a sibling.
#[allow(clippy::too_many_arguments)]
fn build_proof_internal<S: Store>(
    store: &S,
    keys: &[[u8; 32]],
    bit_pos: usize,
    path: [u8; 32],
    version: u64,
    leaves: &mut Vec<(u16, StateCommitment)>,
    siblings: &mut Vec<[u8; 32]>,
    topology_bits: &mut Vec<bool>,
) {
    let mid = keys.partition_point(|k| !k.get_msb(bit_pos));
    let (left_keys, right_keys) = keys.split_at(mid);

    let left_path = path;
    let right_path = path.with_bit_set(bit_pos);

    if !left_keys.is_empty() && !right_keys.is_empty() {
        // Both children have proof keys — emit a split topology bit and recurse both sides.
        topology_bits.push(true);
        build_proof(
            store,
            left_keys,
            bit_pos + 1,
            left_path,
            version,
            leaves,
            siblings,
            topology_bits,
        );
        build_proof(
            store,
            right_keys,
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

        if left_keys.is_empty() {
            // Proof keys are on the right — left subtree hash becomes a sibling.
            let left_key = Key { bit_pos: (bit_pos + 1) as u16, path: left_path };
            siblings.push(node_hash(store, &left_key, version));
            build_proof(
                store,
                right_keys,
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
            siblings.push(node_hash(store, &right_key, version));
            build_proof(
                store,
                left_keys,
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
