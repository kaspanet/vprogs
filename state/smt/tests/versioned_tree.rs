use vprogs_state_smt::{
    Blake3Hasher, EMPTY_HASH, Hasher,
    versioned::{InMemoryStore, VersionedTree},
};

type Tree = VersionedTree<Blake3Hasher, InMemoryStore>;

fn new_tree() -> Tree {
    VersionedTree::new(InMemoryStore::new())
}

// --- Tree update tests ---

#[test]
fn empty_tree_has_empty_root() {
    let tree = new_tree();
    assert_eq!(tree.root(), EMPTY_HASH);
}

#[test]
fn single_insert_creates_shortcut_at_root() {
    let mut tree = new_tree();
    let key = [0xABu8; 32];
    let vh = *blake3::hash(b"hello").as_bytes();

    tree.update(1, &[(key, vh)]);

    // Root should be hash_leaf(key, vh) since the single leaf shortcuts to root.
    let expected = Blake3Hasher::hash_leaf(&key, &vh);
    assert_eq!(tree.root(), expected);
}

#[test]
fn two_inserts_create_internal_at_divergence() {
    let mut tree = new_tree();

    let k1 = [0u8; 32]; // bit 0 = 0
    let mut k2 = [0u8; 32];
    k2[0] = 0x80; // bit 0 = 1

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2)]);

    // Root should be hash_internal(hash_leaf(k1, vh1), hash_leaf(k2, vh2)).
    let lh1 = Blake3Hasher::hash_leaf(&k1, &vh1);
    let lh2 = Blake3Hasher::hash_leaf(&k2, &vh2);
    let expected = Blake3Hasher::hash_internal(&lh1, &lh2);
    assert_eq!(tree.root(), expected);
}

#[test]
fn delete_collapses_back_to_shortcut() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2)]);

    // Delete k1 — should collapse back to a single shortcut leaf for k2.
    tree.update(2, &[(k1, EMPTY_HASH)]);

    let expected = Blake3Hasher::hash_leaf(&k2, &vh2);
    assert_eq!(tree.root(), expected);
}

#[test]
fn delete_all_returns_empty() {
    let mut tree = new_tree();
    let key = [1u8; 32];
    let vh = *blake3::hash(b"data").as_bytes();

    tree.update(1, &[(key, vh)]);
    tree.update(2, &[(key, EMPTY_HASH)]);

    assert_eq!(tree.root(), EMPTY_HASH);
}

#[test]
fn update_existing_key() {
    let mut tree = new_tree();
    let key = [0u8; 32];
    let vh1 = *blake3::hash(b"old").as_bytes();
    let vh2 = *blake3::hash(b"new").as_bytes();

    tree.update(1, &[(key, vh1)]);
    tree.update(2, &[(key, vh2)]);

    let expected = Blake3Hasher::hash_leaf(&key, &vh2);
    assert_eq!(tree.root(), expected);
}

#[test]
fn three_keys_different_depths() {
    let mut tree = new_tree();

    // k1 and k2 diverge at bit 0, k3 shares prefix with k1 but diverges at bit 1.
    let k1 = [0u8; 32]; // 0b00...
    let mut k2 = [0u8; 32];
    k2[0] = 0x80; // 0b10...
    let mut k3 = [0u8; 32];
    k3[0] = 0x40; // 0b01...

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

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
    assert_eq!(tree.root(), expected);
}

#[test]
fn delete_causes_collapse_of_internal() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;
    let mut k3 = [0u8; 32];
    k3[0] = 0x40;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    // Delete k3 — left subtree should collapse from Internal to Leaf(k1).
    tree.update(2, &[(k3, EMPTY_HASH)]);

    let lh1 = Blake3Hasher::hash_leaf(&k1, &vh1);
    let lh2 = Blake3Hasher::hash_leaf(&k2, &vh2);
    let expected = Blake3Hasher::hash_internal(&lh1, &lh2);
    assert_eq!(tree.root(), expected);
}

#[test]
fn rollback_restores_previous_root() {
    let mut tree = new_tree();

    let key = [0u8; 32];
    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();

    tree.update(1, &[(key, vh1)]);
    let root_v1 = tree.root();

    tree.update(2, &[(key, vh2)]);
    assert_ne!(tree.root(), root_v1);

    tree.rollback_to(1);
    assert_eq!(tree.root(), root_v1);
}

#[test]
fn prune_removes_old_versions() {
    let mut tree = new_tree();

    let key = [0u8; 32];
    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();

    tree.update(1, &[(key, vh1)]);
    tree.update(2, &[(key, vh2)]);

    let root_v2 = tree.root();
    tree.prune(2);

    assert_eq!(tree.root(), root_v2);

    // Rollback after pruning — v1 nodes are gone, so root resolves to EMPTY_HASH.
    tree.rollback_to(1);
    assert_eq!(tree.root(), EMPTY_HASH);
}

#[test]
fn incremental_inserts_match_batch() {
    let mut tree_incremental = new_tree();
    let mut tree_batch = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();

    // Incremental: insert one at a time.
    tree_incremental.update(1, &[(k1, vh1)]);
    tree_incremental.update(2, &[(k2, vh2)]);

    // Batch: insert both at once.
    tree_batch.update(1, &[(k1, vh1), (k2, vh2)]);

    assert_eq!(tree_incremental.root(), tree_batch.root());
}

