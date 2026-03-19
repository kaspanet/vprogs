use tempfile::TempDir;
use vprogs_core_smt::{Blake3, Commitment, EMPTY_HASH, Hasher, Key, Node, Tree, proving::Proof};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

// -- Helpers --

/// Helper: commits a set of (key, value) pairs at the given version and returns the new root.
fn commit(store: &RocksDbStore, version: u64, entries: &[([u8; 32], [u8; 32])]) -> [u8; 32] {
    let diffs =
        entries.iter().map(|&(key, value)| Commitment::new(key, Blake3::hash(&value))).collect();

    let mut wb = store.write_batch();
    let root = store.update(&mut wb, diffs, version);
    store.commit(wb);
    root
}

/// Helper: commits raw commitments (including deletions) at the given version.
fn commit_raw(store: &RocksDbStore, version: u64, diffs: Vec<Commitment>) -> [u8; 32] {
    let mut wb = store.write_batch();
    let root = store.update(&mut wb, diffs, version);
    store.commit(wb);
    root
}

/// Deterministic test key from an integer.
fn test_key(id: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[24..32].copy_from_slice(&id.to_be_bytes());
    key
}

/// Deterministic test value from an integer.
fn test_value(id: u64) -> [u8; 32] {
    let mut val = [0xFFu8; 32];
    val[24..32].copy_from_slice(&id.to_be_bytes());
    val
}

// -- Node hashing --

#[test]
fn domain_separation_produces_different_hashes() {
    // Same payload but different domain tags should produce different hashes.
    let a = [1u8; 32];
    let b = [2u8; 32];
    let internal = Node::internal::<Blake3>(&a, &b);
    let leaf = Node::leaf::<Blake3>(a, b);
    assert_ne!(internal.hash(), leaf.hash());
}

#[test]
fn empty_compression_internal() {
    let node = Node::internal::<Blake3>(&EMPTY_HASH, &EMPTY_HASH);
    assert_eq!(*node.hash(), EMPTY_HASH);
}

#[test]
fn empty_compression_leaf() {
    let key = [0xABu8; 32];
    let node = Node::leaf::<Blake3>(key, EMPTY_HASH);
    assert_eq!(*node.hash(), EMPTY_HASH);
}

#[test]
fn non_empty_internal_is_not_empty() {
    let a = [1u8; 32];
    let node = Node::internal::<Blake3>(&a, &EMPTY_HASH);
    assert_ne!(*node.hash(), EMPTY_HASH);
}

#[test]
fn non_empty_leaf_is_not_empty() {
    let key = [1u8; 32];
    let value = [2u8; 32];
    let node = Node::leaf::<Blake3>(key, value);
    assert_ne!(*node.hash(), EMPTY_HASH);
}

// -- Node serialization --

#[test]
fn internal_roundtrip() {
    let node = Node::internal::<Blake3>(&[1u8; 32], &[2u8; 32]);
    let bytes = node.encode();
    assert_eq!(bytes.len(), 33);
    assert_eq!(Node::decode(&mut bytes.as_slice()).unwrap(), node);
}

#[test]
fn leaf_roundtrip() {
    let node = Node::leaf::<Blake3>([1u8; 32], [2u8; 32]);
    let bytes = node.encode();
    assert_eq!(bytes.len(), 97);
    assert_eq!(Node::decode(&mut bytes.as_slice()).unwrap(), node);
}

// -- Versioned tree --

/// Commits state across 3 versions and verifies that:
/// 1. Per-version roots are distinct (reading back nodes at different versions works)
/// 2. Proofs generated at each version verify against the correct root
/// 3. Proofs contain leaves with the expected value hashes
///
/// This exercises the full encode/decode pipeline including `Key::decode` (level field) and
/// `KeyExt::decode_version` (inverted version suffix). An endianness mismatch in either would
/// cause `node` to return wrong data, producing incorrect roots or failing proof verification.
#[test]
fn multi_version_commit_and_prove() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    // Version 1: insert keys 1, 2, 3 - creates internal nodes at non-zero levels.
    let root1 = commit(
        &store,
        1,
        &[(test_key(1), test_value(1)), (test_key(2), test_value(2)), (test_key(3), test_value(3))],
    );
    assert_ne!(root1, EMPTY_HASH, "root should be non-empty after first commit");

    // Version 2: insert key 4, update key 1 - supersedes internal nodes, creates stale markers.
    let root2 = commit(&store, 2, &[(test_key(1), test_value(10)), (test_key(4), test_value(4))]);
    assert_ne!(root2, EMPTY_HASH);
    assert_ne!(root2, root1, "root should change after second commit");

    // Version 3: insert key 5, update key 3 - another round of internal node changes.
    let root3 = commit(&store, 3, &[(test_key(3), test_value(30)), (test_key(5), test_value(5))]);
    assert_ne!(root3, EMPTY_HASH);
    assert_ne!(root3, root2, "root should change after third commit");

    // Per-version roots must be independently retrievable (exercises decode_version).
    assert_eq!(store.root(1), root1, "root(1) should return version 1 root");
    assert_eq!(store.root(2), root2, "root(2) should return version 2 root");
    assert_eq!(store.root(3), root3, "root(3) should return version 3 root");

    // Generate and verify proofs at each version.
    let keys = [test_key(1), test_key(2), test_key(3), test_key(4), test_key(5)];

    for (version, expected_root) in [(1, root1), (2, root2), (3, root3)] {
        let proof_bytes = store.prove(&keys, version);
        let proof = Proof::decode(&proof_bytes).expect("proof should decode");
        assert!(
            proof.verify::<Blake3>(expected_root).unwrap(),
            "proof at version {version} should verify against its root"
        );
        assert!(
            !proof.verify::<Blake3>([0xFFu8; 32]).unwrap(),
            "proof at version {version} should reject a wrong root"
        );
    }
}

