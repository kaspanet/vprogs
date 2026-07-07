//! Correctness oracle for the streaming bulk-load builder.
//!
//! [`build_sorted`] must be byte-identical to `Updater` (driven by [`Tree::update`]) on an empty
//! store: the same root hash and the same set of `put_node` writes. These tests prove that two
//! ways:
//!
//! 1. In-memory capture: an empty [`Tree`] plus a [`WriteBatch`] that records every `put_node`
//!    call. Running both paths and comparing the recorded maps is an exact, order-independent
//!    comparison of the entire node set (key, version, and encoded node) - equivalent to the
//!    persisted `SmtNode` column family, since on an empty store every write is a `put_node` with
//!    the same version.
//! 2. RocksDB integration: the real store backing both paths, comparing roots and the full set of
//!    nodes reachable from the root (which, on an empty store, is every node ever written).

use std::collections::BTreeMap;

use tempfile::TempDir;
use vprogs_core_codec::Bits;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{Commitment, Key, Node, StaleNode, Tree, WriteBatch, build_sorted};
use vprogs_core_types::ResourceId;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

// -- In-memory oracle scaffolding --

/// An always-empty tree: `node` never resolves, so `Updater` runs its empty-store path.
#[derive(Clone)]
struct EmptyTree;

impl Tree for EmptyTree {
    type Hasher = Sha256;
    type Snapshot = ();

    fn snapshot(&self) {}

    fn node(&self, _key: &Key, _max_version: u64, _snapshot: &()) -> Option<(u64, Node)> {
        None
    }

    fn prune(&self, _wb: &mut impl WriteBatch, _version: u64) {
        unreachable!("oracle never prunes")
    }
}

/// Records every `put_node` write, keyed by the 34-byte node key encoding.
#[derive(Default)]
struct Capture {
    nodes: BTreeMap<[u8; 34], (u64, Vec<u8>)>,
}

impl WriteBatch for Capture {
    fn put_node(&mut self, key: &Key, version: u64, data: &Node) {
        self.nodes.insert(key.encode(), (version, data.encode()));
    }

    fn put_stale_node(&mut self, _stale: &StaleNode) {
        unreachable!("empty store never marks nodes stale")
    }

    fn delete_node(&mut self, _key: &Key, _version: u64) {
        unreachable!("empty store never deletes nodes")
    }

    fn delete_stale_node(&mut self, _stale: &StaleNode) {
        unreachable!("empty store never deletes stale markers")
    }
}

/// Runs `Updater` (via [`Tree::update`]) on an empty tree; returns the root and captured node set.
fn updater_build(leaves: &[(ResourceId, [u8; 32])], version: u64) -> ([u8; 32], Capture) {
    let commitments = leaves.iter().map(|&(id, vh)| Commitment::new(id, vh)).collect();
    let mut cap = Capture::default();
    let root = EmptyTree.update(&mut cap, commitments, version);
    (root, cap)
}

/// Runs the streaming builder; returns the root and captured node set.
fn builder_build(leaves: &[(ResourceId, [u8; 32])], version: u64) -> ([u8; 32], Capture) {
    let mut cap = Capture::default();
    let root = build_sorted::<_, Sha256>(&mut cap, version, leaves.iter().copied());
    (root, cap)
}

/// Asserts the builder is byte-identical to `Updater` for `leaves` at `version`.
fn assert_identical(leaves: &[(ResourceId, [u8; 32])], version: u64, tag: &str) {
    let (root_o, cap_o) = updater_build(leaves, version);
    let (root_b, cap_b) = builder_build(leaves, version);
    assert_eq!(root_b, root_o, "root mismatch [{tag}]");
    assert_eq!(cap_b.nodes, cap_o.nodes, "node-set mismatch [{tag}]");
}

// -- Leaf generation --

/// Generates `n` unique leaves sorted ascending by resource id, each with a non-empty value hash.
fn random_leaves(rng: &mut fastrand::Rng, n: usize) -> Vec<(ResourceId, [u8; 32])> {
    let mut set: BTreeMap<[u8; 32], [u8; 32]> = BTreeMap::new();
    while set.len() < n {
        let mut id = [0u8; 32];
        let mut vh = [0u8; 32];
        for b in id.iter_mut() {
            *b = rng.u8(..);
        }
        for b in vh.iter_mut() {
            *b = rng.u8(..);
        }
        if vh == [0u8; 32] {
            vh = [1u8; 32];
        }
        set.insert(id, vh);
    }
    set.into_iter().map(|(id, vh)| (ResourceId::from(id), vh)).collect()
}

