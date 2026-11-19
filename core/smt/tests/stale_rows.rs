//! Pins that a deleted key stays deleted, and that every node row the tree stops using is
//! reclaimed by pruning.
//!
//! A node the tree stops using gets a stale marker, and pruning deletes the marked nodes. Until
//! then the node's row stays readable at its position. Three things went wrong around that:
//!
//! - A delete next to a subtree leaves a tombstone at the leaf's position. The tombstone is marked
//!   stale at the delete's version as well, under the same marker key as the leaf, so the leaf's
//!   marker is replaced. Pruning the delete removed the tombstone and kept the leaf, which then was
//!   the position's latest row again.
//! - An update stored every unchanged sibling along the paths it touched again at its own version
//!   and never marked the sibling's previous row. Those rows stayed in the store, each of them
//!   another copy for a pruned delete to fall back to.
//! - A subtree built below a position that held no internal node (a leaf that splits, a tombstone,
//!   an empty position) was built by reading the positions below it. Those can still hold the nodes
//!   of a subtree that was deleted or promoted away, and the update merged them back in.
//!
//! Pruning a tombstone now removes every node at its position up to the tombstone, an update
//! leaves an unchanged sibling's row where it is, and a subtree below such a position is built
//! from the update's commitments alone.

use std::collections::BTreeMap;

use tempfile::TempDir;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{
    Commitment, EMPTY_HASH, Key, Node, Tree, WriteBatch as SmtWriteBatch, proving::Proof,
};
use vprogs_core_types::ResourceId;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store};

/// A key whose first byte is `first`, so the top bits place it in the tree, and whose last
/// byte is `last`, so keys sharing `first` differ only at the very end.
fn key(first: u8, last: u8) -> ResourceId {
    let mut key = [0u8; 32];
    key[0] = first;
    key[31] = last;
    ResourceId::from(key)
}

/// The tree position reached from the root by following `bits`.
fn position(bits: &[bool]) -> Key {
    let mut path = [0u8; 32];
    for (level, bit) in bits.iter().enumerate() {
        if *bit {
            path[level / 8] |= 0x80 >> (level % 8);
        }
    }
    Key { level: bits.len() as u16, path }
}

/// Opens an empty store.
fn open() -> (TempDir, RocksDbStore) {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    (dir, store)
}

/// Commits `commitments` at `version` and returns the new root.
fn commit(store: &RocksDbStore, version: u64, commitments: Vec<Commitment>) -> [u8; 32] {
    let mut wb = store.write_batch();
    let root = store.update(&mut wb, commitments, version);
    store.commit(wb);
    root
}

/// Prunes every version in `versions`, oldest first.
fn prune(store: &RocksDbStore, versions: impl IntoIterator<Item = u64>) {
    for version in versions {
        let mut wb = store.write_batch();
        store.prune(&mut wb, version);
        store.commit(wb);
    }
}

/// Sets `key` to a value hash of all-`value` bytes.
fn set(key: ResourceId, value: u8) -> Commitment {
    Commitment::new(key, [value; 32])
}

/// Every node row in the store as `(position, version, encoded node)`, read from the raw
/// column: a row key is the 34-byte position followed by the inverted version.
fn rows(store: &RocksDbStore) -> Vec<(Vec<u8>, u64, Vec<u8>)> {
    store
        .prefix_iter(StateSpace::SmtNode, &[])
        .map(|(row_key, node)| {
            let version = !u64::from_be_bytes(row_key[34..42].try_into().unwrap());
            (row_key[..34].to_vec(), version, node)
        })
        .collect()
}

/// The nodes `store` holds a row for at `position` as `(version, node)`, oldest first.
fn nodes_at(store: &RocksDbStore, position: &Key) -> Vec<(u64, Node)> {
    let mut nodes: Vec<(u64, Node)> = rows(store)
        .into_iter()
        .filter(|(row_position, _, _)| row_position[..] == position.encode())
        .map(|(_, version, node)| (version, Node::decode(&mut &node[..]).unwrap()))
        .collect();
    nodes.sort_unstable_by_key(|(version, _)| *version);
    nodes
}

/// The versions `store` holds a row for at `position`, oldest first.
fn versions_at(store: &RocksDbStore, position: &Key) -> Vec<u64> {
    nodes_at(store, position).into_iter().map(|(version, _)| version).collect()
}

/// The store's rows as `position -> encoded node`, asserting it holds one row per position.
fn single_rows(store: &RocksDbStore) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut nodes = BTreeMap::new();
    for (position, version, node) in rows(store) {
        let previous = nodes.insert(position, node);
        assert!(previous.is_none(), "a position holds a second row (version {version})");
    }
    nodes
}

