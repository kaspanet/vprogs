use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
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

/// A read that records being served: the at-risk shape, since a read's receipt latch opens
/// only once the worker executes it, so a read dropped at shutdown wedges its awaiter.
struct TrackedRead {
    /// Set by `exec`, standing in for the receipt latch.
    served: Arc<AtomicBool>,
}

impl ReadCmd for TrackedRead {
    fn exec<S: vprogs_storage_types::ReadStore>(&self, _store: &S) {
        self.served.store(true, Ordering::Release);
    }
}

/// A no-op write command; the drain under test never touches the write worker.
struct NoWrite;

impl WriteCmd for NoWrite {
    fn exec<S: Store>(&self, _store: &S, batch: S::WriteBatch) -> S::WriteBatch {
        batch
    }

    fn flush_now(&self) -> bool {
        false
    }

    fn flushed(self) {}
}

/// A gate that holds its callers while closed, and tells whether one is held.
#[derive(Default)]
struct Gate {
    /// Whether the gate is closed, and how many callers are inside [`wait`](Self::wait).
    state: Mutex<(bool, usize)>,
    /// Signalled when the gate opens and when a caller arrives.
    changed: Condvar,
}

impl Gate {
    /// Closes the gate; subsequent [`wait`](Self::wait) callers block until [`open`](Self::open).
    fn close(&self) {
        self.state.lock().expect("gate poisoned").0 = true;
    }

    /// Opens the gate and releases every held caller.
    fn open(&self) {
        self.state.lock().expect("gate poisoned").0 = false;
        self.changed.notify_all();
    }

    /// Blocks while the gate is closed.
    fn wait(&self) {
        let mut state = self.state.lock().expect("gate poisoned");
        state.1 += 1;
        self.changed.notify_all();
        while state.0 {
            state = self.changed.wait(state).expect("gate poisoned");
        }
        state.1 -= 1;
    }

    /// Waits until a caller is held at the closed gate, and returns whether one is.
    fn wait_held(&self) -> bool {
        let state = self.state.lock().expect("gate poisoned");
        let held = |state: &(bool, usize)| state.0 && state.1 > 0;
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(30), |state| !held(state))
            .expect("gate poisoned");
        held(&state)
    }
}

/// A read that is held at a gate and then submits a write, as a scheduler read does when it
/// resolves the last access of a resource and so completes a state diff.
struct SubmittingRead {
    /// Holds the read inside its execution.
    gate: Arc<Gate>,
    /// The manager the write is submitted to.
    manager: StorageManager<RocksDbStore, SubmittingRead, TrackedPut>,
    /// Handed to the write, which sets it once flushed.
    committed: Arc<AtomicBool>,
}

impl ReadCmd for SubmittingRead {
    fn exec<S: vprogs_storage_types::ReadStore>(&self, _store: &S) {
        self.gate.wait();
        self.manager.submit_write(TrackedPut { key: b'r', committed: self.committed.clone() });
    }
}

/// Reads still queued when the worker exits must be served: the shutdown drain executes
/// them, so the receipt latch every awaiter parks on always opens.
#[test]
fn reads_queued_before_shutdown_are_served() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = StorageConfig::default().with_store(RocksDbStore::open(dir.path()));
    let manager = StorageManager::<RocksDbStore, TrackedRead, NoWrite>::new(config);

    let reads: Vec<_> = (0..3)
        .map(|_| {
            let served = Arc::new(AtomicBool::new(false));
            manager.submit_read(TrackedRead { served: served.clone() });
            served
        })
        .collect();

    manager.shutdown();

    for (id, served) in reads.iter().enumerate() {
        assert!(
            served.load(Ordering::Acquire),
            "read {id} queued before shutdown was never served",
        );
    }
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

/// A write that a read submits while the manager shuts down must commit: the readers are
/// stopped and joined before the write worker is, so the worker is still there to take it.
/// With one flag for both, the write worker drained and left while a read was still being
/// served, and the write stayed in the queue for good.
///
/// The one timing assumption is that a write worker that stops together with the readers has
/// done so before the read is released. If it had not, it would still take the write, so the
/// test could pass on the previous code but cannot fail for it.
#[test]
fn a_write_submitted_by_a_read_during_shutdown_commits() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = StorageConfig::default().with_store(RocksDbStore::open(dir.path()));
    let manager = StorageManager::<RocksDbStore, SubmittingRead, TrackedPut>::new(config);

    // Hold a read inside its execution.
    let gate = Arc::new(Gate::default());
    gate.close();
    let committed = Arc::new(AtomicBool::new(false));
    manager.submit_read(SubmittingRead {
        gate: gate.clone(),
        manager: manager.clone(),
        committed: committed.clone(),
    });
    assert!(gate.wait_held(), "the read never started");

    // Shutdown waits for the held read, so it runs on a thread of its own.
    let shutdown = thread::spawn({
        let manager = manager.clone();
        move || manager.shutdown()
    });
    thread::sleep(Duration::from_millis(250));
    gate.open();
    shutdown.join().expect("shutdown panicked");

    assert!(
        committed.load(Ordering::Acquire),
        "the write a read submitted during shutdown never ran its flushed callback",
    );
    assert_eq!(
        manager.store().get(StateSpace::Index, b"r"),
        Some(b"committed".to_vec()),
        "the write a read submitted during shutdown never committed",
    );
}