/// Builds a sorted, unique leaf set from explicit (id, value-hash) pairs.
fn leaves_of(pairs: &[([u8; 32], [u8; 32])]) -> Vec<(ResourceId, [u8; 32])> {
    let mut out: Vec<_> = pairs.iter().map(|&(id, vh)| (ResourceId::from(id), vh)).collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

// -- In-memory oracle tests --

/// Many small random leaf sets (N in 1..=50), one distinct seed per iteration.
#[test]
fn builder_matches_updater_random_small() {
    for seed in 0..400u64 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let n = (seed as usize % 50) + 1;
        let version = (seed % 7) + 1;
        let leaves = random_leaves(&mut rng, n);
        assert_identical(&leaves, version, &format!("small seed={seed} n={n}"));
    }
}

/// Larger random leaf sets (N up to ~500) across distinct seeds.
#[test]
fn builder_matches_updater_random_large() {
    for seed in 1000..1030u64 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let n = 200 + (seed as usize % 300);
        let leaves = random_leaves(&mut rng, n);
        assert_identical(&leaves, 1, &format!("large seed={seed} n={n}"));
    }
}

/// Structural edge cases that stress bit-level splitting and shortcut placement.
#[test]
fn builder_matches_updater_edge_cases() {
    // Single leaf: root is a shortcut leaf.
    assert_identical(&leaves_of(&[([7u8; 32], [9u8; 32])]), 1, "single");

    // Two leaves differing only in the last bit (bit 255): a full-depth split.
    let mut last_bit = [0u8; 32];
    last_bit[31] = 1;
    assert_identical(
        &leaves_of(&[([0u8; 32], [1u8; 32]), (last_bit, [2u8; 32])]),
        1,
        "differ-last-bit",
    );

    // Two leaves differing in the first bit (bit 0): split at the root.
    let mut first_bit = [0u8; 32];
    first_bit[0] = 0x80;
    assert_identical(
        &leaves_of(&[([0u8; 32], [1u8; 32]), (first_bit, [2u8; 32])]),
        1,
        "differ-first-bit",
    );

    // Keys [0; 32] and [0xff; 32]: opposite halves of the root.
    assert_identical(
        &leaves_of(&[([0u8; 32], [1u8; 32]), ([0xffu8; 32], [2u8; 32])]),
        1,
        "min-max",
    );

    // Keys sharing a long prefix, diverging only in the final two bytes.
    let mut a = [0xAAu8; 32];
    let mut b = [0xAAu8; 32];
    let mut c = [0xAAu8; 32];
    a[30] = 0x00;
    a[31] = 0x01;
    b[30] = 0x00;
    b[31] = 0x02;
    c[30] = 0x01;
    c[31] = 0x00;
    assert_identical(
        &leaves_of(&[(a, [3u8; 32]), (b, [4u8; 32]), (c, [5u8; 32])]),
        1,
        "long-shared-prefix",
    );

    // Three leaves where one is a root-level shortcut and two form a deep pair.
    let k_keep = {
        let mut k = [0u8; 32];
        k[31] = 0xAA;
        k
    };
    let k_deep = {
        let mut k = [0u8; 32];
        k[0] = 0x04;
        k[31] = 0xBB;
        k
    };
    let k_other = {
        let mut k = [0u8; 32];
        k[0] = 0x80;
        k[31] = 0xCC;
        k
    };
    assert_identical(
        &leaves_of(&[(k_keep, [1u8; 32]), (k_deep, [2u8; 32]), (k_other, [3u8; 32])]),
        1,
        "mixed-shortcut-and-deep-pair",
    );
}

// -- Contract-violation guards (debug builds) --

/// A descending stream violates the ascending contract and must panic in debug builds.
#[test]
#[should_panic(expected = "strictly ascending")]
fn build_rejects_unsorted() {
    let mut cap = Capture::default();
    let leaves =
        vec![(ResourceId::from([2u8; 32]), [1u8; 32]), (ResourceId::from([1u8; 32]), [2u8; 32])];
    build_sorted::<_, Sha256>(&mut cap, 1, leaves.into_iter());
}