/// The rows of a tree that holds exactly `entries`, built in one commit.
fn fresh_rows(entries: &BTreeMap<ResourceId, [u8; 32]>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let (_dir, store) = open();
    commit(&store, 1, entries.iter().map(|(key, value)| Commitment::new(*key, *value)).collect());
    single_rows(&store)
}

/// The root of a tree that holds exactly `entries`.
fn fresh_root(entries: &[Commitment]) -> [u8; 32] {
    let (_dir, store) = open();
    commit(&store, 1, entries.to_vec())
}

/// Number of stale markers the store holds.
fn markers(store: &RocksDbStore) -> usize {
    store.prefix_iter(StateSpace::SmtStale, &[]).count()
}

/// Proves `key` at `version` and returns the value hash the proof carries for it, after
/// checking that the proof recomputes to the store's root.
#[track_caller]
fn proved_value(store: &RocksDbStore, key: ResourceId, version: u64) -> [u8; 32] {
    let proof_bytes = store.prove(&[key], version).unwrap();
    let proof = Proof::decode(&proof_bytes).unwrap();
    assert_eq!(
        proof.root::<Sha256>().unwrap(),
        store.root(version),
        "the proof at version {version} recomputes to the root",
    );
    *proof.member(0).unwrap().value_hash()
}

