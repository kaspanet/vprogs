use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch as _};

#[test]
fn prefix_iter_rev_walks_newest_first_within_prefix() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    for k in ["p\0\0\0\0\0\0\0a", "p\0\0\0\0\0\0\0b", "q\0\0\0\0\0\0\0a"] {
        wb.put(StateSpace::Index, k.as_bytes(), b"v");
    }
    store.commit(wb);

    let keys: Vec<Vec<u8>> =
        store.prefix_iter_rev(StateSpace::Index, b"p").map(|(k, _)| k).collect();
    assert_eq!(keys, vec![b"p\0\0\0\0\0\0\0b".to_vec(), b"p\0\0\0\0\0\0\0a".to_vec()]);
}

#[test]
fn prefix_iter_rev_edge_cases() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    // Keys before prefix, inside prefix, and after prefix.
    for k in
        [b"a".as_slice(), b"prefix", b"prefix\x00", b"prefix\x01", b"prefix\xff", b"prefix2", b"z"]
    {
        wb.put(StateSpace::Index, k, b"v");
    }
    store.commit(wb);

    // Prefix "prefix" should match "prefix", "prefix\x00", "prefix\x01", "prefix\xff", "prefix2" in
    // reverse order.
    let keys: Vec<Vec<u8>> =
        store.prefix_iter_rev(StateSpace::Index, b"prefix").map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        vec![
            b"prefix\xff".to_vec(),
            b"prefix2".to_vec(),
            b"prefix\x01".to_vec(),
            b"prefix\x00".to_vec(),
            b"prefix".to_vec(),
        ]
    );

    // Prefix with trailing 0xFF byte: "prefix\xff"
    let keys_ff: Vec<Vec<u8>> =
        store.prefix_iter_rev(StateSpace::Index, b"prefix\xff").map(|(k, _)| k).collect();
    assert_eq!(keys_ff, vec![b"prefix\xff".to_vec()]);

    // Absent prefix returns empty iterator.
    let empty: Vec<Vec<u8>> =
        store.prefix_iter_rev(StateSpace::Index, b"nonexistent").map(|(k, _)| k).collect();
    assert!(empty.is_empty());

    // Empty prefix iterates all keys in reverse order.
    let all_keys: Vec<Vec<u8>> =
        store.prefix_iter_rev(StateSpace::Index, b"").map(|(k, _)| k).collect();
    assert_eq!(
        all_keys,
        vec![
            b"z".to_vec(),
            b"prefix\xff".to_vec(),
            b"prefix2".to_vec(),
            b"prefix\x01".to_vec(),
            b"prefix\x00".to_vec(),
            b"prefix".to_vec(),
            b"a".to_vec(),
        ]
    );
}