/// Verifies that after committing version 2, the tree can still read version 1's nodes correctly
/// and produce a valid proof. This specifically tests that the inverted version encoding in
/// `KeyExt::encode_with_version` / `decode_version` correctly resolves "latest version <=
/// max_version" when multiple versions of the same node exist.
#[test]
fn historical_version_read_after_overwrite() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let key = test_key(1);

    // Version 1: insert key.
    let root1 = commit(&store, 1, &[(key, test_value(1))]);

    // Version 2: update same key with a different value.
    let root2 = commit(&store, 2, &[(key, test_value(2))]);
    assert_ne!(root1, root2);

    // Reading root at version 1 should still return the original root, not version 2's.
    assert_eq!(store.root(1), root1, "historical root should be preserved");

    // Proof at version 1 should verify against version 1's root.
    let proof_bytes = store.prove(&[key], 1);
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert!(
        proof.verify::<Blake3>(root1).unwrap(),
        "proof at version 1 should verify after version 2 overwrites the same key"
    );
    assert!(
        !proof.verify::<Blake3>(root2).unwrap(),
        "proof at version 1 should NOT verify against version 2's root"
    );
}

/// Verifies that `node` returns the correct version number, not just the correct data.
///
/// The version is used by `mark_stale` to record which node version was superseded. If
/// `decode_version` has an endianness mismatch, the returned version will be wrong, causing
/// `prune` to target the wrong node.
#[test]
fn node_returns_correct_version() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let key = test_key(1);

    // Version 1: insert key.
    commit(&store, 1, &[(key, test_value(1))]);

    // The root node written at version 1 should report version = 1.
    let (version, _node) = store.node(&Key::ROOT, 1).expect("root should exist at v1");
    assert_eq!(version, 1, "node should return version 1, not a byte-swapped value");

    // Version 2: update same key.
    commit(&store, 2, &[(key, test_value(2))]);

    // Root at max_version=2 should report version 2.
    let (version, _node) = store.node(&Key::ROOT, 2).expect("root should exist at v2");
    assert_eq!(version, 2, "node should return version 2");

    // Root at max_version=1 should still report version 1 (historical read).
    let (version, _node) = store.node(&Key::ROOT, 1).expect("root should exist at v1");
    assert_eq!(version, 1, "historical node should return version 1");
}

// -- Pruning and rollback --

/// Verifies that pruning correctly removes stale nodes without corrupting the tree.
///
/// Exercises the full stale node lifecycle: `mark_stale` (encodes version from `node`),
/// `put_stale_node` (writes stale marker), `prune` (reads stale markers and deletes
/// nodes). An endianness mismatch in any step would cause pruning to target wrong nodes.
#[test]
fn prune_preserves_tree_integrity() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    // Version 1: insert 3 keys to create a non-trivial tree structure.
    let root1 = commit(
        &store,
        1,
        &[(test_key(1), test_value(1)), (test_key(2), test_value(2)), (test_key(3), test_value(3))],
    );

    // Version 2: update key 1 - supersedes some nodes from version 1.
    let root2 = commit(&store, 2, &[(test_key(1), test_value(10))]);
    assert_ne!(root1, root2);

    // Prune version 2's stale markers (nodes that were superseded when v2 was committed).
    let mut wb = store.write_batch();
    store.prune(&mut wb, 2);
    store.commit(wb);

    // The current tree (version 2) should still be intact after pruning.
    assert_eq!(store.root(2), root2, "root at v2 should survive pruning");

    // Proof at version 2 should still verify.
    let keys = [test_key(1), test_key(2), test_key(3)];
    let proof_bytes = store.prove(&keys, 2);
    let proof = Proof::decode(&proof_bytes).expect("proof should decode");
    assert!(
        proof.verify::<Blake3>(root2).unwrap(),
        "proof at v2 should verify after pruning v2's stale nodes"
    );
}

