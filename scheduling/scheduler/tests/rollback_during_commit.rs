//! Pins that a rollback leaves its target as the last committed batch on disk, also when a
//! batch above the target was canceled while its commit was already being written.
//!
//! A batch's commit checks for cancellation when it executes and again in its callback after
//! the flush. A rollback that cancels the batch in between lets the flush write the batch as
//! the last committed one, while the callback skips the in-memory update. The rollback used to
//! compare its target with the in-memory value, found nothing to correct, and left the disk
//! pointing at the canceled batch.

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
use vprogs_storage_manager::{StorageConfig, WriteConfig};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};

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
/// inside a flush. Every other operation delegates unchanged.
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

/// Tests that the disk names the rollback target as the last committed batch after a batch
/// above it was canceled in the middle of its commit.
///
/// Batch 1 commits. Then every store commit is held, so batch 2's commit executes, passes its
/// cancellation check and stops inside its flush. The rollback to batch 1 cancels batch 2
/// there. Once released, the flush writes batch 2 as the last committed batch, and the
/// rollback, which runs after it, must set the pointer back.
///
/// The write worker's batch window is set far beyond the test, so batch 2's state diff is not
/// flushed by age and the held flush is the one of its commit. The one timing assumption left
/// is that the commit has reached that flush when the rollback starts. If it has not, the
/// commit sees the cancellation and writes nothing, which was never wrong, so the test could
/// pass without reaching the case but cannot fail for it.
#[test]
fn a_rollback_resets_the_last_committed_batch_on_disk() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let gate = Arc::new(Gate::default());
    let storage =
        GatedStore { inner: RocksDbStore::open(temp_dir.path()), gate: Arc::clone(&gate) };
    let write_config = WriteConfig::default().with_batch_duration(Duration::from_secs(600));
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(Processor),
        StorageConfig::default().with_write_config(write_config).with_store(storage),
    );
    scheduler.schedule(1, batch_txs(1)).wait_committed_blocking();

    // Hold every commit from here on and let batch 2's commit run into its flush.
    gate.close();
    scheduler.schedule(2, batch_txs(2)).wait_processed_blocking();
    thread::sleep(Duration::from_millis(250));

    // The rollback cancels batch 2 and then waits for its own write, which is queued behind
    // the held flush, so the gate has to open from another thread.
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        gate.open();
    });
    scheduler.rollback_to(1).expect("rollback to batch 1");
    release.join().expect("release thread panicked");

    let store = scheduler.state().storage().store();
    assert_eq!(
        StateMetadata::last_committed::<u64, _>(store.as_ref()).index(),
        1,
        "the disk still names the canceled batch as the last committed one"
    );
    assert_eq!(scheduler.state().last_committed().index(), 1);

    scheduler.shutdown();
}
