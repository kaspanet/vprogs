use tempfile::TempDir;
use vprogs_core_crypto::{
    Blake3Hasher, EMPTY_HASH, Hasher,
    smt::{NodeKey, StaleNode, TreeStore, VersionedTree},
};
use vprogs_state_smt::{SmtCommit, SmtMetadata};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch};

/// Creates a fresh RocksDB-backed store in a temporary directory.
fn setup() -> (RocksDbStore, TempDir) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let store = RocksDbStore::open(dir.path());
    (store, dir)
}

/// Computes the SMT update for the given leaf mutations, persists the resulting nodes and root
/// atomically, and returns the new root hash.
fn update(store: &RocksDbStore, version: u64, leaf_updates: &[([u8; 32], [u8; 32])]) -> [u8; 32] {
    let prev_root = SmtMetadata::root(store);
    let prev_version = version.saturating_sub(1);
    let mut tree = VersionedTree::<Blake3Hasher, _>::new_with(store, prev_version, prev_root);
    let batch = tree.update(version, leaf_updates);

    let mut wb = store.write_batch();
    SmtCommit::write_all(&mut wb, &batch);
    SmtMetadata::set_root(&mut wb, &batch.root);
    store.commit(wb);

    batch.root
}

/// Returns the current SMT root hash from the Metadata CF.
fn root(store: &RocksDbStore) -> [u8; 32] {
    SmtMetadata::root(store)
}

/// Resets the SMT root to the root at the given version (O(1) pointer swap).
fn rollback(store: &RocksDbStore, version: u64) {
    let rollback_root = if version == 0 {
        EMPTY_HASH
    } else {
        store
            .get_node(&NodeKey::root(), version)
            .map(|(_, data)| *data.hash())
            .unwrap_or(EMPTY_HASH)
    };
    let mut wb = store.write_batch();
    SmtMetadata::set_root(&mut wb, &rollback_root);
    store.commit(wb);
}

/// Prunes all stale SMT nodes for versions up to `oldest_readable`.
fn prune_up_to(store: &RocksDbStore, oldest_readable: u64) {
    let mut wb = store.write_batch();
    for version in 1..=oldest_readable {
        for (stale_key, stale_value) in
            store.prefix_iter(StateSpace::SmtStale, &version.to_be_bytes())
        {
            let (path, bit_pos) = StaleNode::decode_cf_key(&stale_key);
            let node_version = StaleNode::decode_cf_value(&stale_value);
            let node_key = NodeKey { bit_pos, path };
            wb.delete(StateSpace::SmtNode, &node_key.encode_cf_key(node_version));
            wb.delete(StateSpace::SmtStale, &stale_key);
        }
    }
    store.commit(wb);
}

/// Reconstructs a tree from the store at the current root for proof generation.
fn tree_at(store: &RocksDbStore, version: u64) -> VersionedTree<'_, Blake3Hasher, RocksDbStore> {
    let current_root = SmtMetadata::root(store);
    VersionedTree::new_with(store, version, current_root)
}

// --- Tree update tests ---

#[test]
fn empty_tree_has_empty_root() {
    let (store, _dir) = setup();
    assert_eq!(root(&store), EMPTY_HASH);
}

#[test]
fn single_insert_creates_shortcut_at_root() {
    let (store, _dir) = setup();
    let key = [0xABu8; 32];
    let vh = *blake3::hash(b"hello").as_bytes();

    update(&store, 1, &[(key, vh)]);

    // Root should be hash_leaf(key, vh) since the single leaf shortcuts to root.
    let expected = Blake3Hasher::hash_leaf(&key, &vh);
    assert_eq!(root(&store), expected);
}

#[test]
fn two_inserts_create_internal_at_divergence() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32]; // bit 0 = 0
    let mut k2 = [0u8; 32];
    k2[0] = 0x80; // bit 0 = 1

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2)]);

    // Root should be hash_internal(hash_leaf(k1, vh1), hash_leaf(k2, vh2)).
    let lh1 = Blake3Hasher::hash_leaf(&k1, &vh1);
    let lh2 = Blake3Hasher::hash_leaf(&k2, &vh2);
    let expected = Blake3Hasher::hash_internal(&lh1, &lh2);
    assert_eq!(root(&store), expected);
}

#[test]
fn delete_collapses_back_to_shortcut() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2)]);

    // Delete k1 — should collapse back to a single shortcut leaf for k2.
    update(&store, 2, &[(k1, EMPTY_HASH)]);

    let expected = Blake3Hasher::hash_leaf(&k2, &vh2);
    assert_eq!(root(&store), expected);
}

