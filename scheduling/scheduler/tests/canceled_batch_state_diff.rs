//! Pins that a queued state diff keeps its batch alive until the write worker has written it.
//!
//! A state diff reaches its batch only weakly, and the queued write used to hold nothing else.
//! A canceled batch gets no commit, and once its execution has ended the scheduler and its
//! workers let go of it. When the write worker reached one of its diffs after that, it
//! panicked. No later write ran, not even the rollback that had canceled the batch, which
//! waited for its done signal forever.

use std::{
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use vprogs_core_smt::{Key, Node, Tree, WriteBatch as SmtWriteBatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_state_metadata::StateMetadata;
use vprogs_state_version::StateVersion;
use vprogs_storage_canonical_chain::CanonicalChain;
use vprogs_storage_manager::{StorageConfig, WriteConfig};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};

/// The transaction id parked by [`ParkingProcessor`].
const GATED_TX: usize = 99;

/// How long the test waits for a step that must happen.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How long the test gives a canceled batch to be dropped, which must not happen.
const GRACE: Duration = Duration::from_secs(1);

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
            .wait_timeout_while(state, TIMEOUT, |state| !held(state))
            .expect("gate poisoned");
        held(&state)
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

/// A processor that appends the transaction id to every resource it writes, and holds
/// [`GATED_TX`] at its gate, which keeps that batch in flight.
#[derive(Clone)]
struct ParkingProcessor {
    /// The gate [`GATED_TX`] is held at, shared with the test thread that opens it.
    gate: Arc<Gate>,
}

impl<S: Store> Processor<S> for ParkingProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let tx_id = ctx.scheduler_tx().tx;
        if tx_id == GATED_TX {
            self.gate.wait();
        }
        for resource in ctx.resources_mut() {
            if resource.access_metadata().access_type == AccessType::Write {
                resource.data_mut().extend_from_slice(&tx_id.to_be_bytes());
            }
        }
        Ok(())
    }

    fn tx_image_id(&self) -> [u8; 32] {
        [0u8; 32]
    }

    fn batch_image_id(&self) -> [u8; 32] {
        [1u8; 32]
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = u64;
    type Error = ();
}

/// A transaction `tx` that writes resource `resource`.
fn write(resource: usize, tx: usize) -> SchedulerTransaction<usize> {
    let access = vec![AccessMetadata::write(ResourceId::for_test(resource))];
    SchedulerTransaction::new(0, access, tx)
}

/// A transaction `tx` that reads resource `resource`.
fn read(resource: usize, tx: usize) -> SchedulerTransaction<usize> {
    let access = vec![AccessMetadata::read(ResourceId::for_test(resource))];
    SchedulerTransaction::new(1, access, tx)
}

/// Polls until `condition` holds.
#[track_caller]
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Tests that a batch a rollback canceled stays alive while one of its state diffs is queued,
/// that the rollback completes, and that the write worker goes on.
///
/// Batch 1 commits. Then every store commit is held, so the write worker stops inside the
/// flush of batch 2's commit. Batch 3 writes a resource and holds a transaction that reads it:
/// the resource's diff is submitted before that transaction starts, so it is queued behind the
/// held flush. The rollback to batch 1 cancels batch 3, and its held transaction is released.
/// From then on only the queued diff holds the batch. Then the flush is released and the write
/// worker reaches the diff.
///
/// The one timing assumption is the time the scheduler's workers get to let go of the batch
/// before the test checks that it is still alive. If they took longer, the check would pass
/// without the diff being what holds the batch, so the test could pass on the previous code
/// but cannot fail for it.
#[test]
fn a_queued_state_diff_keeps_its_canceled_batch_alive() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (commits, execution) = (Arc::new(Gate::default()), Arc::new(Gate::default()));
    execution.close();
    let storage = GatedStore { inner: RocksDbStore::open(temp_dir.path()), gate: commits.clone() };
    // A batch window far beyond the test, so that only a commit or a rollback flushes.
    let write_config = WriteConfig::default().with_batch_duration(Duration::from_secs(600));
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(ParkingProcessor { gate: execution.clone() }),
        StorageConfig::default().with_write_config(write_config).with_store(storage),
    );
    scheduler.schedule(1, vec![write(1, 1)]).wait_committed_blocking();

    // Hold the write worker inside the flush of batch 2's commit.
    commits.close();
    scheduler.schedule(2, vec![write(2, 2)]);
    assert!(commits.wait_held(), "the commit of batch 2 never reached its flush");

    // Queue the diff of batch 3 behind that flush, and keep only a weak handle on the batch.
    let batch = scheduler.schedule(3, vec![write(3, 3), read(3, GATED_TX)]).downgrade();
    assert!(execution.wait_held(), "batch 3 never reached its held transaction");

    // The rollback cancels batches 2 and 3 and then waits for its own write, which is queued
    // behind the diff.
    let (done, rolled_back) = mpsc::channel();
    thread::spawn(move || {
        let result = scheduler.rollback_to(1);
        let _ = done.send((scheduler, result));
    });
    wait_until("the rollback has canceled batch 3", || {
        batch.upgrade().is_none_or(|batch| batch.canceled())
    });

    // The canceled batch ends its execution, and the scheduler's workers let go of it.
    execution.open();
    thread::sleep(GRACE);
    assert!(batch.upgrade().is_some(), "nothing held the batch while its diff was still queued");

    // The write worker finishes the flush and reaches the diff of the canceled batch.
    commits.open();
    let (mut scheduler, result) = rolled_back
        .recv_timeout(TIMEOUT)
        .expect("the rollback never returned: the write worker is gone");
    result.expect("rollback to batch 1");

    // Nothing of the canceled batch was written, and the write worker still serves commits.
    let store = scheduler.state().storage().store();
    assert!(
        StateVersion::get(&**store, 3, &ResourceId::for_test(3)).is_none(),
        "the write of the canceled batch landed"
    );
    assert_eq!(StateMetadata::last_committed::<u64, _>(store.as_ref()).index(), 1);
    scheduler.schedule(4, vec![write(4, 4)]).wait_committed_blocking();
    assert_eq!(scheduler.state().last_committed().index(), 4);

    // The write is done with the diff, so the batch is released.
    wait_until("batch 3 is dropped", || batch.upgrade().is_none());

    scheduler.shutdown();
}