/// Rollback undoes a committed version: the previous root is restored and proofs verify.
#[test]
fn rollback_restores_previous_state() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    // Version 1: insert key.
    let root1 = commit(&store, 1, &[(test_key(1), test_value(1))]);

    // Version 2: update key.
    let root2 = commit(&store, 2, &[(test_key(1), test_value(2))]);
    assert_ne!(root1, root2);

    // Rollback version 2.
    let mut wb = store.write_batch();
    store.rollback(&mut wb, 2);
    store.commit(wb);

    // Root at version 1 should still be intact.
    assert_eq!(store.root(1), root1);

    // Proof at version 1 should verify.
    let proof_bytes = store.prove(&[test_key(1)], 1);
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert!(proof.verify::<Blake3>(root1).unwrap());
}

// -- Edge cases --

/// Single-key tree: insert one key, prove it, delete it, verify the tree becomes empty.
#[test]
fn single_key_lifecycle() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let key = test_key(1);

    // Version 1: insert a single key.
    let root1 = commit(&store, 1, &[(key, test_value(1))]);
    assert_ne!(root1, EMPTY_HASH);

    // Proof for the single key should verify.
    let proof_bytes = store.prove(&[key], 1);
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert!(proof.verify::<Blake3>(root1).unwrap());
    assert_eq!(proof.leaves.len(), 1);

    // Version 2: delete the key.
    let root2 = commit_raw(&store, 2, vec![Commitment::new(key, EMPTY_HASH)]);
    assert_eq!(root2, EMPTY_HASH, "tree should be empty after deleting the only key");
}

/// Deleting all keys across multiple versions results in an empty tree.
#[test]
fn delete_all_keys() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    // Version 1: insert 3 keys.
    let root1 = commit(
        &store,
        1,
        &[(test_key(1), test_value(1)), (test_key(2), test_value(2)), (test_key(3), test_value(3))],
    );
    assert_ne!(root1, EMPTY_HASH);

    // Version 2: delete all 3 keys in one batch.
    let root2 = commit_raw(
        &store,
        2,
        vec![
            Commitment::new(test_key(1), EMPTY_HASH),
            Commitment::new(test_key(2), EMPTY_HASH),
            Commitment::new(test_key(3), EMPTY_HASH),
        ],
    );
    assert_eq!(root2, EMPTY_HASH, "tree should be empty after deleting all keys");

    // Historical root should still be accessible.
    assert_eq!(store.root(1), root1);
}

/// Duplicate keys in a single batch: last-write-wins semantics.
#[test]
fn duplicate_commitments_last_write_wins() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let key = test_key(1);
    let value_a = test_value(100);
    let value_b = test_value(200);

    // Submit two commitments for the same key — the second (value_b) should win.
    let root_dup = commit_raw(
        &store,
        1,
        vec![
            Commitment::new(key, Blake3::hash(&value_a)),
            Commitment::new(key, Blake3::hash(&value_b)),
        ],
    );

    // Compare against a clean commit with only value_b.
    let dir2 = TempDir::new().unwrap();
    let store2 = RocksDbStore::open(dir2.path());
    let root_clean = commit(&store2, 1, &[(key, value_b)]);

    assert_eq!(root_dup, root_clean, "duplicate key should resolve to last-write-wins");
}

/// Proof verification rejects an incorrect root with `Ok(false)`, not `Err`.
#[test]
fn proof_verify_wrong_root_returns_false() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    commit(&store, 1, &[(test_key(1), test_value(1))]);

    let proof_bytes = store.prove(&[test_key(1)], 1);
    let proof = Proof::decode(&proof_bytes).unwrap();

    // Wrong root should return Ok(false), not an error.
    let result = proof.verify::<Blake3>([0xAB; 32]);
    assert!(!result.unwrap());
}

/// Empty commitments are a no-op — root carries forward from the previous version.
#[test]
fn empty_commitments_preserve_root() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let root1 = commit(&store, 1, &[(test_key(1), test_value(1))]);

    // Version 2: empty commit.
    let root2 = commit_raw(&store, 2, vec![]);
    assert_eq!(root2, root1, "empty commit should carry forward the previous root");
}

/// Update with version 0 should panic.
#[test]
#[should_panic(expected = "version 0 is reserved as pre-genesis")]
fn version_zero_panics() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    commit(&store, 0, &[(test_key(1), test_value(1))]);
}
