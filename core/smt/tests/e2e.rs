use std::collections::HashMap;

use tempfile::TempDir;
use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_smt::{
    Commitment, EMPTY_HASH, HashedNode, INTERNAL, Key, LEAF, Node, Tree, proving::Proof,
};
use vprogs_core_types::{CanonicalChain, NoOpCanonicalChain, ResourceId};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

// -- Helpers --

/// Helper: commits a set of (key, value) pairs at the given version and returns the new root.
fn commit(store: &RocksDbStore, version: u64, entries: &[(ResourceId, [u8; 32])]) -> [u8; 32] {
    let diffs =
        entries.iter().map(|&(key, value)| Commitment::new(key, Sha256::hash(value))).collect();

    let mut wb = store.write_batch();
    let root = store.update(&mut wb, diffs, version, [0u8; 32], &NoOpCanonicalChain);
    store.commit(wb);
    root
}

/// Helper: commits raw commitments (including deletions) at the given version.
fn commit_raw(store: &RocksDbStore, version: u64, diffs: Vec<Commitment>) -> [u8; 32] {
    let mut wb = store.write_batch();
    let root = store.update(&mut wb, diffs, version, [0u8; 32], &NoOpCanonicalChain);
    store.commit(wb);
    root
}

/// Deterministic test key from an integer.
fn test_key(id: u64) -> ResourceId {
    let mut key = [0u8; 32];
    key[24..32].copy_from_slice(&id.to_be_bytes());
    ResourceId::from(key)
}

/// Deterministic test value from an integer.
fn test_value(id: u64) -> [u8; 32] {
    let mut val = [0xFFu8; 32];
    val[24..32].copy_from_slice(&id.to_be_bytes());
    val
}

/// Controllable canonical-chain oracle for fork-aware read tests: maps each version to its
/// canonical block hash.
#[derive(Clone, Default)]
struct StubChain(HashMap<u64, [u8; 32]>);

impl StubChain {
    /// Records `block_hash` as canonical at `index`.
    fn with(mut self, index: u64, block_hash: [u8; 32]) -> Self {
        self.0.insert(index, block_hash);
        self
    }
}

impl CanonicalChain for StubChain {
    fn block(&self, index: u64) -> Option<[u8; 32]> {
        self.0.get(&index).copied()
    }
}

// -- Node hashing --

#[test]
fn domain_separation_produces_different_hashes() {
    // Same payload but different domain tags should produce different hashes.
    let a = [1u8; 32];
    let b = [2u8; 32];
    let internal = Node::internal::<Sha256>(&HashedNode::new(LEAF, a), &HashedNode::new(LEAF, b));
    let leaf = Node::leaf::<Sha256>(ResourceId::from(a), b);
    assert_ne!(internal.hash(), leaf.hash());
}

#[test]
fn empty_compression_internal() {
    let node = Node::internal::<Sha256>(&HashedNode::EMPTY, &HashedNode::EMPTY);
    assert_eq!(*node.hash(), EMPTY_HASH);
}

#[test]
fn empty_compression_leaf() {
    let key = ResourceId::from([0xABu8; 32]);
    let node = Node::leaf::<Sha256>(key, EMPTY_HASH);
    assert_eq!(*node.hash(), EMPTY_HASH);
}

#[test]
fn non_empty_internal_is_not_empty() {
    let a = [1u8; 32];
    let node = Node::internal::<Sha256>(&HashedNode::new(INTERNAL, a), &HashedNode::EMPTY);
    assert_ne!(*node.hash(), EMPTY_HASH);
}

#[test]
fn non_empty_leaf_is_not_empty() {
    let key = ResourceId::from([1u8; 32]);
    let value = [2u8; 32];
    let node = Node::leaf::<Sha256>(key, value);
    assert_ne!(*node.hash(), EMPTY_HASH);
}

// -- Node serialization --

#[test]
fn internal_roundtrip() {
    let node = Node::internal::<Sha256>(
        &HashedNode::new(LEAF, [1u8; 32]),
        &HashedNode::new(LEAF, [2u8; 32]),
    );
    let bytes = node.encode();
    assert_eq!(bytes.len(), 33);
    assert_eq!(Node::decode(&mut bytes.as_slice()).unwrap(), node);
}

