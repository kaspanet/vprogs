//! Zero-copy scan and point-lookup primitives for streaming reads (e.g. snapshot export),
//! which must not allocate per record.
//!
//! Consistency is per column family: a [`RawScanCursor`] pins one column family's view for its
//! whole lifetime, while `get_pinned` reads the live database at call time. An export that walks
//! one column family's cursor while fetching values from another must therefore run against a
//! quiesced or read-only store, so a concurrent commit cannot mix states from two points in
//! time.

use rocksdb::{ColumnFamily, DB, DBPinnableSlice, DBRawIteratorWithThreadMode};
use vprogs_storage_types::StateSpace;

use crate::{Config, RocksDbStore};

type RawIter<'a> = DBRawIteratorWithThreadMode<'a, DB>;

/// A raw forward cursor over one column family, yielding borrowed `&[u8]` key/value pairs straight
/// from RocksDB (no `to_vec`, no borsh decode).
///
/// The cursor pins a consistent read view of the column family for its whole lifetime, so the whole
/// scan reflects a single point in time. `key()`/`value()` borrow from the current position and are
/// invalidated by the next `next()`/`seek()` call.
pub struct RawScanCursor<'a> {
    /// The live raw iterator over the column family.
    iter: RawIter<'a>,
}

impl<'a> RawScanCursor<'a> {
    /// Opens a raw iterator over `cf`, positioned at the first key.
    fn new(db: &'a DB, cf: &'a ColumnFamily) -> Self {
        let mut read_opts = rocksdb::ReadOptions::default();
        // Fixed-prefix column families iterate in prefix mode by default, which RocksDB only
        // defines within one prefix; a scan (or seek) that must span prefixes needs total order,
        // mirroring `prefix_iter`'s empty-prefix path.
        read_opts.set_total_order_seek(true);
        let mut iter = db.raw_iterator_cf_opt(cf, read_opts);
        iter.seek_to_first();
        Self { iter }
    }

    /// Whether the cursor is positioned at a live key.
    ///
    /// # Panics
    ///
    /// Panics if the scan has failed with a RocksDB error (corruption, I/O failure), matching
    /// the store's panic-on-error convention: without this, the natural
    /// `while cursor.valid()` loop silently truncates on error.
    pub fn valid(&self) -> bool {
        if self.iter.valid() {
            return true;
        }
        if let Err(e) = self.status() {
            panic!("rocksdb raw scan failed: {e}");
        }
        false
    }

    /// Borrowed key at the current position, or `None` if the cursor is invalid/exhausted. Valid
    /// until the next `next()`/`seek()` call.
    pub fn key(&self) -> Option<&[u8]> {
        self.iter.key()
    }

    /// Borrowed value at the current position, or `None` if the cursor is invalid/exhausted. Valid
    /// until the next `next()`/`seek()` call.
    pub fn value(&self) -> Option<&[u8]> {
        self.iter.value()
    }

    /// Advances to the next key.
    // Can't be `Iterator::next`: this advances the cursor and returns `()`, mirroring rocksdb's
    // raw-iterator API. The lint fires on the name alone.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) {
        self.iter.next();
    }

    /// Seeks to the first key `>= key`.
    pub fn seek(&mut self, key: &[u8]) {
        self.iter.seek(key);
    }

    /// The underlying iterator's status; `Err` if a scan error occurred. [`valid`](Self::valid)
    /// already panics on a failed scan, so this serves callers that prefer an explicit check.
    pub fn status(&self) -> Result<(), rocksdb::Error> {
        self.iter.status()
    }
}