/// An update writes rows only for the nodes on the paths it changes. A sibling it passes, a
/// leaf or a whole subtree, keeps the one row it has, and every version still reads through it.
#[test]
fn an_update_writes_no_row_for_an_unchanged_sibling() {
    // `a` and the pair `b1`, `b2` share the left half, `c` sits in the right one.
    let (a, b1, b2, c) = (key(0x00, 1), key(0x40, 1), key(0x60, 1), key(0xFF, 1));
    let (_dir, store) = open();

    let root_1 = commit(&store, 1, vec![set(a, 0xA1), set(b1, 0xB1), set(b2, 0xB2), set(c, 0xC1)]);
    // Changes the path to `b1`: its siblings are `c`, `a` and `b2`.
    let root_2 = commit(&store, 2, vec![set(b1, 0xB3)]);
    // Changes the path to `c`: its sibling is the whole left subtree.
    let root_3 = commit(&store, 3, vec![set(c, 0xC2)]);

    assert_eq!(versions_at(&store, &Key::ROOT), [1, 2, 3], "the root changes with every update");
    assert_eq!(versions_at(&store, &position(&[true])), [1, 3], "c is written when it is set");
    assert_eq!(versions_at(&store, &position(&[false, false])), [1], "a never changes");
    assert_eq!(versions_at(&store, &position(&[false, true, true])), [1], "b2 never changes");
    assert_eq!(versions_at(&store, &position(&[false, true, false])), [1, 2], "b1 is set at 2");
    assert_eq!(versions_at(&store, &position(&[false])), [1, 2], "the left subtree changes at 2");

    // Every version still proves to its own root through the shared rows.
    for (version, root, values) in [
        (1, root_1, [0xA1, 0xB1, 0xB2, 0xC1]),
        (2, root_2, [0xA1, 0xB3, 0xB2, 0xC1]),
        (3, root_3, [0xA1, 0xB3, 0xB2, 0xC2]),
    ] {
        assert_eq!(store.root(version), root);
        for (key, value) in [a, b1, b2, c].into_iter().zip(values) {
            assert_eq!(proved_value(&store, key, version), [value; 32], "version {version}");
        }
    }

    // Pruned to the tip, the store holds the live tree and nothing else.
    prune(&store, 1..=3);
    let live =
        BTreeMap::from([(a, [0xA1; 32]), (b1, [0xB3; 32]), (b2, [0xB2; 32]), (c, [0xC2; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live));
    assert_eq!(markers(&store), 0, "every marker is consumed");
    assert_eq!(store.root(3), root_3);
}

/// A key deleted next to a subtree stays deleted once its delete is pruned: the prune removes
/// the tombstone and the leaf under it. It used to remove the tombstone and keep the leaf.
#[test]
fn a_pruned_delete_keeps_the_key_deleted() {
    // `a` sits next to the subtree of `b1` and `b2`, so its delete promotes nothing.
    let (a, b1, b2) = (key(0x00, 1), key(0x40, 1), key(0x60, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(b1, 0xB1), set(b2, 0xB2)]);
    let root_2 = commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH)]);
    assert_eq!(root_2, fresh_root(&[set(b1, 0xB1), set(b2, 0xB2)]));
    assert_eq!(proved_value(&store, a, 2), EMPTY_HASH, "deleted before the prune");
    assert_eq!(
        nodes_at(&store, &position(&[false, false])),
        [(1, Node::leaf::<Sha256>(a, [0xA1; 32])), (2, Node::Empty)],
        "until the prune a tombstone shadows the leaf",
    );

    prune(&store, 1..=2);
    assert_eq!(store.root(2), root_2);
    assert_eq!(proved_value(&store, a, 2), EMPTY_HASH, "still deleted after the prune");
    let live = BTreeMap::from([(b1, [0xB1; 32]), (b2, [0xB2; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");

    // The next update next to it builds on a tree without the key.
    let root_3 = commit(&store, 3, vec![set(b2, 0xB3)]);
    assert_eq!(root_3, fresh_root(&[set(b1, 0xB1), set(b2, 0xB3)]));
    assert_eq!(proved_value(&store, a, 3), EMPTY_HASH);
}

/// The same for a key that an update passed before it was deleted, on its way to a key next
/// to it.
#[test]
fn a_pruned_delete_keeps_a_key_deleted_that_an_update_passed() {
    let (a, b1, b2) = (key(0x00, 1), key(0x40, 1), key(0x60, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(b1, 0xB1), set(b2, 0xB2)]);
    // Passes `a` on its way to `b1`.
    commit(&store, 2, vec![set(b1, 0xB3)]);
    let root_3 = commit(&store, 3, vec![Commitment::new(a, EMPTY_HASH)]);
    assert_eq!(root_3, fresh_root(&[set(b1, 0xB3), set(b2, 0xB2)]));

    prune(&store, 1..=3);
    assert_eq!(store.root(3), root_3);
    assert_eq!(proved_value(&store, a, 3), EMPTY_HASH, "still deleted after the prune");
    let live = BTreeMap::from([(b1, [0xB3; 32]), (b2, [0xB2; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");

    let root_4 = commit(&store, 4, vec![set(b2, 0xB4)]);
    assert_eq!(root_4, fresh_root(&[set(b1, 0xB3), set(b2, 0xB4)]));
    assert_eq!(proved_value(&store, a, 4), EMPTY_HASH);
}

/// A key set again before its delete is pruned replaces the tombstone, and pruning both
/// versions leaves the new leaf alone at the position.
#[test]
fn a_key_set_again_before_its_delete_is_pruned_leaves_one_row() {
    let (a, b1, b2) = (key(0x00, 1), key(0x40, 1), key(0x60, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(b1, 0xB1), set(b2, 0xB2)]);
    commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH)]);
    let root_3 = commit(&store, 3, vec![set(a, 0xA3)]);

    prune(&store, 1..=3);
    assert_eq!(store.root(3), root_3);
    assert_eq!(proved_value(&store, a, 3), [0xA3; 32]);
    let live = BTreeMap::from([(a, [0xA3; 32]), (b1, [0xB1; 32]), (b2, [0xB2; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live));
    assert_eq!(markers(&store), 0, "every marker is consumed");
}

/// A subtree drained in one update stays drained once that update is pruned. Its node and its
/// leaves were passed by earlier updates, and after the prune none of their positions holds a
/// row.
#[test]
fn a_pruned_subtree_delete_keeps_its_keys_deleted() {
    // The pairs `a1`, `a2` and `b1`, `b2` are two subtrees side by side, so draining one
    // promotes nothing. `c` sits in the other half.
    let (a1, a2) = (key(0x00, 1), key(0x20, 1));
    let (b1, b2) = (key(0x40, 1), key(0x60, 1));
    let c = key(0xFF, 1);
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a1, 0xA1), set(a2, 0xA2), set(b1, 0xB1), set(b2, 0xB2), set(c, 1)]);
    // Updates that pass the subtree of the `b` keys, and each of the two inside it.
    commit(&store, 2, vec![set(a1, 0xA3)]);
    commit(&store, 3, vec![set(b1, 0xB3)]);
    commit(&store, 4, vec![set(b2, 0xB4)]);
    let deletes = vec![Commitment::new(b1, EMPTY_HASH), Commitment::new(b2, EMPTY_HASH)];
    let root_5 = commit(&store, 5, deletes);
    assert_eq!(root_5, fresh_root(&[set(a1, 0xA3), set(a2, 0xA2), set(c, 1)]));

    prune(&store, 1..=5);
    assert_eq!(store.root(5), root_5);
    for key in [b1, b2] {
        assert_eq!(proved_value(&store, key, 5), EMPTY_HASH, "still deleted after the prune");
    }
    let mut live = BTreeMap::from([(a1, [0xA3; 32]), (a2, [0xA2; 32]), (c, [1; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");

    // New keys in the drained subtree build on a tree without the deleted ones.
    let (x, y) = (key(0x50, 1), key(0x70, 1));
    let root_6 = commit(&store, 6, vec![set(x, 0xD1), set(y, 0xD2)]);
    live.extend([(x, [0xD1; 32]), (y, [0xD2; 32])]);
    let entries: Vec<_> = live.iter().map(|(key, value)| Commitment::new(*key, *value)).collect();
    assert_eq!(root_6, fresh_root(&entries));
}

/// A store written before this change still holds the copies of unchanged siblings. A pruned
/// delete removes the copies of the leaf with it, so the key stays deleted there as well.
#[test]
fn a_pruned_delete_removes_the_copies_an_older_updater_left() {
    let (a, b1, b2) = (key(0x00, 1), key(0x40, 1), key(0x60, 1));
    let a_position = position(&[false, false]);
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(b1, 0xB1), set(b2, 0xB2)]);
    // Two updates that pass `a`, with the copy of its leaf the older updater stored at each
    // version, unmarked.
    for (version, value) in [(2, 0xB3), (3, 0xB4)] {
        let mut wb = store.write_batch();
        store.update(&mut wb, vec![set(b1, value)], version);
        wb.put_node(&a_position, version, &Node::leaf::<Sha256>(a, [0xA1; 32]));
        store.commit(wb);
    }
    assert_eq!(versions_at(&store, &a_position), [1, 2, 3]);

    let root_4 = commit(&store, 4, vec![Commitment::new(a, EMPTY_HASH)]);

    prune(&store, 1..=4);
    assert_eq!(store.root(4), root_4);
    assert_eq!(proved_value(&store, a, 4), EMPTY_HASH, "still deleted after the prune");
    assert!(versions_at(&store, &a_position).is_empty(), "no copy of the leaf is left");
}

/// The root and every value of `live` at `version`, checked against a tree built from `live`
/// alone, plus `gone` proving as empty.
#[track_caller]
fn assert_tree(
    store: &RocksDbStore,
    version: u64,
    live: &BTreeMap<ResourceId, [u8; 32]>,
    gone: &[ResourceId],
) {
    let entries: Vec<_> = live.iter().map(|(key, value)| Commitment::new(*key, *value)).collect();
    assert_eq!(store.root(version), fresh_root(&entries), "the root is the live tree's");
    for (key, value) in live {
        assert_eq!(proved_value(store, *key, version), *value);
    }
    for key in gone {
        assert_eq!(proved_value(store, *key, version), EMPTY_HASH, "a deleted key is back");
    }
}

/// A deleted key stays deleted when the leaf that was promoted over it is split again, with the
/// delete not pruned yet. The split used to read the deleted leaf's row and merge it back.
#[test]
fn a_deleted_key_stays_deleted_when_the_promoted_leaf_splits() {
    let (a, c, x) = (key(0x00, 1), key(0xFF, 1), key(0x00, 2));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(c, 0xC1)]);
    // Promotes `c` to the root.
    commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH)]);
    // Splits `c`, with `x` going where `a` was.
    commit(&store, 3, vec![set(x, 0xD1)]);

    let live = BTreeMap::from([(c, [0xC1; 32]), (x, [0xD1; 32])]);
    assert_tree(&store, 3, &live, &[a]);
    prune(&store, 1..=3);
    assert_tree(&store, 3, &live, &[a]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");
}

/// The same when the new key goes next to the promoted leaf, so the side of the deleted key
/// gets no commitment at all. Its row used to be taken for the live node of that side.
#[test]
fn a_deleted_key_stays_deleted_when_the_split_passes_its_position() {
    // Deleting `a` promotes `b` over the node the two shared.
    let (a, b, c, x) = (key(0x00, 1), key(0x40, 1), key(0xFF, 1), key(0x50, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(b, 0xB1), set(c, 0xC1)]);
    commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH)]);
    // Splits `b`: both `b` and `x` go right below it.
    commit(&store, 3, vec![set(x, 0xD1)]);

    let live = BTreeMap::from([(b, [0xB1; 32]), (c, [0xC1; 32]), (x, [0xD1; 32])]);
    assert_tree(&store, 3, &live, &[a]);
    prune(&store, 1..=3);
    assert_tree(&store, 3, &live, &[a]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");
}

