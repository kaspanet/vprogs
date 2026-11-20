//! A proof over no key still has to recompute to the tree's root.
//!
//! A proof carries the root only through its leaves: without one it recomputes to the empty
//! root whatever the tree holds. A caller that proves a state transition touching no key (a
//! batch whose transactions declare no resource) would then commit the empty root for a
//! non-empty tree. A proof over no key therefore carries the path to one leaf as a witness, and
//! no membership.

use tempfile::TempDir;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{Commitment, EMPTY_HASH, Tree, proving::Proof};
use vprogs_core_types::ResourceId;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// A key of all-`fill` bytes ending in `last`, so two keys with the same `fill` share their
/// first 31 bytes.
fn key(fill: u8, last: u8) -> ResourceId {
    let mut key = [fill; 32];
    key[31] = last;
    ResourceId::from(key)
}

/// Opens an empty store.
fn open() -> (TempDir, RocksDbStore) {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    (dir, store)
}

/// Commits `commitments` at `version`.
fn commit(store: &RocksDbStore, version: u64, commitments: Vec<Commitment>) {
    let mut wb = store.write_batch();
    store.update(&mut wb, commitments, version);
    store.commit(wb);
}

/// Sets `key` to a value hash derived from `value`.
fn set(key: ResourceId, value: u8) -> Commitment {
    Commitment::new(key, [value; 32])
}

/// Proves no key at `version` and checks that the proof answers no key and recomputes to the
/// store's root, as the pre-state and as an unchanged post-state. Returns the number of leaves
/// the proof carries.
fn assert_binds_root(store: &RocksDbStore, version: u64) -> usize {
    let proof_bytes = store.prove(&[], version).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    let root = store.root(version);

    assert!(proof.memberships.is_empty(), "a proof over no key answers no key");
    proof.check_sorted_queried_keys().unwrap();
    assert_eq!(proof.root::<Sha256>().unwrap(), root, "the proof recomputes to the root");
    assert_eq!(
        proof.compute_roots::<Sha256>(|_| unreachable!("the proof has no member")).unwrap(),
        (root, root),
        "with no member to change, both roots are the tree's",
    );

    proof.leaves.len()
}

/// A tree that never held a key has the empty root, which a proof without leaves recomputes to.
#[test]
fn an_empty_tree_needs_no_witness() {
    let (_dir, store) = open();
    assert_eq!(store.root(0), EMPTY_HASH);
    assert_eq!(assert_binds_root(&store, 0), 0);
}

/// The same for a tree drained by deletes, whose root position holds a tombstone.
#[test]
fn a_drained_tree_needs_no_witness() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0x00, 1), 0xA1), set(key(0xFF, 1), 0xA2)]);
    commit(
        &store,
        2,
        vec![Commitment::new(key(0x00, 1), EMPTY_HASH), Commitment::new(key(0xFF, 1), EMPTY_HASH)],
    );
    assert_eq!(store.root(2), EMPTY_HASH);
    assert_eq!(assert_binds_root(&store, 2), 0);

    // The version before the deletes still proves to its own root.
    assert_ne!(store.root(1), EMPTY_HASH);
    assert_eq!(assert_binds_root(&store, 1), 1);
}

/// A single key sits as a shortcut leaf at the root, which is the whole witness.
#[test]
fn a_root_leaf_is_its_own_witness() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0x5A, 1), 0xA1)]);

    let proof_bytes = store.prove(&[], 1).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(assert_binds_root(&store, 1), 1);
    assert_eq!(proof.leaves[0].depth.get(), 0, "the leaf sits at the root");
    assert!(proof.siblings.is_empty(), "a root leaf has no sibling");
}

/// Two keys splitting at the first bit: the witness is the left leaf, the right one its sibling.
#[test]
fn a_split_at_the_root_is_witnessed_by_one_side() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0x00, 1), 0xA1), set(key(0xFF, 1), 0xA2)]);

    let proof_bytes = store.prove(&[], 1).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(assert_binds_root(&store, 1), 1);
    assert_eq!(proof.keys[proof.leaves[0].key_idx.get() as usize], *key(0x00, 1));
    assert_eq!(proof.siblings.len(), 1);
}

/// Two keys sharing their first 254 bits on the left: every level above the split has an empty
/// right sibling, and the witness is a leaf at depth 255.
#[test]
fn a_deep_split_on_the_left_is_witnessed() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0x00, 1), 0xA1), set(key(0x00, 2), 0xA2)]);

    let proof_bytes = store.prove(&[], 1).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(assert_binds_root(&store, 1), 1);
    assert_eq!(proof.leaves[0].depth.get(), 255);
    assert_eq!(proof.siblings.len(), 255);
}

/// The same on the right: every level above the split has an empty left child, so the witness
/// path has to turn right at each of them.
#[test]
fn a_deep_split_on_the_right_is_witnessed() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0xFF, 0xFD), 0xA1), set(key(0xFF, 0xFE), 0xA2)]);

    let proof_bytes = store.prove(&[], 1).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(assert_binds_root(&store, 1), 1);
    assert_eq!(proof.leaves[0].depth.get(), 255);
    assert_eq!(proof.siblings.len(), 255);
}

/// Two keys that differ only in their last bit: the witness is a leaf at the full depth.
#[test]
fn a_split_at_the_last_bit_is_witnessed_at_full_depth() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0x00, 0), 0xA1), set(key(0x00, 1), 0xA2)]);

    let proof_bytes = store.prove(&[], 1).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(assert_binds_root(&store, 1), 1);
    assert_eq!(proof.leaves[0].depth.get(), 256);
    assert_eq!(proof.siblings.len(), 256);
}

/// Many keys over several versions, with overwrites and deletes: every version proves to its
/// own root.
#[test]
fn every_version_of_a_changing_tree_is_witnessed() {
    let (_dir, store) = open();

    // Twelve clusters spread over the top bits, each of four keys sharing their first 31 bytes.
    let keys: Vec<ResourceId> = (0u8..48)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[0] = (i / 4).wrapping_mul(151);
            bytes[31] = i;
            ResourceId::from(bytes)
        })
        .collect();

    commit(&store, 1, keys[..16].iter().map(|&k| set(k, 0xA1)).collect());
    commit(&store, 2, keys[16..].iter().map(|&k| set(k, 0xA2)).collect());
    commit(&store, 3, keys[8..24].iter().map(|&k| set(k, 0xA3)).collect());
    commit(&store, 4, keys[..40].iter().map(|&k| Commitment::new(k, EMPTY_HASH)).collect());

    for version in 1..=4 {
        assert_ne!(store.root(version), EMPTY_HASH);
        assert_eq!(assert_binds_root(&store, version), 1, "version {version}");
    }
}

/// A proof over keys is what it was: the witness path is only taken without an input key.
#[test]
fn a_proof_over_keys_carries_no_extra_witness() {
    let (_dir, store) = open();
    commit(&store, 1, vec![set(key(0x00, 1), 0xA1), set(key(0xFF, 1), 0xA2)]);

    let proof_bytes = store.prove(&[key(0xFF, 1)], 1).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(proof.memberships.len(), 1);
    assert_eq!(proof.leaves.len(), 1);
    assert_eq!(proof.keys[proof.leaves[0].key_idx.get() as usize], *key(0xFF, 1));
    assert_eq!(proof.root::<Sha256>().unwrap(), store.root(1));
}
