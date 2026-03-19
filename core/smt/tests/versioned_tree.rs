use tempfile::TempDir;
use vprogs_core_smt::{Blake3Hasher, Commitment, EMPTY_HASH, Hasher, Key, Tree, proving::Proof};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// Helper: commits a set of (key, value) pairs at the given version and returns the new root.
fn commit(store: &RocksDbStore, version: u64, entries: &[([u8; 32], [u8; 32])]) -> [u8; 32] {
    let diffs: Vec<Commitment> = entries
        .iter()
        .map(|&(key, value)| Commitment { key, value_hash: Blake3Hasher::hash_leaf(&key, &value) })
        .collect();

    let mut wb = store.write_batch();
    let root = store.commit_diffs(&mut wb, version, &diffs);
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

/// Commits state across 3 versions and verifies that:
/// 1. Per-version roots are distinct (reading back nodes at different versions works)
/// 2. Proofs generated at each version verify against the correct root
/// 3. Proofs contain leaves with the expected value hashes
///
/// This exercises the full encode/decode pipeline including `Key::decode` (level field) and
/// `KeyExt::decode_version` (inverted version suffix). An endianness mismatch in either would
/// cause `get_node` to return wrong data, producing incorrect roots or failing proof verification.
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
    assert_eq!(store.get_root(1), root1, "get_root(1) should return version 1 root");
    assert_eq!(store.get_root(2), root2, "get_root(2) should return version 2 root");
    assert_eq!(store.get_root(3), root3, "get_root(3) should return version 3 root");

    // Generate and verify proofs at each version.
    let keys = [test_key(1), test_key(2), test_key(3), test_key(4), test_key(5)];

    for (version, expected_root) in [(1, root1), (2, root2), (3, root3)] {
        let proof_bytes = store.prove(&keys, version);
        let proof = Proof::decode(&proof_bytes).expect("proof should decode");
        assert!(
            proof.verify::<Blake3Hasher>(expected_root).unwrap(),
            "proof at version {version} should verify against its root"
        );
        assert!(
            !proof.verify::<Blake3Hasher>([0xFFu8; 32]).unwrap(),
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
    assert_eq!(store.get_root(1), root1, "historical root should be preserved");

    // Proof at version 1 should verify against version 1's root.
    let proof_bytes = store.prove(&[key], 1);
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert!(
        proof.verify::<Blake3Hasher>(root1).unwrap(),
        "proof at version 1 should verify after version 2 overwrites the same key"
    );
    assert!(
        !proof.verify::<Blake3Hasher>(root2).unwrap(),
        "proof at version 1 should NOT verify against version 2's root"
    );
}

/// Verifies that `get_node` returns the correct version number, not just the correct data.
///
/// The version is used by `mark_stale` to record which node version was superseded. If
/// `decode_version` has an endianness mismatch, the returned version will be wrong, causing
/// `prune` to target the wrong node.
#[test]
fn get_node_returns_correct_version() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let key = test_key(1);

    // Version 1: insert key.
    commit(&store, 1, &[(key, test_value(1))]);

    // The root node written at version 1 should report version = 1.
    let (version, _node) = store.get_node(&Key::root(), 1).expect("root should exist at v1");
    assert_eq!(version, 1, "get_node should return version 1, not a byte-swapped value");

    // Version 2: update same key.
    commit(&store, 2, &[(key, test_value(2))]);

    // Root at max_version=2 should report version 2.
    let (version, _node) = store.get_node(&Key::root(), 2).expect("root should exist at v2");
    assert_eq!(version, 2, "get_node should return version 2");

    // Root at max_version=1 should still report version 1 (historical read).
    let (version, _node) = store.get_node(&Key::root(), 1).expect("root should exist at v1");
    assert_eq!(version, 1, "historical get_node should return version 1");
}

/// Verifies that pruning correctly removes stale nodes without corrupting the tree.
///
/// Exercises the full stale node lifecycle: `mark_stale` (encodes version from `get_node`),
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
    assert_eq!(store.get_root(2), root2, "root at v2 should survive pruning");

    // Proof at version 2 should still verify.
    let keys = [test_key(1), test_key(2), test_key(3)];
    let proof_bytes = store.prove(&keys, 2);
    let proof = Proof::decode(&proof_bytes).expect("proof should decode");
    assert!(
        proof.verify::<Blake3Hasher>(root2).unwrap(),
        "proof at v2 should verify after pruning v2's stale nodes"
    );
}
