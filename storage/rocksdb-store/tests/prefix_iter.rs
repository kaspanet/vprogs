use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch as _};

#[test]
fn prefix_iter_matches_the_full_prefix_despite_the_cf_extractor() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    // StateVersion carries an 8-byte fixed-prefix extractor; the keys below either share its
    // slice while diverging later, or are shorter than it.
    for k in [
        b"a".as_slice(),
        b"abcdefgh1",
        b"abcdefgh2",
        b"abcdefghi",
        b"prefix",
        b"prefix\x00",
        b"prefix\x01",
        b"prefix\xff",
        b"prefix2",
        b"z",
    ] {
        wb.put(StateSpace::StateVersion, k, b"v");
    }
    store.commit(wb);

    // A prefix longer than the extractor's slice matches only its exact keys.
    let keys8: Vec<Vec<u8>> =
        store.prefix_iter(StateSpace::StateVersion, b"abcdefgh1").map(|(k, _)| k).collect();
    assert_eq!(keys8, vec![b"abcdefgh1".to_vec()]);

    // A prefix shorter than the extractor's slice still matches by full bytes.
    let keys: Vec<Vec<u8>> =
        store.prefix_iter(StateSpace::StateVersion, b"prefix").map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec![
            b"prefix".to_vec(),
            b"prefix\x00".to_vec(),
            b"prefix\x01".to_vec(),
            b"prefix2".to_vec(),
            b"prefix\xff".to_vec(),
        ]
    );

    // Empty prefix iterates all keys in key order.
    let all: Vec<Vec<u8>> =
        store.prefix_iter(StateSpace::StateVersion, b"").map(|(k, _)| k).collect();
    assert_eq!(all.len(), 10);

    // Absent prefix returns an empty iterator.
    assert!(store.prefix_iter(StateSpace::StateVersion, b"nonexistent").next().is_none());
}