#[test]
fn delete_all_returns_empty() {
    let (store, _dir) = setup();
    let key = [1u8; 32];
    let vh = *blake3::hash(b"data").as_bytes();

    update(&store, 1, &[(key, vh)]);
    update(&store, 2, &[(key, EMPTY_HASH)]);

    assert_eq!(root(&store), EMPTY_HASH);
}

#[test]
fn update_existing_key() {
    let (store, _dir) = setup();
    let key = [0u8; 32];
    let vh1 = *blake3::hash(b"old").as_bytes();
    let vh2 = *blake3::hash(b"new").as_bytes();

    update(&store, 1, &[(key, vh1)]);
    update(&store, 2, &[(key, vh2)]);

    let expected = Blake3Hasher::hash_leaf(&key, &vh2);
    assert_eq!(root(&store), expected);
}

#[test]
fn three_keys_different_depths() {
    let (store, _dir) = setup();

    // k1 and k2 diverge at bit 0, k3 shares prefix with k1 but diverges at bit 1.
    let k1 = [0u8; 32]; // 0b00...
    let mut k2 = [0u8; 32];
    k2[0] = 0x80; // 0b10...
    let mut k3 = [0u8; 32];
    k3[0] = 0x40; // 0b01...

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    // Expected structure:
    //        Internal(root)
    //       /              \
    //   Internal         Leaf(k2, vh2)
    //   /      \
    // Leaf(k1) Leaf(k3)
    let lh1 = Blake3Hasher::hash_leaf(&k1, &vh1);
    let lh2 = Blake3Hasher::hash_leaf(&k2, &vh2);
    let lh3 = Blake3Hasher::hash_leaf(&k3, &vh3);
    let left_internal = Blake3Hasher::hash_internal(&lh1, &lh3);
    let expected = Blake3Hasher::hash_internal(&left_internal, &lh2);
    assert_eq!(root(&store), expected);
}

#[test]
fn delete_causes_collapse_of_internal() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;
    let mut k3 = [0u8; 32];
    k3[0] = 0x40;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    // Delete k3 — left subtree should collapse from Internal to Leaf(k1).
    update(&store, 2, &[(k3, EMPTY_HASH)]);

    let lh1 = Blake3Hasher::hash_leaf(&k1, &vh1);
    let lh2 = Blake3Hasher::hash_leaf(&k2, &vh2);
    let expected = Blake3Hasher::hash_internal(&lh1, &lh2);
    assert_eq!(root(&store), expected);
}

#[test]
fn rollback_restores_previous_root() {
    let (store, _dir) = setup();

    let key = [0u8; 32];
    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();

    update(&store, 1, &[(key, vh1)]);
    let root_v1 = root(&store);

    update(&store, 2, &[(key, vh2)]);
    assert_ne!(root(&store), root_v1);

    rollback(&store, 1);
    assert_eq!(root(&store), root_v1);
}

#[test]
fn prune_removes_old_versions() {
    let (store, _dir) = setup();

    let key = [0u8; 32];
    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();

    update(&store, 1, &[(key, vh1)]);
    update(&store, 2, &[(key, vh2)]);

    let root_v2 = root(&store);
    prune_up_to(&store, 2);

    assert_eq!(root(&store), root_v2);

    // Rollback after pruning — v1 nodes are gone, so root resolves to EMPTY_HASH.
    rollback(&store, 1);
    assert_eq!(root(&store), EMPTY_HASH);
}

#[test]
fn incremental_inserts_match_batch() {
    let (store_inc, _dir1) = setup();
    let (store_batch, _dir2) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();

    // Incremental: insert one at a time.
    update(&store_inc, 1, &[(k1, vh1)]);
    update(&store_inc, 2, &[(k2, vh2)]);

    // Batch: insert both at once.
    update(&store_batch, 1, &[(k1, vh1), (k2, vh2)]);

    assert_eq!(root(&store_inc), root(&store_batch));
}

// --- Proof generation + verification tests ---

#[test]
fn proof_single_key() {
    let (store, _dir) = setup();
    let key = [0xABu8; 32];
    let vh = *blake3::hash(b"hello").as_bytes();

    update(&store, 1, &[(key, vh)]);

    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&[key]);
    assert_eq!(proof.n_leaves(), 1);
    assert!(proof.verify::<Blake3Hasher>(root(&store)));
}

#[test]
fn proof_two_keys() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2)]);

    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&[k1, k2]);
    assert_eq!(proof.n_leaves(), 2);
    assert!(proof.verify::<Blake3Hasher>(root(&store)));
}

