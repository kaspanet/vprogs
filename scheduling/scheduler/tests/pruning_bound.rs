//! Pins that a pruning pass never goes past the last committed batch.
//!
//! The pass used to prune every id below its threshold. An id whose batch had not committed
//! was pruned too: a batch still in flight as if it had written nothing, a batch rolled back by
//! a re-root and about to be fed again as an orphan. When that batch then committed, its rows
//! sat below the root, where no pass returns.

use std::{
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use vprogs_core_smt::Tree;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ChainSink, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{
    ExecutionConfig, Processor, ScheduledBatch, Scheduler, TransactionContext,
};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// The transaction id parked by [`GatedProcessor`].
const GATED_TX: usize = 3;

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
}

/// A processor that appends the transaction id to every resource it writes, and parks
/// [`GATED_TX`] until its gate opens, which keeps that batch in flight.
#[derive(Clone)]
struct GatedProcessor {
    /// The gate [`GATED_TX`] parks on, shared with the test thread that opens it.
    gate: Arc<Gate>,
    /// Whether a block that returns is restored from disk instead of executed again.
    restore: bool,
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

    fn supports_restore(&self) -> bool {
        self.restore
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = u64;
    type Error = ();
}

/// The scheduler under test.
type TestScheduler = Scheduler<RocksDbStore, GatedProcessor>;

/// Opens a scheduler over a fresh store at `path`.
fn open(path: &Path, gate: &Arc<Gate>, restore: bool) -> TestScheduler {
    let storage: RocksDbStore = RocksDbStore::open(path);
    let processor = GatedProcessor { gate: gate.clone(), restore };
    Scheduler::new(
        ExecutionConfig::default().with_processor(processor),
        StorageConfig::default().with_store(storage),
    )
}

/// The resource batch `id` writes: one of its own, so batches never contend.
fn resource(id: u64) -> ResourceId {
    ResourceId::for_test(id as usize)
}

/// Schedules block `id`, whose one transaction has id `id` and writes the block's resource.
fn schedule(
    scheduler: &mut TestScheduler,
    id: u64,
) -> ScheduledBatch<RocksDbStore, GatedProcessor> {
    let access = vec![AccessMetadata::write(resource(id))];
    let batch = scheduler.schedule(id, vec![SchedulerTransaction::new(0, access, id as usize)]);
    assert_eq!(batch.checkpoint().index(), id);
    batch
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

/// Asserts that the root is batch 5, that no metadata row or rollback pointer of a batch is
/// left below it, and that the batches 3 to 5 kept what they wrote.
#[track_caller]
fn assert_pruned_up_to_the_tip(scheduler: &TestScheduler) {
    assert_eq!(scheduler.state().root().index(), 5, "the root is the last committed batch");
    let store = scheduler.state().storage().store();
    for id in 1..=4 {
        assert!(!has_metadata(scheduler, id), "batch {id} kept its metadata below the root");
        assert_eq!(
            StatePtrRollback::iter_batch(&**store, id).count(),
            0,
            "batch {id} kept rollback pointers below the root"
        );
    }
    for id in 3..=5 {
        assert!(
            StateVersion::get(&**store, id, &resource(id)).is_some(),
            "batch {id} lost its write"
        );
    }
}

/// Tests that a batch which is still in flight is not pruned, and that pruning catches up by
/// itself once it has committed.
///
/// Batches 1 and 2 commit, batch 3 is held in flight, and 4 and 5 queue up behind it. The
/// threshold is set above all of them. A pass may only take batch 1, since batch 2 is the last
/// committed one. Once batch 3 is released and the rest has committed, the passes go on to
/// the threshold without another call.
#[test]
fn a_batch_in_flight_is_not_pruned_before_it_commits() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let gate = Arc::new(Gate::default());
    let mut scheduler = open(temp_dir.path(), &gate, true);

    for id in 1..=2 {
        schedule(&mut scheduler, id).wait_committed_blocking();
    }
    schedule(&mut scheduler, 3);
    schedule(&mut scheduler, 4);
    let tip = schedule(&mut scheduler, 5);

    scheduler.finalize(5);
    wait_pruned(&scheduler, 1);
    assert!(has_metadata(&scheduler, 2), "the pass went past the last committed batch");

    gate.open();
    tip.wait_committed_blocking();
    wait_pruned(&scheduler, 4);
    assert_pruned_up_to_the_tip(&scheduler);

    scheduler.shutdown();
}

/// Runs a re-root below the pruning threshold and feeds the blocks above the boundary again,
/// restored from disk or executed again.
///
/// Batches 1 to 5 commit, a re-root rolls execution back to batch 2, and the threshold is set
/// to 5. The batches 3 to 5 read orphaned then, but their blocks are about to return under the
/// same ids. A pass may only take batch 1. The blocks are fed again, and once they have
/// committed the passes go on to the threshold.
fn reroot_below_the_threshold(restore: bool) {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let gate = Arc::new(Gate::default());
    gate.open();
    let mut scheduler = open(temp_dir.path(), &gate, restore);

    for id in 1..=5 {
        schedule(&mut scheduler, id).wait_committed_blocking();
    }
    let root_5 = scheduler.state().storage().store().root(5);

    scheduler.reroot_to(2).expect("re-root to the boundary");
    scheduler.finalize(5);
    wait_pruned(&scheduler, 1);
    for id in 3..=4 {
        assert!(
            has_metadata(&scheduler, id),
            "the pass reclaimed batch {id}, which is about to be fed again"
        );
    }

    for id in 3..=5 {
        let batch = schedule(&mut scheduler, id);
        batch.wait_committed_blocking();
        assert_eq!(batch.restored(), restore, "block {id} returns under its id");
    }
    wait_pruned(&scheduler, 4);
    assert_pruned_up_to_the_tip(&scheduler);
    assert_eq!(scheduler.state().storage().store().root(5), root_5, "the tree is what it was");

    scheduler.shutdown();
}

/// Tests that a re-root below the pruning threshold holds pruning back until the batches above
/// the boundary have been fed again and restored from disk, which needs their rows.
#[test]
fn a_reroot_below_the_threshold_holds_pruning_until_the_batches_are_restored() {
    reroot_below_the_threshold(true);
}

/// Tests the same for a node that executes a returning block again, as a proving node does:
/// the batches executed again leave no metadata row or rollback pointer below the root.
#[test]
fn a_reroot_below_the_threshold_holds_pruning_until_the_batches_are_executed_again() {
    reroot_below_the_threshold(false);
}
