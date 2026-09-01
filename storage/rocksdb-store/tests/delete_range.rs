//! `delete_range` semantics over a real RocksDB store: half-open `[start, end)`, same-CF only.
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch as _};

#[test]
fn delete_range_is_half_open_and_range_limited() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    for k in ["a", "ab", "abc", "b"].map(|k| k.as_bytes()) {
        wb.put(StateSpace::Index, k, b"v");
    }
    store.commit(wb);

    // Delete keys in ["a", "abc") — removes "a" and "ab", keeps "abc" and "b".
    let mut wb = store.write_batch();
    wb.delete_range(StateSpace::Index, b"a", b"abc");
    store.commit(wb);

    assert_eq!(store.get(StateSpace::Index, b"a"), None);
    assert_eq!(store.get(StateSpace::Index, b"ab"), None);
    assert_eq!(store.get(StateSpace::Index, b"abc"), Some(vec![b'v']));
    assert_eq!(store.get(StateSpace::Index, b"b"), Some(vec![b'v']));
}

#[test]
fn delete_range_of_absent_keys_is_harmless() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    wb.put(StateSpace::Index, b"keep", b"v");
    wb.delete_range(StateSpace::Index, b"x", b"z");
    store.commit(wb);
    assert_eq!(store.get(StateSpace::Index, b"keep"), Some(vec![b'v']));
}
