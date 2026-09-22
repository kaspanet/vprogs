use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use vprogs_storage_manager::{ReadCmd, StorageConfig, StorageManager, WriteCmd, WriteConfig};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch as _};

/// A batched write that records its post-commit callback: the at-risk shape, since a batched
/// write's confirmation latch opens only once its batch commits, so a dropped shutdown flush
/// both loses the data and wedges the latch's awaiter.
struct TrackedPut {
    /// Key the write lands under.
    key: u8,
    /// Set by `flushed`, standing in for the confirmation latch.
    committed: Arc<AtomicBool>,
}

impl WriteCmd for TrackedPut {
    fn exec<S: Store>(&self, _store: &S, mut batch: S::WriteBatch) -> S::WriteBatch {
        batch.put(StateSpace::Index, &[self.key], b"committed");
        batch
    }

    fn flush_now(&self) -> bool {
        false
    }

    fn flushed(self) {
        self.committed.store(true, Ordering::Release);
    }
}

/// A no-op read command; the drain under test never touches the read worker.
struct NoRead;

impl ReadCmd for NoRead {
    fn exec<S: vprogs_storage_types::ReadStore>(&self, _store: &S) {}
}

/// Writes still queued or accumulated when the worker exits must commit: the shutdown drain
/// flushes them, so both the stored value and the write's confirmation callback land. The batch
/// thresholds sit far beyond the test's writes, so the drain is the only path to a commit.
#[test]
fn batched_writes_queued_before_shutdown_commit() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = StorageConfig::default()
        .with_write_config(
            WriteConfig::default()
                .with_batch_size(1000)
                .with_batch_duration(Duration::from_secs(600)),
        )
        .with_store(RocksDbStore::open(dir.path()));
    let manager = StorageManager::<RocksDbStore, NoRead, TrackedPut>::new(config);

    let writes: Vec<_> = (b'a'..b'd')
        .map(|key| {
            let committed = Arc::new(AtomicBool::new(false));
            manager.submit_write(TrackedPut { key, committed: committed.clone() });
            committed
        })
        .collect();

    manager.shutdown();

    let store = manager.store();
    for (key, committed) in (b'a'..b'd').zip(&writes) {
        assert!(
            committed.load(Ordering::Acquire),
            "write under key {key} queued before shutdown never ran its flushed callback",
        );
        assert_eq!(
            store.get(StateSpace::Index, &[key]),
            Some(b"committed".to_vec()),
            "write under key {key} queued before shutdown never committed",
        );
    }
}