/// A promoted leaf leaves no row behind: the one at its old position goes stale with the move
/// and is pruned with it, so a later split finds nothing of it there.
#[test]
fn a_promoted_leaf_leaves_no_row_at_its_old_position() {
    // Deleting `b` promotes `a` one level up, and deleting `a` then promotes `c` to the root.
    let (a, b, c) = (key(0x00, 1), key(0x40, 1), key(0xFF, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(b, 0xB1), set(c, 0xC1)]);
    commit(&store, 2, vec![Commitment::new(b, EMPTY_HASH)]);
    commit(&store, 3, vec![Commitment::new(a, EMPTY_HASH)]);
    prune(&store, 1..=3);
    let mut live = BTreeMap::from([(c, [0xC1; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");

    // Two keys that split `c` and walk down to where `a` first was.
    let (x, y) = (key(0x00, 2), key(0x20, 1));
    commit(&store, 4, vec![set(x, 0xD1), set(y, 0xD2)]);
    live.extend([(x, [0xD1; 32]), (y, [0xD2; 32])]);
    assert_tree(&store, 4, &live, &[a, b]);
}

/// A subtree that is drained and filled again before the drain is pruned holds the new keys
/// alone. The rebuild used to read the drained leaves, which were still stored below the
/// subtree's tombstone.
#[test]
fn a_drained_subtree_filled_again_before_the_prune_holds_the_new_keys_alone() {
    let (a1, a2) = (key(0x00, 1), key(0x20, 1));
    let (b1, b2) = (key(0x40, 1), key(0x60, 1));
    let c = key(0xFF, 1);

    // The new keys go to both sides of the drained subtree, or both to one side of it, which
    // leaves the other side without a commitment.
    for (x, y) in [(key(0x50, 1), key(0x70, 1)), (key(0x48, 1), key(0x50, 1))] {
        let (_dir, store) = open();
        let filled = vec![set(a1, 0xA1), set(a2, 0xA2), set(b1, 0xB1), set(b2, 0xB2), set(c, 1)];
        commit(&store, 1, filled);
        commit(&store, 2, vec![Commitment::new(b1, EMPTY_HASH), Commitment::new(b2, EMPTY_HASH)]);
        commit(&store, 3, vec![set(x, 0xD1), set(y, 0xD2)]);

        let live = BTreeMap::from([
            (a1, [0xA1; 32]),
            (a2, [0xA2; 32]),
            (c, [1; 32]),
            (x, [0xD1; 32]),
            (y, [0xD2; 32]),
        ]);
        assert_tree(&store, 3, &live, &[b1, b2]);
        prune(&store, 1..=3);
        assert_tree(&store, 3, &live, &[b1, b2]);
        assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
        assert_eq!(markers(&store), 0, "every marker is consumed");
    }
}

/// A whole subtree deleted next to a leaf promotes the leaf over it and leaves its internal
/// node behind. A key that splits the leaf before the prune walks into that node, whose
/// children are the deleted leaves. None of it is read.
#[test]
fn a_split_below_a_promoted_leaf_ignores_the_subtree_it_was_promoted_over() {
    // `a1` and `a2` form the left subtree, `c` is the leaf next to it.
    let (a1, a2, c, x) = (key(0x00, 1), key(0x40, 1), key(0xFF, 1), key(0x10, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a1, 0xA1), set(a2, 0xA2), set(c, 0xC1)]);
    // Drains the left subtree and promotes `c` to the root.
    commit(&store, 2, vec![Commitment::new(a1, EMPTY_HASH), Commitment::new(a2, EMPTY_HASH)]);
    // Splits `c`, with `x` going into the drained subtree's position.
    commit(&store, 3, vec![set(x, 0xD1)]);

    let live = BTreeMap::from([(c, [0xC1; 32]), (x, [0xD1; 32])]);
    assert_tree(&store, 3, &live, &[a1, a2]);
    prune(&store, 1..=3);
    assert_tree(&store, 3, &live, &[a1, a2]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");
}

/// A drained tree filled again before the drain is pruned holds the new keys alone.
#[test]
fn a_drained_tree_filled_again_before_the_prune_holds_the_new_keys_alone() {
    let (a, c) = (key(0x00, 1), key(0xFF, 1));
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a, 0xA1), set(c, 0xC1)]);
    let root_2 =
        commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH), Commitment::new(c, EMPTY_HASH)]);
    assert_eq!(root_2, EMPTY_HASH);
    let (x, y) = (key(0x00, 2), key(0xFF, 2));
    commit(&store, 3, vec![set(x, 0xD1), set(y, 0xD2)]);

    let live = BTreeMap::from([(x, [0xD1; 32]), (y, [0xD2; 32])]);
    assert_tree(&store, 3, &live, &[a, c]);
    prune(&store, 1..=3);
    assert_tree(&store, 3, &live, &[a, c]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the store holds the live tree alone");
    assert_eq!(markers(&store), 0, "every marker is consumed");
}