// --- Proof generation + verification tests ---

#[test]
fn proof_single_key() {
    let mut tree = new_tree();
    let key = [0xABu8; 32];
    let vh = *blake3::hash(b"hello").as_bytes();

    tree.update(1, &[(key, vh)]);

    let proof = tree.multi_proof(&[key]);
    assert_eq!(proof.n_leaves(), 1);
    assert!(proof.verify::<Blake3Hasher>(tree.root()));
}

#[test]
fn proof_two_keys() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"left").as_bytes();
    let vh2 = *blake3::hash(b"right").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2)]);

    let proof = tree.multi_proof(&[k1, k2]);
    assert_eq!(proof.n_leaves(), 2);
    assert!(proof.verify::<Blake3Hasher>(tree.root()));
}

#[test]
fn proof_absent_key() {
    let mut tree = new_tree();
    let key = [0u8; 32];
    let vh = *blake3::hash(b"data").as_bytes();

    tree.update(1, &[(key, vh)]);

    // Prove a key that's not in the tree. The proof includes the shortcut leaf that
    // occupies the position — callers detect non-existence by checking key mismatch.
    let absent = [0xFFu8; 32];
    let proof = tree.multi_proof(&[absent]);

    assert_eq!(proof.n_leaves(), 1);
    assert!(proof.verify::<Blake3Hasher>(tree.root()));
    // The proof contains the existing leaf, not the absent key.
    assert_eq!(proof.leaf_key(0), &key);
    assert_eq!(proof.leaf_value_hash(0), &vh);
}

#[test]
fn proof_compute_root_with_update() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"old1").as_bytes();
    let vh2 = *blake3::hash(b"old2").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2)]);
    let proof = tree.multi_proof(&[k1, k2]);

    // Compute new root with updated value hashes.
    let new_vh1 = *blake3::hash(b"new1").as_bytes();
    let new_vh2 = *blake3::hash(b"new2").as_bytes();
    let computed_root = proof.compute_root::<Blake3Hasher>(&[new_vh1, new_vh2]);

    // Actually update the tree and verify the computed root matches.
    tree.update(2, &[(k1, new_vh1), (k2, new_vh2)]);
    assert_eq!(computed_root, tree.root());
}

#[test]
fn proof_three_keys() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;
    let mut k3 = [0u8; 32];
    k3[0] = 0x40;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    let proof = tree.multi_proof(&[k1, k2, k3]);
    assert_eq!(proof.n_leaves(), 3);
    assert!(proof.verify::<Blake3Hasher>(tree.root()));
}

#[test]
fn proof_subset_of_keys() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;
    let mut k3 = [0u8; 32];
    k3[0] = 0x40;

    let vh1 = *blake3::hash(b"a").as_bytes();
    let vh2 = *blake3::hash(b"b").as_bytes();
    let vh3 = *blake3::hash(b"c").as_bytes();

    tree.update(1, &[(k1, vh1), (k2, vh2), (k3, vh3)]);

    // Prove only k1 — k2 and k3 subtree hashes appear as siblings.
    let proof = tree.multi_proof(&[k1]);
    assert_eq!(proof.n_leaves(), 1);
    assert!(proof.verify::<Blake3Hasher>(tree.root()));
}

// --- Stress / property tests ---

#[test]
fn many_random_keys() {
    let mut tree = new_tree();
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

    tree.update(1, &keys_and_values);

    // Verify proof for all keys.
    let all_keys: Vec<[u8; 32]> = keys_and_values.iter().map(|(k, _)| *k).collect();
    let proof = tree.multi_proof(&all_keys);
    assert_eq!(proof.n_leaves(), all_keys.len());
    assert!(proof.verify::<Blake3Hasher>(tree.root()));
}

#[test]
fn versioned_proofs() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();

    tree.update(1, &[(k1, vh1)]);
    let root_v1 = tree.root();
    let proof_v1 = tree.multi_proof(&[k1]);

    tree.update(2, &[(k2, vh2)]);
    let root_v2 = tree.root();
    let proof_v2 = tree.multi_proof(&[k1, k2]);

    // Both proofs should verify against their respective roots.
    assert!(proof_v1.verify::<Blake3Hasher>(root_v1));
    assert!(proof_v2.verify::<Blake3Hasher>(root_v2));

    // Rollback and verify root is restored.
    tree.rollback_to(1);
    assert_eq!(tree.root(), root_v1);
}

#[test]
fn proof_at_old_version() {
    let mut tree = new_tree();

    let k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    k2[0] = 0x80;

    let vh1 = *blake3::hash(b"v1").as_bytes();
    let vh2 = *blake3::hash(b"v2").as_bytes();
    let vh3 = *blake3::hash(b"v3").as_bytes();

    tree.update(1, &[(k1, vh1)]);
    let root_v1 = tree.root();

    tree.update(2, &[(k2, vh2)]);
    let root_v2 = tree.root();

    tree.update(3, &[(k1, vh3)]);

    // Generate proof at version 1 without rolling back.
    let proof = tree.multi_proof_at_version(&[k1], 1);
    assert!(proof.verify::<Blake3Hasher>(root_v1));

    // Generate proof at version 2 without rolling back.
    let proof = tree.multi_proof_at_version(&[k1, k2], 2);
    assert!(proof.verify::<Blake3Hasher>(root_v2));
}
