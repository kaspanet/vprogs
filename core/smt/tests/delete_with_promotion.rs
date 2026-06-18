//! Regression tests for SMT delete behaviour: shortcut-promotion and stale-leaf retention.
//!
//! When a `Present` query writes `EMPTY_HASH` and its sibling at some depth is a single `Leaf`,
//! the SMT promotes that leaf upward (potentially many levels). Without kind-aware hashing in
//! [`Proof::new_root`], the verifier-side computation would hash empty-sibling-internal nodes that
//! the actual SMT never materializes - producing a post-state root different from
//! [`Tree::update`] + [`Tree::root`].

use tempfile::TempDir;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{Commitment, EMPTY_HASH, Tree, proving::Proof};
use vprogs_core_types::{NoOpCanonicalChain, ResourceId};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// Constructs a key with `byte_0` controlling the MSB prefix bits and `byte_31` ensuring
/// distinguishability among keys sharing the same prefix.
fn key(byte_0: u8, byte_31: u8) -> ResourceId {
    let mut bytes = [0u8; 32];
    bytes[0] = byte_0;
    bytes[31] = byte_31;
    ResourceId::from(bytes)
}

/// Opens a store, applies `commitments` at v=1, and returns the dir and store.
fn store_with(commitments: Vec<Commitment>) -> (TempDir, RocksDbStore) {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    store.update(&mut wb, commitments, 1, [0u8; 32], &NoOpCanonicalChain);
    store.commit(wb);
    (dir, store)
}

/// Two keys sharing all MSB-zero bits up to bit 5 and diverging at bit 5; deleting one causes the
/// survivor to promote all the way up to the root.
#[test]
fn delete_promotes_to_root() {
    // byte 0 = 0x00 (all MSB-bits 0..7 are zero); byte 0 = 0x04 sets MSB-bit 5.
    let k_keep = key(0x00, 0xAA);
    let k_drop = key(0x04, 0xBB);
    let v_keep = [0xCD; 32];
    let v_drop = [0xEF; 32];
    let (_dir, store) =
        store_with(vec![Commitment::new(k_keep, v_keep), Commitment::new(k_drop, v_drop)]);

    // Pre-state proof covering both keys.
    let proof_bytes = store.prove(&[k_drop, k_keep], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    // Apply the delete in the SMT and capture the actual post-state root.
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(k_drop, EMPTY_HASH)],
        2,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);
    let smt_root = store.root(2, &NoOpCanonicalChain);

    // The verifier-side `new_root` must match.
    let computed = proof
        .new_root::<Sha256>(|i| match i {
            0 => &EMPTY_HASH, // k_drop deleted
            1 => &v_keep,     // k_keep unchanged
            _ => unreachable!(),
        })
        .unwrap();
    assert_eq!(computed, smt_root, "post-state root must match SMT after promotion to root");
}

/// Three keys: two sharing a deep prefix (diverge at bit 5), one on the opposite half of the root
/// (bit 0 = 1) that ends up as a root-level shortcut leaf. Deleting one of the deep pair promotes
/// the survivor up, but the root-level Leaf sibling on the other side prevents further promotion.
#[test]
fn delete_promotes_partially_then_stops_at_leaf_sibling() {
    let k_keep = key(0x00, 0xAA); // bit 0 = 0, bit 5 = 0
    let k_drop = key(0x04, 0xBB); // bit 0 = 0, bit 5 = 1
    let k_other = key(0x80, 0xCC); // bit 0 = 1
    let v_keep = [0x11; 32];
    let v_drop = [0x22; 32];
    let v_other = [0x33; 32];
    let (_dir, store) = store_with(vec![
        Commitment::new(k_keep, v_keep),
        Commitment::new(k_drop, v_drop),
        Commitment::new(k_other, v_other),
    ]);

    let proof_bytes = store.prove(&[k_drop, k_keep], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(k_drop, EMPTY_HASH)],
        2,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);
    let smt_root = store.root(2, &NoOpCanonicalChain);

    let computed = proof
        .new_root::<Sha256>(|i| match i {
            0 => &EMPTY_HASH,
            1 => &v_keep,
            _ => unreachable!(),
        })
        .unwrap();
    assert_eq!(computed, smt_root);
}

/// Four keys: two in a deep pair on the left half of the root (diverge at bit 5), two on the right
/// half sharing a deep prefix (diverge only in byte 31). The right half is an `Internal` sibling at
/// depth 1, not a `Leaf` - deleting one of the deep pair must NOT promote the survivor across the
/// `Internal` sibling.
#[test]
fn delete_with_subtree_sibling_does_not_promote_across_subtree() {
    let k_keep = key(0x00, 0xAA); // bit 0 = 0, bit 5 = 0
    let k_drop = key(0x04, 0xBB); // bit 0 = 0, bit 5 = 1
    // Both on bit-0 = 1 side, sharing byte 0 = 0x80 and diverging only at byte 31.
    let k_sub_a = key(0x80, 0xCC);
    let k_sub_b = key(0x80, 0xDD);
    let v_keep = [0x11; 32];
    let v_drop = [0x22; 32];
    let v_sub_a = [0x33; 32];
    let v_sub_b = [0x44; 32];
    let (_dir, store) = store_with(vec![
        Commitment::new(k_keep, v_keep),
        Commitment::new(k_drop, v_drop),
        Commitment::new(k_sub_a, v_sub_a),
        Commitment::new(k_sub_b, v_sub_b),
    ]);

    let proof_bytes = store.prove(&[k_drop, k_keep], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(k_drop, EMPTY_HASH)],
        2,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);
    let smt_root = store.root(2, &NoOpCanonicalChain);

    let computed = proof
        .new_root::<Sha256>(|i| match i {
            0 => &EMPTY_HASH,
            1 => &v_keep,
            _ => unreachable!(),
        })
        .unwrap();
    assert_eq!(computed, smt_root);
}