/// A store written before this change holds copies of unchanged siblings at positions a later
/// delete vacates without a tombstone: the leaves of a drained subtree. Pruning the delete
/// leaves the older copies there, and a rebuild of the subtree does not read them.
#[test]
fn a_rebuilt_subtree_ignores_the_copies_an_older_updater_left_below_it() {
    let (a1, a2) = (key(0x00, 1), key(0x20, 1));
    let (b1, b2) = (key(0x40, 1), key(0x60, 1));
    let c = key(0xFF, 1);
    let b2_position = position(&[false, true, true]);
    let (_dir, store) = open();

    commit(&store, 1, vec![set(a1, 0xA1), set(a2, 0xA2), set(b1, 0xB1), set(b2, 0xB2), set(c, 1)]);
    // An update that passes `b2`, with the copy of its leaf the older updater stored, unmarked.
    let mut wb = store.write_batch();
    store.update(&mut wb, vec![set(b1, 0xB3)], 2);
    wb.put_node(&b2_position, 2, &Node::leaf::<Sha256>(b2, [0xB2; 32]));
    store.commit(wb);

    commit(&store, 3, vec![Commitment::new(b1, EMPTY_HASH), Commitment::new(b2, EMPTY_HASH)]);
    prune(&store, 1..=3);
    assert_eq!(versions_at(&store, &b2_position), [1], "the older copy has no marker");

    let (x, y) = (key(0x50, 1), key(0x70, 1));
    commit(&store, 4, vec![set(x, 0xD1), set(y, 0xD2)]);
    let live = BTreeMap::from([
        (a1, [0xA1; 32]),
        (a2, [0xA2; 32]),
        (c, [1; 32]),
        (x, [0xD1; 32]),
        (y, [0xD2; 32]),
    ]);
    assert_tree(&store, 4, &live, &[b1, b2]);
}