#[test]
fn proof_absent_key() {
    let (store, _dir) = setup();
    let key = [0u8; 32];
    let vh = *blake3::hash(b"data").as_bytes();

    update(&store, 1, &[(key, vh)]);

    // Prove a key that's not in the tree. The proof includes the shortcut leaf that
    // occupies the position — callers detect non-existence by checking key mismatch.
    let absent = [0xFFu8; 32];
    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&[absent]);

    assert_eq!(proof.n_leaves(), 1);
    assert!(proof.verify::<Blake3Hasher>(root(&store)));
    // The proof contains the existing leaf, not the absent key.
    assert_eq!(proof.leaf_key(0), &key);
    assert_eq!(proof.leaf_value_hash(0), &vh);
}

#[test]
fn proof_compute_root_with_update() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"old1").as_bytes();
    let vh2 = *blake3::hash(b"old2").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2)]);
    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&[k1, k2]);

    // Compute new root with updated value hashes.
    let new_vh1 = *blake3::hash(b"new1").as_bytes();
    let new_vh2 = *blake3::hash(b"new2").as_bytes();
    let computed_root = proof.compute_root::<Blake3Hasher>(&[new_vh1, new_vh2]);

    // Actually update the tree and verify the computed root matches.
    update(&store, 2, &[(k1, new_vh1), (k2, new_vh2)]);
    assert_eq!(computed_root, root(&store));
}

#[test]
fn proof_three_keys() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;
    let mut k3 = [0u8; 32];
    k3[0] = 0x40;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&[k1, k2, k3]);
    assert_eq!(proof.n_leaves(), 3);
    assert!(proof.verify::<Blake3Hasher>(root(&store)));
}

#[test]
fn proof_subset_of_keys() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;
    let mut k3 = [0u8; 32];
    k3[0] = 0x40;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    update(&store, 1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    // Prove only k1 — k2 and k3 subtree hashes appear as siblings.
    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&[k1]);
    assert_eq!(proof.n_leaves(), 1);
    assert!(proof.verify::<Blake3Hasher>(root(&store)));
}

// --- Stress / property tests ---

#[test]
fn many_random_keys() {
    let (store, _dir) = setup();
    let mut rng_state = 12345u64;

    // Generate 100 random key-value pairs using a simple LCG.
    let mut keys_and_values = Vec::new();
    for _ in 0..100 {
        let mut key = [0u8; 32];
        for b in &mut key {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *b = (rng_state >> 33) as u8;
        }
        let mut vh = [0u8; 32];
        for b in &mut vh {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *b = (rng_state >> 33) as u8;
        }
        keys_and_values.push((key, vh));
    }

    update(&store, 1, &keys_and_values);

    // Verify proof for all keys.
    let all_keys: Vec<[u8; 32]> = keys_and_values.iter().map(|(k, _)| *k).collect();
    let tree = tree_at(&store, 1);
    let proof = tree.multi_proof(&all_keys);
    assert_eq!(proof.n_leaves(), all_keys.len());
    assert!(proof.verify::<Blake3Hasher>(root(&store)));
}

#[test]
fn versioned_proofs() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();

    update(&store, 1, &[(k1, vh1)]);
    let root_v1 = root(&store);
    let tree = tree_at(&store, 1);
    let proof_v1 = tree.multi_proof(&[k1]);

    update(&store, 2, &[(k2, vh2)]);
    let root_v2 = root(&store);
    let tree = tree_at(&store, 2);
    let proof_v2 = tree.multi_proof(&[k1, k2]);

    // Both proofs should verify against their respective roots.
    assert!(proof_v1.verify::<Blake3Hasher>(root_v1));
    assert!(proof_v2.verify::<Blake3Hasher>(root_v2));

    // Rollback and verify root is restored.
    rollback(&store, 1);
    assert_eq!(root(&store), root_v1);
}

#[test]
fn proof_at_old_version() {
    let (store, _dir) = setup();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();
    let vh3 = *blake3::hash(b"v3").as_bytes();

    update(&store, 1, &[(k1, vh1)]);
    let root_v1 = root(&store);

    update(&store, 2, &[(k2, vh2)]);
    let root_v2 = root(&store);

    update(&store, 3, &[(k1, vh3)]);

    // Generate proof at version 1 without rolling back.
    let tree = tree_at(&store, 3);
    let proof = tree.multi_proof_at_version(&[k1], 1);
    assert!(proof.verify::<Blake3Hasher>(root_v1));

    // Generate proof at version 2 without rolling back.
    let proof = tree.multi_proof_at_version(&[k1, k2], 2);
    assert!(proof.verify::<Blake3Hasher>(root_v2));
}