/// Duplicate ids violate the uniqueness contract and must panic in debug builds.
#[test]
#[should_panic(expected = "strictly ascending")]
fn build_rejects_duplicates() {
    let mut cap = Capture::default();
    let id = ResourceId::from([1u8; 32]);
    let leaves = vec![(id, [1u8; 32]), (id, [2u8; 32])];
    build_sorted::<_, Sha256>(&mut cap, 1, leaves.into_iter());
}

/// Version 0 is reserved as pre-genesis.
#[test]
#[should_panic(expected = "version 0")]
fn build_rejects_version_zero() {
    let mut cap = Capture::default();
    let leaves = vec![(ResourceId::from([1u8; 32]), [1u8; 32])];
    build_sorted::<_, Sha256>(&mut cap, 0, leaves.into_iter());
}

// -- RocksDB integration --

/// The left child key of `key` (bit 0 at the current level).
fn left_child(key: &Key) -> Key {
    Key { level: key.level + 1, path: key.path }
}

/// The right child key of `key` (bit 1 at the current level).
fn right_child(key: &Key) -> Key {
    let mut path = key.path;
    path[..].set_msb(key.level as usize);
    Key { level: key.level + 1, path }
}

/// Collects every node reachable from the root as (encoded key -> encoded node).
///
/// On an empty store every written node is reachable from the root, so this enumerates the whole
/// `SmtNode` column family without depending on total-order CF iteration.
fn reachable_nodes(store: &RocksDbStore, version: u64) -> BTreeMap<[u8; 34], Vec<u8>> {
    let mut out = BTreeMap::new();
    let snapshot = store.snapshot();
    let mut stack = vec![Key::ROOT];
    while let Some(key) = stack.pop() {
        if let Some((_v, node)) = store.node(&key, version, &snapshot) {
            if let Node::Internal { .. } = node {
                stack.push(left_child(&key));
                stack.push(right_child(&key));
            }
            out.insert(key.encode(), node.encode());
        }
    }
    out
}

/// Builds via the builder and via `Updater`, each into its own RocksDB store, and asserts the roots
/// and the full reachable node sets match.
fn assert_identical_rocksdb(leaves: &[(ResourceId, [u8; 32])], version: u64, tag: &str) {
    let dir_o = TempDir::new().unwrap();
    let store_o = RocksDbStore::open(dir_o.path());
    let commitments = leaves.iter().map(|&(id, vh)| Commitment::new(id, vh)).collect();
    let mut wb_o = store_o.write_batch();
    let root_o = store_o.update(&mut wb_o, commitments, version);
    store_o.commit(wb_o);

    let dir_b = TempDir::new().unwrap();
    let store_b = RocksDbStore::open(dir_b.path());
    let mut wb_b = store_b.write_batch();
    let root_b = build_sorted::<_, Sha256>(&mut wb_b, version, leaves.iter().copied());
    store_b.commit(wb_b);

    assert_eq!(root_b, root_o, "root mismatch [{tag}]");
    assert_eq!(store_b.root(version), root_o, "persisted root mismatch [{tag}]");
    assert_eq!(
        reachable_nodes(&store_b, version),
        reachable_nodes(&store_o, version),
        "column-family mismatch [{tag}]"
    );
}

/// Real-store integration across random sets and edge cases.
#[test]
fn builder_matches_updater_on_rocksdb() {
    // Random sets of varying size.
    for seed in 0..24u64 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let n = (seed as usize % 12) * 15 + 1;
        let version = (seed % 5) + 1;
        let leaves = random_leaves(&mut rng, n);
        assert_identical_rocksdb(&leaves, version, &format!("rocksdb seed={seed} n={n}"));
    }

    // Edge cases mirrored on the real store.
    assert_identical_rocksdb(&leaves_of(&[([7u8; 32], [9u8; 32])]), 1, "rocksdb single");
    let mut last_bit = [0u8; 32];
    last_bit[31] = 1;
    assert_identical_rocksdb(
        &leaves_of(&[([0u8; 32], [1u8; 32]), (last_bit, [2u8; 32])]),
        1,
        "rocksdb differ-last-bit",
    );
    assert_identical_rocksdb(
        &leaves_of(&[([0u8; 32], [1u8; 32]), ([0xffu8; 32], [2u8; 32])]),
        1,
        "rocksdb min-max",
    );
}