/// Deleting the only key in the tree drives the root back to `EMPTY_HASH`; the proof's `new_root`
/// must reach the same state.
///
/// Captures `update`'s return value rather than `store.root(2)` because the SMT storage layer
/// doesn't write a tombstone at `Key::ROOT` when the tree becomes entirely empty - `store.root(2)`
/// would surface the stale-marked v=1 root. The existing `single_key_lifecycle` test in `e2e.rs`
/// sidesteps the same quirk the same way.
#[test]
fn delete_only_key_yields_empty_root() {
    let k = key(0x80, 0xAA);
    let v = [0xBB; 32];
    let (_dir, store) = store_with(vec![Commitment::new(k, v)]);

    let proof_bytes = store.prove(&[k], 1, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();

    let mut wb = store.write_batch();
    let smt_root = store.update(
        &mut wb,
        vec![Commitment::new(k, EMPTY_HASH)],
        2,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);

    assert_eq!(smt_root, EMPTY_HASH);
    let computed = proof.new_root::<Sha256>(|_| &EMPTY_HASH).unwrap();
    assert_eq!(computed, EMPTY_HASH);
    assert_eq!(computed, smt_root);
}

/// Builds the bug's shape: a single key on the bit-0 = 0 half (a root-level shortcut leaf) opposite
/// a two-key `Internal` subtree on the bit-0 = 1 half. Returns `(k_drop, k_sub_a, k_sub_b)`.
fn store_with_leaf_opposite_subtree() -> (TempDir, RocksDbStore, ResourceId, ResourceId, ResourceId)
{
    // k_drop is the only key on its half, so it sits as a shortcut leaf directly under the root.
    // The other half holds two keys that diverge only at byte 31, forming an `Internal` subtree
    // - the sibling that blocks promotion when k_drop is deleted.
    let k_drop = key(0x00, 0xAA);
    let k_sub_a = key(0x80, 0xCC);
    let k_sub_b = key(0x80, 0xDD);
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![
            Commitment::new(k_drop, [0x22; 32]),
            Commitment::new(k_sub_a, [0x33; 32]),
            Commitment::new(k_sub_b, [0x44; 32]),
        ],
        1,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);
    (dir, store, k_drop, k_sub_a, k_sub_b)
}

/// Deleting a leaf whose immediate sibling is an `Internal` subtree leaves the parent as
/// `Internal(EMPTY, sibling)` - no promotion fires, so the deleted leaf's own position is abandoned
/// without a tombstone. Reading that version must still see the key as gone; without a tombstone
/// the `latest <= version` seek surfaces the stale pre-deletion leaf and resurrects the resource.
#[test]
fn delete_with_internal_sibling_does_not_resurrect_at_same_version() {
    let (_dir, store, k_drop, ..) = store_with_leaf_opposite_subtree();

    // Delete k_drop at v=2; its position is abandoned with no tombstone written.
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(k_drop, EMPTY_HASH)],
        2,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);

    // Prove k_drop AT v=2 (after the delete). It must witness as empty, and the proof's pre-state
    // root must match the committed root - both fail if the stale leaf is resurfaced.
    let proof_bytes = store.prove(&[k_drop], 2, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(
        proof.member(0).unwrap().value_hash(),
        &EMPTY_HASH,
        "key deleted at v=2 must read as empty at v=2, not resurrect its pre-deletion value",
    );
    assert_eq!(
        proof.root::<Sha256>().unwrap(),
        store.root(2, &NoOpCanonicalChain),
        "proof's pre-state root must match the committed root after deletion",
    );
}

/// The same un-tombstoned position is also re-incorporated by a *later* update that touches the
/// parent region: the updater reads the abandoned position at `prev_version` and carries the stale
/// leaf forward, silently resurrecting the deleted resource at a version where nothing re-created
/// it.
#[test]
fn delete_with_internal_sibling_does_not_resurrect_at_later_version() {
    let (_dir, store, k_drop, k_sub_a, _) = store_with_leaf_opposite_subtree();

    // v=2: delete k_drop.
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(k_drop, EMPTY_HASH)],
        2,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);

    // v=3: touch only the sibling subtree. The root is rebuilt reading v=2, where k_drop's
    // abandoned position still holds its stale leaf - it must not be carried into v=3.
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![Commitment::new(k_sub_a, [0x55; 32])],
        3,
        [0u8; 32],
        &NoOpCanonicalChain,
    );
    store.commit(wb);

    let proof_bytes = store.prove(&[k_drop], 3, &NoOpCanonicalChain).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(
        proof.member(0).unwrap().value_hash(),
        &EMPTY_HASH,
        "key deleted at v=2 must stay absent at v=3, not be resurrected by an unrelated update",
    );
}
