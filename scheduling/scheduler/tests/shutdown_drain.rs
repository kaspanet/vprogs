//! Pins that batch commits still queued at shutdown build on each other.
//!
//! A batch commit computes its tree update from the store, not from the write batch it is added
//! to. While the write worker runs, it flushes after every commit, so the next commit reads what
//! the one before wrote. The shutdown drain used to execute everything still queued into one
//! write batch and flush once at the end. A second commit in the drain then built its tree on
//! the state before the first: both commits landed, and the root stored for the last batch was
//! not the root of the state.

use std::{
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

use tempfile::TempDir;
use vprogs_core_smt::{Key, Node, Tree, WriteBatch as SmtWriteBatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_utils::Processor;
use vprogs_state_metadata::StateMetadata;
use vprogs_storage_canonical_chain::CanonicalChain;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};

/// Number of batches each run commits.
const BATCHES: u64 = 4;

/// A latch that holds a caller while closed.
#[derive(Default)]
struct Gate {
    /// True while callers of [`wait`](Self::wait) are held.
    closed: Mutex<bool>,
    /// Signalled when the gate opens.
    opened: Condvar,
}

impl Gate {
    /// Closes the gate; subsequent [`wait`](Self::wait) callers block until [`open`](Self::open).
    fn close(&self) {
        *self.closed.lock().expect("gate poisoned") = true;
    }

    /// Opens the gate and releases every held caller.
    fn open(&self) {
        *self.closed.lock().expect("gate poisoned") = false;
        self.opened.notify_all();
    }

    /// Blocks while the gate is closed.
    fn wait(&self) {
        let mut closed = self.closed.lock().expect("gate poisoned");
        while *closed {
            closed = self.opened.wait(closed).expect("gate poisoned");
        }
    }
}

/// A [`Store`] whose commits are held while its gate is closed, which keeps the write worker
/// inside a flush while later commands queue up behind it. Every other operation delegates
/// unchanged.
#[derive(Clone)]
struct GatedStore {
    /// The store every operation delegates to.
    inner: RocksDbStore,
    /// Held commits wait here.
    gate: Arc<Gate>,
}

impl Store for GatedStore {
    type WriteBatch = <RocksDbStore as Store>::WriteBatch;

    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.get(state_space, key)
    }

    fn write_batch(&self) -> Self::WriteBatch {
        self.inner.write_batch()
    }

    fn commit(&self, write_batch: Self::WriteBatch) {
        self.gate.wait();
        self.inner.commit(write_batch);
    }

    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter(state_space, prefix)
    }

    fn canonical_chain(&self) -> CanonicalChain {
        self.inner.canonical_chain()
    }

    fn prefix_iter_rev(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter_rev(state_space, prefix)
    }

    fn range_iter(&self, state_space: StateSpace, start: &[u8], end: &[u8]) -> PrefixIterator<'_> {
        self.inner.range_iter(state_space, start, end)
    }
}

impl Tree for GatedStore {
    type Hasher = <RocksDbStore as Tree>::Hasher;
    type Snapshot = <RocksDbStore as Tree>::Snapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.inner.snapshot()
    }

    fn node(&self, key: &Key, max_version: u64, snapshot: &Self::Snapshot) -> Option<(u64, Node)> {
        self.inner.node(key, max_version, snapshot)
    }

    fn prune(&self, wb: &mut impl SmtWriteBatch, version: u64) {
        self.inner.prune(wb, version)
    }

    fn prune_orphaned(&self, wb: &mut impl SmtWriteBatch, version: u64) {
        self.inner.prune_orphaned(wb, version)
    }
}

/// The transactions of batch `index`: one write to a resource of its own.
fn batch_txs(index: u64) -> Vec<SchedulerTransaction<usize>> {
    let resource = ResourceId::for_test(index as usize);
    vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(resource)], index as usize)]
}

/// The root after all batches, from a run that commits them one by one.
fn expected_root() -> [u8; 32] {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(Processor),
        StorageConfig::default().with_store(storage),
    );
    for index in 1..=BATCHES {
        scheduler.schedule(index, batch_txs(index)).wait_committed_blocking();
    }
    let root = scheduler.state().storage().store().root(BATCHES);
    scheduler.shutdown();
    root
}

/// Tests that batch commits executed by the shutdown drain leave the same tree as commits
/// flushed one by one.
///
/// Batch 1 commits. Then every store commit is held, so the write worker stops inside its next
/// flush while the commits of the batches after it queue up. Shutdown is flagged while it is
/// held, so once released the worker leaves its loop after at most one more command and the
/// drain executes the rest: at least the commits of the last two batches, whichever flush was
/// the held one.
///
/// The one timing assumption is that the gate opens after shutdown was flagged. If it opened
/// before, the worker would run the commits in its loop, which was never wrong, so the test
/// could pass without reaching the drain but cannot fail for it.
#[test]
fn commits_drained_at_shutdown_build_on_each_other() {
    let expected = expected_root();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let gate = Arc::new(Gate::default());
    {
        let storage =
            GatedStore { inner: RocksDbStore::open(temp_dir.path()), gate: Arc::clone(&gate) };
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );
        scheduler.schedule(1, batch_txs(1)).wait_committed_blocking();

        // Hold every commit from here on. Execution only reads, so the batches still process,
        // and their commits pile up behind the flush the write worker is held in.
        gate.close();
        for index in 2..=BATCHES {
            scheduler.schedule(index, batch_txs(index)).wait_processed_blocking();
        }

        // The lifecycle worker submitted each commit when its batch was processed. Shutdown stops
        // it, then the storage manager: its idle readers first, then the held write worker, which
        // it flags and waits for.
        let shutdown = thread::spawn(move || scheduler.shutdown());
        thread::sleep(Duration::from_millis(250));
        gate.open();
        shutdown.join().expect("shutdown panicked");
    }

    let store: RocksDbStore = RocksDbStore::open(temp_dir.path());
    assert_eq!(
        StateMetadata::last_committed::<u64, _>(&store).index(),
        BATCHES,
        "every queued commit lands at shutdown"
    );
    assert_eq!(
        store.root(BATCHES),
        expected,
        "a commit in the shutdown drain built its tree on the state before the one ahead of it"
    );
}
