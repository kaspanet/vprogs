use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use vprogs_storage_manager::{ReadCmd, StorageConfig, StorageManager, WriteCmd, WriteConfig};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{StateSpace, Store, WriteBatch as _};

/// A write carrying the flush-now flag that records its post-commit callback.
struct FlushNowPut {
    /// Key the write lands under.
    key: u8,
    /// Set by `flushed`, standing in for the confirmation latch.
    committed: Arc<AtomicBool>,
}

impl WriteCmd for FlushNowPut {
    fn exec<S: Store>(&self, _store: &S, mut batch: S::WriteBatch) -> S::WriteBatch {
        batch.put(StateSpace::Index, &[self.key], b"committed");
        batch
    }

    fn flush_now(&self) -> bool {
        true
    }

    fn flushed(self) {
        self.committed.store(true, Ordering::Release);
    }
}

/// A no-op read command; the test never touches the read worker.
struct NoRead;

impl ReadCmd for NoRead {
    fn exec<S: vprogs_storage_types::ReadStore>(&self, _store: &S) {}
}

/// A flush-now write commits on arrival: the stored value and the confirmation callback both
/// land without waiting out the batch window, with the thresholds sitting far beyond the
/// test's single write.
#[test]
fn flush_now_write_commits_without_the_batch_window() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = StorageConfig::default()
        .with_write_config(
            WriteConfig::default()
                .with_batch_size(1000)
                .with_batch_duration(Duration::from_secs(600)),
        )
        .with_store(RocksDbStore::open(dir.path()));
    let manager = StorageManager::<RocksDbStore, NoRead, FlushNowPut>::new(config);

    let committed = Arc::new(AtomicBool::new(false));
    manager.submit_write(FlushNowPut { key: b'k', committed: committed.clone() });

    // Only the flush-now path can commit inside this deadline; the window is 600 s.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !committed.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "flush-now write never committed");
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        manager.store().get(StateSpace::Index, b"k"),
        Some(b"committed".to_vec()),
        "flush-now write's value never landed",
    );

    manager.shutdown();
}
