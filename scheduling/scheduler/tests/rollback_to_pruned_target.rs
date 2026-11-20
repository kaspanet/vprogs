//! Pins that a rollback or a re-root is refused when its target is a batch the pruning worker
//! has reached, or is not a batch on the canonical chain.
//!
//! The pruning handshake compared a rollback target with the last batch a pass had pruned, and
//! accepted the target when the two were equal, although a pass deletes the metadata of every
//! batch in its range. The scheduler then found no metadata for the target, took the default in
//! its place and persisted that as the last committed batch, with the canonical tip below the
//! root. While the pass was still in flight the rollback even found the metadata, and the pass
//! deleted it afterwards. The default also stood in for the id of a canceled batch, and a batch
//! that had been reorged away was accepted as a target with the metadata it kept.

use std::{
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use borsh::{BorshDeserialize, BorshSerialize};
use tempfile::TempDir;
use vprogs_core_smt::{Key, Node, Tree, WriteBatch};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{
    AccessMetadata, AccessType, BatchMetadata, ChainSink, Checkpoint, ResourceId,
    SchedulerTransaction,
};
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, SchedulerError, SchedulerResult,
    TransactionContext,
};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_storage_canonical_chain::{CanonicalChain, FrozenBits};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{PrefixIterator, StateSpace, Store};

/// The transaction id parked by [`GatedProcessor`].
const GATED_TX: usize = 99;

/// A one-shot gate: [`Gate::wait`] parks until [`Gate::open`], and never parks once open.
#[derive(Default)]
struct Gate {
    /// Whether the gate has been opened.
    opened: Mutex<bool>,
    /// Wakes the parked waiters when `opened` flips.
    signal: Condvar,
}

impl Gate {
    /// Opens the gate, releasing every parked waiter.
    fn open(&self) {
        *self.opened.lock().expect("gate mutex poisoned") = true;
        self.signal.notify_all();
    }

    /// Parks the caller until the gate is open, returning immediately if it already is.
    fn wait(&self) {
        let mut opened = self.opened.lock().expect("gate mutex poisoned");
        while !*opened {
            opened = self.signal.wait(opened).expect("gate mutex poisoned");
        }
    }

    /// Parks the caller until the gate is open or `timeout` has passed, and returns whether it
    /// is open.
    fn wait_for(&self, timeout: Duration) -> bool {
        let opened = self.opened.lock().expect("gate mutex poisoned");
        let (opened, _) = self
            .signal
            .wait_timeout_while(opened, timeout, |opened| !*opened)
            .expect("gate mutex poisoned");
        *opened
    }
}

/// Holds a pruning pass inside its commit: once armed, the pass parks at its frozen-bits write,
/// which comes after it has announced its range and before the commit that removes the range's
/// metadata.
#[derive(Default)]
struct PassHold {
    /// True while the next frozen-bits write must park.
    armed: AtomicBool,
    /// Opened by the pass when it has parked.
    reached: Gate,
    /// Opened by the test to let the pass go on.
    release: Gate,
}

/// A [`Store`] whose frozen-bits write parks while its [`PassHold`] is armed. Every operation
/// delegates unchanged.
#[derive(Clone)]
struct HeldPassStore {
    /// The store every operation delegates to.
    inner: RocksDbStore,
    /// Parks the pruning pass on request.
    hold: Arc<PassHold>,
}

impl Store for HeldPassStore {
    type WriteBatch = <RocksDbStore as Store>::WriteBatch;

    fn get(&self, state_space: StateSpace, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.get(state_space, key)
    }

    fn write_batch(&self) -> Self::WriteBatch {
        self.inner.write_batch()
    }

    fn commit(&self, write_batch: Self::WriteBatch) {
        self.inner.commit(write_batch)
    }

    fn prefix_iter(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter(state_space, prefix)
    }

    fn prefix_iter_rev(&self, state_space: StateSpace, prefix: &[u8]) -> PrefixIterator<'_> {
        self.inner.prefix_iter_rev(state_space, prefix)
    }

    fn range_iter(&self, state_space: StateSpace, start: &[u8], end: &[u8]) -> PrefixIterator<'_> {
        self.inner.range_iter(state_space, start, end)
    }

    fn canonical_chain(&self) -> CanonicalChain {
        self.inner.canonical_chain()
    }

    fn put_frozen_bits(&self, wb: &mut Self::WriteBatch, frozen: &[FrozenBits]) {
        // Only a pruning pass writes frozen bits, inside the commit that holds its deletions.
        if self.hold.armed.swap(false, Ordering::SeqCst) {
            self.hold.reached.open();
            self.hold.release.wait();
        }
        self.inner.put_frozen_bits(wb, frozen)
    }
}

impl Tree for HeldPassStore {
    type Hasher = <RocksDbStore as Tree>::Hasher;
    type Snapshot = <RocksDbStore as Tree>::Snapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.inner.snapshot()
    }

    fn node(&self, key: &Key, max_version: u64, snapshot: &Self::Snapshot) -> Option<(u64, Node)> {
        self.inner.node(key, max_version, snapshot)
    }

    fn prune(&self, wb: &mut impl WriteBatch, version: u64) {
        self.inner.prune(wb, version)
    }

    fn prune_orphaned(&self, wb: &mut impl WriteBatch, version: u64) {
        self.inner.prune_orphaned(wb, version)
    }
}