impl<C: Config> RocksDbStore<C> {
    /// Opens a raw cursor over `ns`, positioned at the first key. Yields borrowed key/value slices
    /// directly from RocksDB for zero-copy forward scans (e.g. streaming a resource-id-sorted CF
    /// like `latest_ptr`).
    pub fn raw_scan(&self, ns: StateSpace) -> RawScanCursor<'_> {
        RawScanCursor::new(&self.db, self.cf(&ns))
    }

    /// Zero-copy point lookup: the value bytes for `key` in `ns`, borrowed straight from RocksDB
    /// with no heap copy, or `None` if absent. Panics on a rocksdb error, matching `get`.
    pub fn get_pinned(&self, ns: StateSpace, key: &[u8]) -> Option<DBPinnableSlice<'_>> {
        match self.db.get_pinned_cf(self.cf(&ns), key) {
            Ok(res) => res,
            Err(e) => panic!("rocksdb get_pinned failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use vprogs_storage_types::{Store, WriteBatch as _};

    use super::*;
    use crate::DefaultConfig;

    // `StateSpace` has no `Copy`/`Clone` impl, so tests construct it fresh at each use site via
    // this helper rather than binding and reusing a single value.
    fn ns() -> StateSpace {
        StateSpace::StatePtrLatest
    }

    fn seed(store: &RocksDbStore<DefaultConfig>, pairs: &[(u8, u8)]) {
        let mut wb = store.write_batch();
        for &(k, v) in pairs {
            wb.put(ns(), &[k], &[v]);
        }
        store.commit(wb);
    }

    /// The cursor walks the same key/value sequence as a plain raw iterator across a forward scan
    /// and re-seeks, including landing on the next-greater key for a missing seek target and going
    /// invalid past the end.
    #[test]
    fn matches_plain_raw_iterator() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path());

        let pairs: Vec<(u8, u8)> = (0..12u8).map(|i| (i * 2, i + 100)).collect();
        seed(&store, &pairs);

        let cf = store.cf(&ns());
        let mut raw = store.db.raw_iterator_cf(cf);
        raw.seek_to_first();
        let mut cursor = store.raw_scan(ns());

        let both = |raw: &RawIter<'_>, cur: &RawScanCursor<'_>| {
            assert_eq!(raw.valid(), cur.valid());
            assert_eq!(raw.key().map(<[u8]>::to_vec), cur.key().map(<[u8]>::to_vec));
            assert_eq!(raw.value().map(<[u8]>::to_vec), cur.value().map(<[u8]>::to_vec));
        };

        both(&raw, &cursor);
        for _ in 0..6 {
            raw.next();
            cursor.next();
            both(&raw, &cursor);
        }

        // 7 is missing; both land on the next-greater key, 8.
        raw.seek([7]);
        cursor.seek(&[7]);
        both(&raw, &cursor);
        assert_eq!(cursor.key(), Some([8u8].as_slice()));

        // 99 is past every key; both go invalid.
        raw.seek([99]);
        cursor.seek(&[99]);
        both(&raw, &cursor);
        assert!(!cursor.valid());
        assert!(cursor.status().is_ok());
    }

    /// A store configuration enabling a memtable prefix bloom on a fixed-prefix column
    /// family: the configuration under which RocksDB's default prefix-mode iteration is
    /// allowed to skip keys across prefix boundaries.
    struct BloomConfig;

    impl Config for BloomConfig {
        fn cf_smt_node_opts() -> rocksdb::Options {
            let mut opts = <DefaultConfig as Config>::cf_smt_node_opts();
            opts.set_memtable_prefix_bloom_ratio(0.1);
            opts
        }
    }

    /// A full scan over a fixed-prefix column family must yield every key across prefixes, in
    /// order, even with a memtable prefix bloom active: the freshly written (memtable-resident)
    /// keys are exactly where a prefix-mode iterator may skip.
    #[test]
    fn scan_spans_prefixes_on_prefix_extractor_cf() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<BloomConfig>::open(dir.path());

        // 37-byte keys in three distinct 34-byte prefix groups, ascending overall.
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for group in [0x00u8, 0x7fu8, 0xffu8] {
            for i in 0..5u8 {
                let mut key = vec![group; 34];
                key.extend_from_slice(&[i, 0xAB, 0xCD]);
                keys.push(key);
            }
        }
        keys.sort();
        let mut wb = store.write_batch();
        for key in &keys {
            wb.put(StateSpace::SmtNode, key, b"v");
        }
        store.commit(wb);

        let mut cursor = store.raw_scan(StateSpace::SmtNode);
        let mut got = Vec::new();
        while cursor.valid() {
            got.push(cursor.key().expect("valid cursor has a key").to_vec());
            cursor.next();
        }
        assert_eq!(got, keys, "full scan must span every prefix group");

        // A seek into a later prefix must land within the total order, not skip to its end.
        let mut mid = vec![0x7fu8; 34];
        mid.extend_from_slice(&[2, 0x00, 0x00]);
        cursor.seek(&mid);
        let mut after = Vec::new();
        while cursor.valid() {
            after.push(cursor.key().expect("valid cursor has a key").to_vec());
            cursor.next();
        }
        let expected: Vec<Vec<u8>> =
            keys.iter().filter(|k| k.as_slice() >= mid.as_slice()).cloned().collect();
        assert_eq!(after, expected, "seek must land within the total order");
    }

    #[test]
    fn get_pinned_returns_borrowed_value_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path());

        let mut wb = store.write_batch();
        wb.put(ns(), b"present", b"the-value");
        store.commit(wb);

        let value = store.get_pinned(ns(), b"present").expect("value should be present");
        assert_eq!(&*value, b"the-value");

        assert!(store.get_pinned(ns(), b"missing").is_none());
    }
}
