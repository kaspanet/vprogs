//! Round-trip of the frozen canonical-bit rows through the store's column family.

use tempfile::TempDir;
use vprogs_storage_canonical_chain::FrozenBits;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store};

/// Reads every persisted row back as `(bucket, words)`, in bucket order.
fn read_rows(store: &RocksDbStore) -> Vec<(u64, [u64; 2])> {
    store
        .prefix_iter(StateSpace::CanonicalBits, &[])
        .map(|(key, value)| {
            let bucket = u64::from_be_bytes(key[..8].try_into().unwrap());
            let words = [
                u64::from_be_bytes(value[..8].try_into().unwrap()),
                u64::from_be_bytes(value[8..16].try_into().unwrap()),
            ];
            (bucket, words)
        })
        .collect()
}

/// Persisted rows read back identical, keyed by bucket number.
#[test]
fn frozen_bits_round_trip() {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());

    let rows = [
        FrozenBits { bucket: 38, words: [0x00ff_ffff_ffff_ffff, 0] },
        FrozenBits { bucket: 39, words: [0, 0xffff] },
    ];
    store.persist_frozen_bits(&rows);
    assert_eq!(read_rows(&store), vec![(38, [0x00ff_ffff_ffff_ffff, 0]), (39, [0, 0xffff])]);

    // A later step overwrites the same bucket's row and leaves the other intact.
    store.persist_frozen_bits(&[FrozenBits { bucket: 39, words: [u64::MAX, u64::MAX] }]);
    assert_eq!(
        read_rows(&store),
        vec![(38, [0x00ff_ffff_ffff_ffff, 0]), (39, [u64::MAX, u64::MAX])]
    );
}