/// Batch metadata that names its parent, as a chain block's does. The `u64` metadata of the
/// other scheduler tests has parent id 0, which ends a restart's ancestry walk at the tip.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
struct Block {
    /// Stands in for the block hash.
    tag: u64,
    /// Id of the batch this block extends, 0 for the first block of a chain.
    parent: u64,
}

impl BatchMetadata for Block {
    fn block_hash(&self) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[24..].copy_from_slice(&self.tag.to_be_bytes());
        hash
    }

    fn parent_id(&self) -> u64 {
        self.parent
    }
}

/// A processor that appends the transaction id to every resource it writes, and parks
/// [`GATED_TX`] until its gate opens, which keeps that batch in flight.
#[derive(Clone)]
struct GatedProcessor {
    /// The gate [`GATED_TX`] parks on, shared with the test thread that opens it.
    gate: Arc<Gate>,
}

impl<S: Store> Processor<S> for GatedProcessor {
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
    type BatchMetadata = Block;
    type Error = ();
}

/// The scheduler under test.
type TestScheduler = Scheduler<HeldPassStore, GatedProcessor>;

/// Opens a scheduler over the store at `path`, replaying whatever is persisted there.
fn open(path: &Path, hold: &Arc<PassHold>, gate: &Arc<Gate>) -> TestScheduler {
    let storage = HeldPassStore { inner: RocksDbStore::open(path), hold: hold.clone() };
    Scheduler::new(
        ExecutionConfig::default().with_processor(GatedProcessor { gate: gate.clone() }),
        StorageConfig::default().with_store(storage),
    )
}

/// Schedules block `id` on top of batch `parent`, with one transaction `tx` that writes a
/// resource of the block's own, and checks that it is batch `id`.
fn schedule(
    scheduler: &mut TestScheduler,
    id: u64,
    parent: u64,
    tx: usize,
) -> ScheduledBatch<HeldPassStore, GatedProcessor> {
    let access = vec![AccessMetadata::write(ResourceId::for_test(id as usize))];
    let txs = vec![SchedulerTransaction::new(0, access, tx)];
    let batch = scheduler.schedule(Block { tag: id, parent }, txs);
    assert_eq!(batch.checkpoint().index(), id);
    batch
}

/// Commits block `id` on top of batch `parent`.
fn commit(scheduler: &mut TestScheduler, id: u64, parent: u64) {
    schedule(scheduler, id, parent, id as usize).wait_committed_blocking();
}

/// Whether batch `id` still has its metadata row, which a pruning pass removes.
fn has_metadata(scheduler: &TestScheduler, id: u64) -> bool {
    StoredBatchMetadata::exists(&**scheduler.state().storage().store(), id)
}