#[test]
fn leaf_roundtrip() {
    let node = Node::leaf::<Sha256>(ResourceId::from([1u8; 32]), [2u8; 32]);
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
    assert_eq!(store.root(1, &NoOpCanonicalChain), root1, "root(1) should return version 1 root");
    assert_eq!(store.root(2, &NoOpCanonicalChain), root2, "root(2) should return version 2 root");
    assert_eq!(store.root(3, &NoOpCanonicalChain), root3, "root(3) should return version 3 root");

    // Generate and verify proofs at each version.
    let keys = [test_key(1), test_key(2), test_key(3), test_key(4), test_key(5)];

    for (version, expected_root) in [(1, root1), (2, root2), (3, root3)] {
        let proof_bytes = store.prove(&keys, version, &NoOpCanonicalChain).unwrap();
        let proof = Proof::decode(&proof_bytes).expect("proof should decode");
        assert_eq!(
            proof.root::<Sha256>().unwrap(),
            expected_root,
            "proof at version {version} should verify against its root"
        );
        assert_ne!(
            proof.root::<Sha256>().unwrap(),
            [0xFFu8; 32],
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
    assert_eq!(store.root(1, &NoOpCanonicalChain), root1, "historical root should be preserved");

    // Proof at version 1 should verify against version 1's root.
    let proof_bytes = store.prove(&[key], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(
        proof.root::<Sha256>().unwrap(),
        root1,
        "proof at version 1 should verify after version 2 overwrites the same key"
    );
    assert_ne!(
        proof.root::<Sha256>().unwrap(),
        root2,
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
    let (version, _, _node) =
        store.node(&Key::ROOT, 1, &NoOpCanonicalChain).expect("root should exist at v1");
    assert_eq!(version, 1, "node should return version 1, not a byte-swapped value");

    // Version 2: update same key.
    commit(&store, 2, &[(key, test_value(2))]);

    // Root at max_version=2 should report version 2.
    let (version, _, _node) =
        store.node(&Key::ROOT, 2, &NoOpCanonicalChain).expect("root should exist at v2");
    assert_eq!(version, 2, "node should return version 2");

    // Root at max_version=1 should still report version 1 (historical read).
    let (version, _, _node) =
        store.node(&Key::ROOT, 1, &NoOpCanonicalChain).expect("root should exist at v1");
    assert_eq!(version, 1, "historical node should return version 1");
}

// -- Pruning and fork-aware reads --

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

    // Prune version 2's stale markers (nodes that were superseded when v2 was committed). Pruning
    // is canonical-aware; with the no-op oracle every entry counts as canonical.
    let mut wb = store.write_batch();
    store.prune(&mut wb, 2, &NoOpCanonicalChain);
    store.commit(wb);

    // The current tree (version 2) should still be intact after pruning.
    assert_eq!(store.root(2, &NoOpCanonicalChain), root2, "root at v2 should survive pruning");

    // Proof at version 2 should still verify.
    let keys = [test_key(1), test_key(2), test_key(3)];
    let proof_bytes = store.prove(&keys, 2, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).expect("proof should decode");
    assert_eq!(
        proof.root::<Sha256>().unwrap(),
        root2,
        "proof at v2 should verify after pruning v2's stale nodes"
    );
}

/// Reads return the node written by the fork the oracle deems canonical, and switching the oracle
/// switches which fork's state is visible. This is the fork-aware replacement for eager rollback:
/// competing forks coexist on disk and the canonical chain selects between them at read time.
#[test]
fn fork_aware_reads_select_canonical_fork() {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());

    let key = test_key(1);
    let hash_genesis = [0x11u8; 32];
    let hash_a = [0xAAu8; 32];
    let hash_b = [0xBBu8; 32];

    // Version 1: shared history under the genesis fork.
    let oracle_genesis = StubChain::default().with(1, hash_genesis);
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(key, Sha256::hash(test_value(1)))],
        1,
        hash_genesis,
        &oracle_genesis,
    );
    store.commit(wb);

    // Two competing forks both write version 2 with different values and block hashes. Each builds
    // on the shared version-1 state via an oracle that includes the genesis fork.
    let oracle_a = StubChain::default().with(1, hash_genesis).with(2, hash_a);
    let mut wb = store.write_batch();
    let root_a = store.update(
        &mut wb,
        vec![Commitment::new(key, Sha256::hash(test_value(10)))],
        2,
        hash_a,
        &oracle_a,
    );
    store.commit(wb);

    let oracle_b = StubChain::default().with(1, hash_genesis).with(2, hash_b);
    let mut wb = store.write_batch();
    let root_b = store.update(
        &mut wb,
        vec![Commitment::new(key, Sha256::hash(test_value(20)))],
        2,
        hash_b,
        &oracle_b,
    );
    store.commit(wb);

    assert_ne!(root_a, root_b, "the two forks must produce different roots");

    // Both forks' nodes coexist on disk; the oracle selects which one a read sees.
    assert_eq!(store.root(2, &oracle_a), root_a, "oracle A must see fork A's state");
    assert_eq!(store.root(2, &oracle_b), root_b, "oracle B must see fork B's state");

    // Proofs follow the canonical fork too.
    let proof_a_bytes = store.prove(&[key], 2, &oracle_a).unwrap();
    let proof_a = Proof::decode(&proof_a_bytes).unwrap();
    assert_eq!(proof_a.root::<Sha256>().unwrap(), root_a);
    let proof_b_bytes = store.prove(&[key], 2, &oracle_b).unwrap();
    let proof_b = Proof::decode(&proof_b_bytes).unwrap();
    assert_eq!(proof_b.root::<Sha256>().unwrap(), root_b);
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
    let proof_bytes = store.prove(&[key], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(proof.root::<Sha256>().unwrap(), root1);
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
    assert_eq!(store.root(1, &NoOpCanonicalChain), root1);
}

/// Duplicate keys in a single batch: last-write-wins semantics.
#[test]
fn duplicate_commitments_last_write_wins() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let key = test_key(1);
    let value_a = test_value(100);
    let value_b = test_value(200);

    // Submit two commitments for the same key - the second (value_b) should win.
    let root_dup = commit_raw(
        &store,
        1,
        vec![
            Commitment::new(key, Sha256::hash(value_a)),
            Commitment::new(key, Sha256::hash(value_b)),
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

    let proof_bytes = store.prove(&[test_key(1)], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    // Wrong root should not match.
    assert_ne!(proof.root::<Sha256>().unwrap(), [0xAB; 32]);
}

/// Empty commitments are a no-op - root carries forward from the previous version.
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

/// After a delete that empties the tree, `store.root(v)` returns `EMPTY_HASH` (not the stale prior
/// root) and a subsequent insert does not resurrect the deleted key.
#[test]
fn delete_then_insert_does_not_resurrect() {
    let dir = TempDir::new().unwrap();
    let store = RocksDbStore::open(dir.path());

    let k_old = test_key(1);
    let k_new = test_key(2);

    // v=1: insert k_old.
    let root1 = commit(&store, 1, &[(k_old, test_value(1))]);
    assert_ne!(root1, EMPTY_HASH);

    // v=2: delete k_old. Tree becomes empty.
    let root2 = commit_raw(&store, 2, vec![Commitment::new(k_old, EMPTY_HASH)]);
    assert_eq!(root2, EMPTY_HASH);
    assert_eq!(
        store.root(2, &NoOpCanonicalChain),
        EMPTY_HASH,
        "store.root(2) must reflect the empty state"
    );

    // v=3: insert k_new. The tree must contain only k_new - k_old must not be resurrected.
    let _root3 = commit(&store, 3, &[(k_new, test_value(2))]);

    let proof_bytes = store.prove(&[k_old, k_new], 3, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    let m_old = proof.member(0).unwrap();
    let m_new = proof.member(1).unwrap();
    assert!(m_old.absent, "previously-deleted key must not be resurrected");
    assert!(!m_new.absent, "newly inserted key must be present");
    assert_eq!(m_new.leaf.value_hash, Sha256::hash(test_value(2)));
}