/// Over a long run of inserts, overwrites and deletes, every checked version has the root of
/// its live keys, and pruned to the tip the store holds exactly the rows of the live tree: one
/// per node, and no marker.
#[test]
fn pruning_to_the_tip_leaves_exactly_the_live_tree() {
    // Twelve clusters spread over the top bits, each of four keys that share their first
    // byte and split within the second one.
    let keys: Vec<ResourceId> = (0u8..48)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[0] = (i / 4).wrapping_mul(151);
            bytes[1] = i % 4;
            ResourceId::from(bytes)
        })
        .collect();

    // Two runs of one fixed pseudo-random sequence, each version touching one to four keys
    // and deleting about every third of them. The first run is pruned only at the end, so its
    // updates meet the rows earlier deletes left. The second is pruned after every version.
    for prune_each_version in [false, true] {
        let (_dir, store) = open();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |bound: u64| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) % bound
        };
        let mut live = BTreeMap::new();
        let tip = 80;
        for version in 1..=tip {
            let mut commitments = Vec::new();
            for _ in 0..=next(4) {
                let key = keys[next(keys.len() as u64) as usize];
                if next(3) == 0 {
                    live.remove(&key);
                    commitments.push(Commitment::new(key, EMPTY_HASH));
                } else {
                    let value = [next(250) as u8 + 1; 32];
                    live.insert(key, value);
                    commitments.push(Commitment::new(key, value));
                }
            }
            let root = commit(&store, version, commitments);
            if prune_each_version {
                prune(&store, [version]);
            }

            // Every eighth root is checked against a tree built from the live keys alone.
            if version % 8 == 0 {
                let entries: Vec<_> =
                    live.iter().map(|(key, value)| Commitment::new(*key, *value)).collect();
                assert_eq!(root, fresh_root(&entries), "version {version}");
            }
        }
        assert!(!live.is_empty(), "the run ends on a tree that holds keys");

        prune(&store, 1..=tip);
        let gone: Vec<_> = keys.iter().filter(|key| !live.contains_key(*key)).copied().collect();
        assert_tree(&store, tip, &live, &gone);
        assert_eq!(single_rows(&store), fresh_rows(&live));
        assert_eq!(markers(&store), 0, "every marker is consumed");
    }
}