/// Waits until a pruning pass has removed batch `id`.
#[track_caller]
fn wait_pruned(scheduler: &TestScheduler, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while has_metadata(scheduler, id) {
        assert!(Instant::now() < deadline, "no pruning pass removed batch {id}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts that a rollback was refused because pruning has reached its target.
#[track_caller]
fn assert_pruning_conflict(result: SchedulerResult<Checkpoint<Block>>) {
    assert!(
        matches!(result, Err(SchedulerError::PruningConflict)),
        "a target the pruning worker has reached must be refused, got {result:?}"
    );
}

/// Asserts that a rollback was refused because its target is not a batch on the canonical chain.
#[track_caller]
fn assert_unknown_target(result: SchedulerResult<Checkpoint<Block>>) {
    assert!(
        matches!(result, Err(SchedulerError::UnknownTarget)),
        "a target that is not a batch on the canonical chain must be refused, got {result:?}"
    );
}

/// Asserts that the scheduler still stands at batch `id`: a refused rollback changes nothing.
#[track_caller]
fn assert_tip(scheduler: &TestScheduler, id: u64) {
    assert_eq!(scheduler.state().last_committed().index(), id, "the last committed batch moved");
    assert_eq!(scheduler.state().last_processed().index(), id, "the last processed batch moved");
    assert_eq!(scheduler.tip(), id, "the canonical tip moved");
}

/// Tests that a rollback and a re-root to the last batch a pruning pass removed are refused,
/// that the first batch the pass kept is still a target, and that the node starts again.
///
/// Batches 1 to 5 commit and a pass prunes batches 1 to 3, which makes batch 4 the root. A
/// rollback to batch 3 used to be accepted, since no rollback pointer it needs is gone, but
/// the metadata of batch 3 is.
#[test]
fn a_rollback_to_the_last_pruned_batch_is_refused() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (hold, gate) = (Arc::new(PassHold::default()), Arc::new(Gate::default()));
    {
        let mut scheduler = open(temp_dir.path(), &hold, &gate);
        for id in 1..=5 {
            commit(&mut scheduler, id, id - 1);
        }
        scheduler.finalize(4);
        wait_pruned(&scheduler, 3);

        assert_pruning_conflict(scheduler.rollback_to(3));
        assert_pruning_conflict(scheduler.reroot_to(3));
        assert_tip(&scheduler, 5);

        // The root is the first batch left, and a rollback to it finds its metadata.
        let target = scheduler.rollback_to(4).expect("the root is still a rollback target");
        assert_eq!((target.index(), target.metadata().tag), (4, 4));
        assert_tip(&scheduler, 4);

        scheduler.shutdown();
    }

    // The store restarts at batch 4 and goes on with a new block on top of it.
    let mut scheduler = open(temp_dir.path(), &hold, &gate);
    assert_tip(&scheduler, 4);
    commit(&mut scheduler, 6, 4);
    scheduler.shutdown();
}

/// Tests that a rollback to a batch is refused while the pass that prunes it is still in
/// flight.
///
/// The pass over batches 1 to 3 is held inside its commit: it has announced its range, and the
/// metadata of batch 3 is still on disk. A rollback to batch 3 used to be accepted and found
/// that metadata, and the pass deleted it afterwards, which left the last committed batch
/// below the root and without metadata.
#[test]
fn a_rollback_to_a_batch_a_pass_is_pruning_is_refused() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (hold, gate) = (Arc::new(PassHold::default()), Arc::new(Gate::default()));
    let mut scheduler = open(temp_dir.path(), &hold, &gate);
    for id in 1..=5 {
        commit(&mut scheduler, id, id - 1);
    }

    hold.armed.store(true, Ordering::SeqCst);
    scheduler.finalize(4);
    assert!(hold.reached.wait_for(Duration::from_secs(60)), "no pruning pass reached its commit");
    assert!(has_metadata(&scheduler, 3), "the held pass has not deleted anything yet");

    // Release the pass before the result is checked, so a failed assertion leaves no thread
    // parked.
    let result = scheduler.rollback_to(3);
    hold.release.open();
    assert_pruning_conflict(result);
    assert_tip(&scheduler, 5);

    wait_pruned(&scheduler, 3);
    assert_eq!(scheduler.state().root().index(), 4, "the pass moved the root to batch 4");
    scheduler.shutdown();
}

/// Tests that a rollback and a re-root to a batch that is no longer on the canonical chain are
/// refused.
///
/// Batches 1 and 2 commit and batch 3 is parked in flight. A rollback to batch 1 orphans batch
/// 2, which keeps its metadata, and cancels batch 3, which never gets any. Block 4 then extends
/// batch 1. A rollback to batch 2 used to be accepted and ended the canonical chain at a batch
/// that is not on it. A rollback to id 3 used to be accepted with default metadata. Pruning
/// must not stay paused at a refused target.
#[test]
fn a_rollback_to_a_batch_that_left_the_chain_is_refused() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (hold, gate) = (Arc::new(PassHold::default()), Arc::new(Gate::default()));
    let mut scheduler = open(temp_dir.path(), &hold, &gate);
    commit(&mut scheduler, 1, 0);
    commit(&mut scheduler, 2, 1);

    // The gate keeps batch 3 in flight until the rollback has canceled it.
    let canceled = schedule(&mut scheduler, 3, 2, GATED_TX);
    scheduler.rollback_to(1).expect("rollback to batch 1");
    assert!(canceled.canceled(), "batch 3 must be canceled while in flight");
    gate.open();
    commit(&mut scheduler, 4, 1);
    assert!(has_metadata(&scheduler, 2), "the orphaned batch keeps its metadata");
    assert!(!has_metadata(&scheduler, 3), "the canceled batch must leave no metadata");

    assert_unknown_target(scheduler.rollback_to(3));
    assert_unknown_target(scheduler.rollback_to(2));
    assert_unknown_target(scheduler.reroot_to(2));
    assert_tip(&scheduler, 4);

    // A refusal lifts its pause again: pruning goes on past the refused target.
    scheduler.finalize(4);
    wait_pruned(&scheduler, 2);

    scheduler.shutdown();
}

/// Tests that a rollback to a canonical batch without a metadata row is refused instead of
/// taking the default in the row's place.
///
/// The scheduler never removes the row of a canonical batch at or above the root, so the test
/// deletes one behind its back. That stands for any store in which a canonical id has no
/// metadata.
#[test]
fn a_rollback_to_a_batch_without_metadata_is_refused() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let (hold, gate) = (Arc::new(PassHold::default()), Arc::new(Gate::default()));
    let mut scheduler = open(temp_dir.path(), &hold, &gate);
    for id in 1..=4 {
        commit(&mut scheduler, id, id - 1);
    }

    let store = scheduler.state().storage().store();
    let mut write_batch = store.write_batch();
    StoredBatchMetadata::delete(&mut write_batch, 2);
    store.commit(write_batch);

    assert_unknown_target(scheduler.rollback_to(2));
    assert_tip(&scheduler, 4);

    scheduler.shutdown();
}
