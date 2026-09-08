use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch as _};

#[test]
fn range_iter_is_half_open_and_key_ordered() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    for k in ["a", "b", "c", "d", "e"] {
        wb.put(StateSpace::Index, k.as_bytes(), format!("val_{k}").as_bytes());
    }
    store.commit(wb);

    let items: Vec<(Vec<u8>, Vec<u8>)> = store.range_iter(StateSpace::Index, b"b", b"d").collect();
    assert_eq!(
        items,
        vec![(b"b".to_vec(), b"val_b".to_vec()), (b"c".to_vec(), b"val_c".to_vec()),]
    );
}

#[test]
fn range_iter_fixed_width_cursor_trick() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();

    let player = [7u8; 32];
    let event = 1u8;

    let k1 = [b"\x01".as_slice(), &player, &[event], &1u64.to_be_bytes(), &[10u8; 32]].concat();
    let k2 = [b"\x01".as_slice(), &player, &[event], &2u64.to_be_bytes(), &[20u8; 32]].concat();
    let k3 = [b"\x01".as_slice(), &player, &[event], &3u64.to_be_bytes(), &[30u8; 32]].concat();
    // Entry in next event
    let k_next_event =
        [b"\x01".as_slice(), &player, &[event + 1], &1u64.to_be_bytes(), &[10u8; 32]].concat();

    wb.put(StateSpace::Index, &k1, b"v1");
    wb.put(StateSpace::Index, &k2, b"v2");
    wb.put(StateSpace::Index, &k3, b"v3");
    wb.put(StateSpace::Index, &k_next_event, b"v_next");
    store.commit(wb);

    // Scan from beginning of (player, event): start = prefix || 0u64.be, end =
    // prefix_with_event_plus_1
    let start_all = [b"\x01".as_slice(), &player, &[event], &0u64.to_be_bytes()].concat();
    let end_all = [b"\x01".as_slice(), &player, &[event + 1]].concat();

    let all: Vec<_> = store.range_iter(StateSpace::Index, &start_all, &end_all).collect();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].0, k1);
    assert_eq!(all[1].0, k2);
    assert_eq!(all[2].0, k3);

    // Cursor pagination: after = k1, start = k1 || 0x00
    let start_after_k1 = [&k1[..], &[0x00]].concat();
    let after_k1: Vec<_> = store.range_iter(StateSpace::Index, &start_after_k1, &end_all).collect();
    assert_eq!(after_k1.len(), 2);
    assert_eq!(after_k1[0].0, k2);
    assert_eq!(after_k1[1].0, k3);

    // Cursor pagination: after = k3 (last item), should return empty
    let start_after_k3 = [&k3[..], &[0x00]].concat();
    let after_k3: Vec<_> = store.range_iter(StateSpace::Index, &start_after_k3, &end_all).collect();
    assert!(after_k3.is_empty());
}

#[test]
fn range_iter_empty_when_start_gte_end_or_absent() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    wb.put(StateSpace::Index, b"k", b"v");
    store.commit(wb);

    let empty1: Vec<_> = store.range_iter(StateSpace::Index, b"z", b"a").collect();
    assert!(empty1.is_empty());

    let empty2: Vec<_> = store.range_iter(StateSpace::Index, b"k", b"k").collect();
    assert!(empty2.is_empty());

    let empty3: Vec<_> =
        store.range_iter(StateSpace::Index, b"nonexistent1", b"nonexistent2").collect();
    assert!(empty3.is_empty());
}