/// Pruning a version that a rollback orphaned removes the nodes it wrote and leaves the ones
/// it replaced, which are the canonical ones again.
#[test]
fn pruning_an_orphaned_version_removes_its_own_nodes() {
    let (a, b1, b2) = (key(0x00, 1), key(0x40, 1), key(0x60, 1));
    let (_dir, store) = open();
    let mut versions = store.canonical_chain_manager::<u64>(0);

    assert_eq!(versions.append(1).id, 1);
    let root_1 = commit(&store, 1, vec![set(a, 0xA1), set(b1, 0xB1), set(b2, 0xB2)]);
    // Version 2 deletes `a`, overwrites `b1`, and is rolled back.
    assert_eq!(versions.append(2).id, 2);
    commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH), set(b1, 0xB3)]);
    versions.rollback(1);

    prune_history(&store, 2, &[1]);

    let live = BTreeMap::from([(a, [0xA1; 32]), (b1, [0xB1; 32]), (b2, [0xB2; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "version 1's tree is all that is left");
    assert_eq!(markers(&store), 0, "every marker is consumed");
    assert_eq!(store.root(1), root_1);
}

/// Prunes every version up to `tip` as the pruning worker does: a canonical one by its
/// markers, an orphaned one by its own nodes.
fn prune_history(store: &RocksDbStore, tip: u64, canonical: &[u64]) {
    for version in 1..=tip {
        let mut wb = store.write_batch();
        if canonical.contains(&version) {
            store.prune(&mut wb, version);
        } else {
            store.prune_orphaned(&mut wb, version);
        }
        store.commit(wb);
    }
}

/// An orphaned version that rebuilt a subtree leaves none of its nodes behind: the children of
/// the node it built hang off the root it wrote, like every other node it wrote.
#[test]
fn pruning_an_orphaned_version_removes_the_subtree_it_built() {
    let (a, c) = (key(0x00, 1), key(0xFF, 1));
    let (_dir, store) = open();
    let mut versions = store.canonical_chain_manager::<u64>(0);

    assert_eq!(versions.append(1).id, 1);
    commit(&store, 1, vec![set(a, 0xA1), set(c, 0xC1)]);
    // Version 2 deletes `a`, which promotes `c` to the root, and version 3 sets `a` again,
    // which splits `c` and builds both leaves anew. Both are rolled back.
    assert_eq!(versions.append(2).id, 2);
    commit(&store, 2, vec![Commitment::new(a, EMPTY_HASH)]);
    assert_eq!(versions.append(3).id, 3);
    commit(&store, 3, vec![set(a, 0xA3)]);
    versions.rollback(1);
    // Version 4 builds on version 1 and passes `a`.
    assert_eq!(versions.append(4).id, 4);
    commit(&store, 4, vec![set(c, 0xC4)]);

    prune_history(&store, 4, &[1, 4]);
    let live = BTreeMap::from([(a, [0xA1; 32]), (c, [0xC4; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the canonical tree is all that is left");
    assert_eq!(markers(&store), 0, "every marker is consumed");
    assert_tree(&store, 4, &live, &[]);
}

/// An orphaned version that wrote a node where there was none leaves nothing behind either,
/// also next to a block that returned under its old id after it.
#[test]
fn pruning_an_orphaned_version_removes_a_node_it_created() {
    let (a, c) = (key(0x00, 1), key(0xFF, 1));
    let (_dir, store) = open();
    let mut versions = store.canonical_chain_manager::<u64>(0);

    assert_eq!(versions.append(1).id, 1);
    commit(&store, 1, vec![set(a, 0xA1)]);
    // Block 2 creates `c`, is rolled back, and returns after block 3 created `c` as well.
    assert_eq!(versions.append(2).id, 2);
    commit(&store, 2, vec![set(c, 0xC1)]);
    versions.rollback(1);
    assert_eq!(versions.append(3).id, 3);
    commit(&store, 3, vec![set(c, 0xC3)]);
    versions.rollback(1);
    assert_eq!(versions.append(2).id, 2, "block 2 returns under its id");
    // Version 4 builds on version 2 and passes `c`.
    assert_eq!(versions.append(4).id, 4);
    commit(&store, 4, vec![set(a, 0xA4)]);

    prune_history(&store, 4, &[1, 2, 4]);
    let live = BTreeMap::from([(a, [0xA4; 32]), (c, [0xC1; 32])]);
    assert_eq!(single_rows(&store), fresh_rows(&live), "the canonical tree is all that is left");
    assert_eq!(markers(&store), 0, "every marker is consumed");
    assert_tree(&store, 4, &live, &[]);
}

/// Over a long run of updates with rollbacks in between, pruned to the tip, the store holds
/// exactly the rows of the canonical tree: no node of an orphaned version, and no marker.
#[test]
fn pruning_a_reorged_history_leaves_exactly_the_canonical_tree() {
    let keys: Vec<ResourceId> = (0u8..32)
        .map(|i| {
            let mut bytes = [0u8; 32];
            bytes[0] = (i / 4).wrapping_mul(151);
            bytes[1] = i % 4;
            ResourceId::from(bytes)
        })
        .collect();
    let (_dir, store) = open();
    let mut versions = store.canonical_chain_manager::<u64>(0);

    // A fixed pseudo-random sequence: each step appends a version that touches one to four
    // keys and deletes about every third of them, and about every sixth step first rolls back
    // up to five versions.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = |bound: u64| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) % bound
    };
    // The canonical ids, oldest first, and the live keys each of them left.
    let mut canonical: Vec<u64> = Vec::new();
    let mut states: BTreeMap<u64, BTreeMap<ResourceId, [u8; 32]>> = BTreeMap::new();
    let mut tip = 0;
    for step in 0..120u64 {
        if canonical.len() > 1 && next(6) == 0 {
            let keep = canonical.len() - 1 - next((canonical.len() as u64 - 1).min(5)) as usize;
            canonical.truncate(keep.max(1));
            versions.rollback(*canonical.last().unwrap());
        }

        let mut live = canonical.last().map(|id| states[id].clone()).unwrap_or_default();
        let mut commitments = Vec::new();
        for _ in 0..=next(4) {
            let key = keys[next(keys.len() as u64) as usize];
            if next(3) == 0 {
                live.remove(&key);
                commitments.push(Commitment::new(key, EMPTY_HASH));
            } else {
                let value = [next(250) as u8 + 1; 32];
                live.insert(key, value);
                commitments.push(Commitment::new(key, value));
            }
        }
        tip = versions.append(step).id;
        let root = commit(&store, tip, commitments);

        // Every eighth root is checked against a tree built from the live keys alone.
        if step % 8 == 0 {
            let entries: Vec<_> =
                live.iter().map(|(key, value)| Commitment::new(*key, *value)).collect();
            assert_eq!(root, fresh_root(&entries), "version {tip}");
        }
        canonical.push(tip);
        states.insert(tip, live);
    }
    assert!((canonical.len() as u64) < tip, "the run rolled versions back");

    // The orphaned versions wrote nodes several levels below their roots, a tombstone among
    // them, so the prune below has to find them through the internal nodes above them.
    let orphaned: Vec<_> = rows(&store)
        .into_iter()
        .filter(|(_, version, _)| !canonical.contains(version))
        .map(|(position, _, node)| (Key::decode(&mut &position[..]).unwrap().level, node))
        .collect();
    assert!(orphaned.iter().any(|(level, _)| *level > 2), "an orphaned version wrote deep down");
    let tombstone = Node::Empty.encode();
    let below_root = |(level, node): &(u16, Vec<u8>)| *level > 0 && *node == tombstone;
    assert!(orphaned.iter().any(below_root), "one left a tombstone below its root");

    prune_history(&store, tip, &canonical);
    let live = &states[&tip];
    assert!(!live.is_empty(), "the run ends on a tree that holds keys");
    let gone: Vec<_> = keys.iter().filter(|key| !live.contains_key(*key)).copied().collect();
    assert_tree(&store, tip, live, &gone);
    assert_eq!(single_rows(&store), fresh_rows(live));
    assert_eq!(markers(&store), 0, "every marker is consumed");
}

/// A sibling's row stays in use across a version that a rollback orphaned: the update after
/// the rollback reads the sibling as the canonical version left it.
#[test]
fn an_unchanged_sibling_is_read_past_an_orphaned_version() {
    let (a, c) = (key(0x00, 1), key(0xFF, 1));
    let (_dir, store) = open();
    let mut versions = store.canonical_chain_manager::<u64>(0);

    assert_eq!(versions.append(1).id, 1);
    commit(&store, 1, vec![set(a, 0xA1), set(c, 0xC1)]);

    // Version 2 sets `c` and is rolled back.
    assert_eq!(versions.append(2).id, 2);
    commit(&store, 2, vec![set(c, 0xC2)]);
    versions.rollback(1);

    // Version 3 sets `a` on top of version 1 and passes `c`.
    assert_eq!(versions.append(3).id, 3);
    let root_3 = commit(&store, 3, vec![set(a, 0xA3)]);

    assert_eq!(root_3, fresh_root(&[set(a, 0xA3), set(c, 0xC1)]));
    assert_eq!(proved_value(&store, c, 3), [0xC1; 32], "c is the one version 1 wrote");
    assert_eq!(proved_value(&store, a, 3), [0xA3; 32]);
}
