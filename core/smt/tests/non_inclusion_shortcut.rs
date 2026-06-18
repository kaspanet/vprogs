//! Regression tests for SMT non-inclusion proofs: shortcut-leaf and empty-subtree witnesses,
//! multi-key deduplication of shared witnesses, and `subtree_hash` post-state synthesis.

use tempfile::TempDir;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{
    Commitment, EMPTY_HASH, Tree,
    proving::{Membership, Proof},
};
use vprogs_core_types::{NoOpCanonicalChain, ResourceId};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;
use zerocopy::little_endian::U32;

/// Deterministic test key from an integer: all-zero except the trailing big-endian `u64`.
fn test_key(id: u64) -> ResourceId {
    let mut key = [0u8; 32];
    key[24..32].copy_from_slice(&id.to_be_bytes());
    ResourceId::from(key)
}

/// Inserts one key at v=1 and returns an opened store. The single key forms a root-level shortcut
/// leaf, so any other key resolves into its subtree.
fn store_with_one_key(key: ResourceId, value_hash: [u8; 32]) -> (TempDir, RocksDbStore) {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(key, value_hash)],
        1,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);
    (dir, store)
}

/// Shortcut-leaf non-inclusion: the witness leaf is the *existing* key, but `member` reports the
/// queried (different) key as absent, and the proof still recomputes the stored root.
#[test]
fn shortcut_leaf_non_inclusion_resolves_to_absent() {
    let key_present = test_key(1);
    let value_hash_present = [0xAB; 32];
    let (_dir, store) = store_with_one_key(key_present, value_hash_present);

    // `for_test(1)` and `for_test(2)` share their first 31 bytes, so the absent key lands in the
    // existing key's root-level shortcut subtree.
    let key_absent = test_key(2);
    let proof_bytes = store.prove(&[key_absent], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    // One query, one witness leaf - but the leaf is the existing key, not the query. The keys
    // table holds both: the queried key at index 0 and the foreign witness at index 1.
    assert_eq!(proof.leaves.len(), 1);
    assert_eq!(proof.memberships.len(), 1);
    assert_eq!(proof.memberships[0], Membership::Absent(U32::new(0)));
    assert_eq!(proof.keys[0], *key_absent);
    let witness_key_idx = proof.leaves[0].key_idx.get() as usize;
    assert_eq!(proof.keys[witness_key_idx], *key_present);

    // The pair primitive reports the queried key as absent via the foreign witness leaf.
    let m = proof.member(0).unwrap();
    assert!(m.absent);
    assert_eq!(m.key, &*key_absent);

    // Still a sound proof: it recomputes the stored root from the existing leaf.
    assert_eq!(proof.root::<Sha256>().unwrap(), store.root(1, &NoOpCanonicalChain));
}

/// Multiple absent keys sharing one shortcut leaf - the case that previously broke the
/// one-leaf-per-query invariant. The proof emits a single witness leaf, and every absent query
/// points at it via its `Membership::Absent` entry.
#[test]
fn multiple_absent_keys_share_one_shortcut_leaf() {
    let (_dir, store) = store_with_one_key(test_key(1), [0xAB; 32]);

    let absent_a = test_key(2);
    let absent_b = test_key(3);
    let proof_bytes = store.prove(&[absent_a, absent_b], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    // One witness leaf, but two memberships - both Absent, both pointing at the same shortcut.
    assert_eq!(proof.leaves.len(), 1, "one shared witness leaf");
    assert_eq!(proof.memberships.len(), 2, "one entry per queried key");
    assert_eq!(proof.memberships[0], Membership::Absent(U32::new(0)));
    assert_eq!(proof.memberships[1], Membership::Absent(U32::new(0)));

    // Keys table: queried keys first (in caller order), foreign witness key past them.
    assert_eq!(proof.keys[0], *absent_a);
    assert_eq!(proof.keys[1], *absent_b);

    assert!(proof.member(0).unwrap().absent);
    assert!(proof.member(1).unwrap().absent);
    assert_eq!(proof.root::<Sha256>().unwrap(), store.root(1, &NoOpCanonicalChain));
}

/// Inclusion: querying the existing key resolves to a present member carrying its stored value.
#[test]
fn inclusion_resolves_to_present() {
    let key_present = test_key(1);
    let value_hash_present = [0xAB; 32];
    let (_dir, store) = store_with_one_key(key_present, value_hash_present);

    let proof_bytes = store.prove(&[key_present], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    assert!(matches!(proof.memberships[0], Membership::Present(_)));
    let m = proof.member(0).unwrap();
    assert!(!m.absent);
    assert_eq!(m.key, &*key_present);
    assert_eq!(m.leaf.value_hash, value_hash_present);
}

/// Empty-subtree non-inclusion: the queried key's subtree has no sibling leaf, so the proof
/// carries the key's *own* (empty) leaf. `member` returns `absent = false` with `leaf.value_hash
/// == EMPTY_HASH`; consumers apply the empty-as-absent semantic themselves.
#[test]
fn empty_subtree_non_inclusion_resolves_to_present_empty() {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());

    // Two keys that both have msb=1 but differ at bit 1: the root becomes an internal node with
    // an empty left (msb=0) child and a non-empty right (msb=1) child.
    let mut bytes_a = [0u8; 32];
    bytes_a[0] = 0x80;
    let mut bytes_b = [0u8; 32];
    bytes_b[0] = 0xC0;
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![
            Commitment::new(ResourceId::from(bytes_a), [0xCD; 32]),
            Commitment::new(ResourceId::from(bytes_b), [0xEF; 32]),
        ],
        1,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);

    // Query a key with msb=0: it lands on the empty left child of the root.
    let key_absent = test_key(1);
    let proof_bytes = store.prove(&[key_absent], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    let m = proof.member(0).unwrap();
    assert!(!m.absent, "empty-subtree absent should produce a present member with the queried key");
    assert_eq!(m.key, &*key_absent, "queried key is preserved");
    assert_eq!(m.leaf.value_hash, EMPTY_HASH);
}

/// Foreign-create: `new_root` over a proof covering an existing shortcut key and a new key in the
/// same subtree reproduces the SMT's post-state root after the write.
#[test]
fn new_root_matches_post_state_after_foreign_create() {
    let k_prime = test_key(1);
    let v_prime = [0xAB; 32];
    let (_dir, store) = store_with_one_key(k_prime, v_prime);

    // Prove against the pre-state, covering both the existing key and the new key.
    let k_new = test_key(2);
    let v_new = [0xCD; 32];
    let proof_bytes = store.prove(&[k_prime, k_new], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    // Apply the foreign-create in the SMT and capture the resulting root.
    let mut wb = store.write_batch();
    store.update(&mut wb, vec![Commitment::new(k_new, v_new)], 2, [0u8; 32], &NoOpCanonicalChain);
    store.commit(wb);
    let smt_root = store.root(2, &NoOpCanonicalChain);

    // `new_root` over the proof, supplying each member's post-state value, must match.
    let computed = proof
        .new_root::<Sha256>(|i| match i {
            0 => &v_prime,
            1 => &v_new,
            _ => unreachable!(),
        })
        .unwrap();
    assert_eq!(computed, smt_root);
}
