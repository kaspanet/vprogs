//! Reproduction: a reorg-canceled tip batch leaves a permanent id gap that bricks restart.
//!
//! A canceled batch never persists its metadata, yet keeps its allocated id, so the next block
//! takes a higher one. The resulting hole in the BatchMetadata column family is never backfilled,
//! and every subsequent start replays a non-contiguous id sequence.

use std::{
    path::Path,
    sync::{Arc, Condvar, Mutex},
};

use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// The transaction id parked by [`GatedProcessor`], carried by the batch the reorg cancels.
const GATED_TX: usize = 1;

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

/// A processor that parks [`GATED_TX`] until its gate opens, pinning that batch in flight.
///
/// Without the gate the tip batch races the reorg: its commit lands first often enough that a run
/// persists a contiguous id sequence and the gap under test never forms.
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

    fn supports_restore(&self) -> bool {
        true
    }

    type Transaction = usize;
    type TransactionArtifact = Vec<u8>;
    type BatchArtifact = Vec<u8>;
    type AggregatorArtifact = Vec<u8>;
    type BatchMetadata = u64;
    type Error = ();
}

/// A single-transaction batch writing to its own resource, so batches never contend.
fn write_tx(tx_id: usize, resource: usize) -> Vec<SchedulerTransaction<usize>> {
    vec![SchedulerTransaction::new(
        0,
        vec![AccessMetadata::write(ResourceId::for_test(resource))],
        tx_id,
    )]
}

/// Opens a scheduler over the store rooted at `path`, replaying whatever is already persisted.
fn open_scheduler(path: &Path, gate: Arc<Gate>) -> Scheduler<RocksDbStore, GatedProcessor> {
    let storage: RocksDbStore = RocksDbStore::open(path);
    Scheduler::new(
        ExecutionConfig::default().with_processor(GatedProcessor { gate }),
        StorageConfig::default().with_store(storage),
    )
}

/// A reorg canceling an in-flight tip batch strands its id, and the node never starts again.
///
/// The gap must be interior: `CanonicalChainManager::new` takes `base` from the first replayed
/// entry, so a leading gap restores fine and only a hole above the base strands the tip.
///
/// The second reorg target must be a fresh block hash. Flipping back onto the canceled block's
/// hash would reuse its retained id through the manager's reverse index and refill the gap.
#[test]
#[allow(clippy::assertions_on_constants)]
fn canceled_tip_batch_strands_an_id_and_bricks_restart() {
    // The debug_assert_eq! in CanonicalChainManager::new catches the non-contiguous replay and
    // aborts before the restart path runs, demonstrating a different failure than the one shipped
    // to users. Only a debug-assertions-off build exercises the real defect. The condition is a
    // compile-time constant on purpose: it must fail the run rather than silently skip it.
    assert!(
        !cfg!(debug_assertions),
        "run this repro with debug assertions off (cargo test --release), otherwise the \
         debug_assert_eq! in CanonicalChainManager::new masks the restart panic under test"
    );

    let temp_dir = TempDir::new().expect("failed to create temp dir");

    // Persist ids 1 and 3 while a reorg strands id 2.
    {
        let gate = Arc::new(Gate::default());
        let mut scheduler = open_scheduler(temp_dir.path(), gate.clone());

        let batch1 = scheduler.schedule(1, write_tx(0, 1));
        batch1.wait_committed_blocking();
        assert_eq!(batch1.checkpoint().index(), 1);

        // The gate holds the tip batch in flight, so its commit cannot outrun the reorg.
        let batch2 = scheduler.schedule(2, write_tx(GATED_TX, 2));

        // The reorg. rollback_to drives cancel_and_rollback, which cancels the tip batch and
        // rolls the canonical chain back, leaving the batch's commit a no-op.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(batch2.canceled(), "the tip batch must be canceled while in flight");
        assert_eq!(batch2.checkpoint().index(), 2, "the canceled batch held id 2");
        gate.open();

        // Block 3 is a distinct hash, so the reverse index cannot hand back the retained id 2.
        let batch3 = scheduler.schedule(3, write_tx(2, 3));
        batch3.wait_committed_blocking();
        assert_eq!(batch3.checkpoint().index(), 3, "the next block takes a higher id");

        let store = scheduler.state().storage().store();
        let persisted: Vec<u64> =
            (1..=3).filter(|&id| StoredBatchMetadata::exists(&**store, id)).collect();
        assert_eq!(persisted, vec![1, 3], "the canceled id must leave an interior gap");

        scheduler.shutdown();
    }

    // Restart over the persisted metadata. The replay re-densifies id 3 onto id 2, so the tip has
    // no live entry and the ancestry walk panics: the node cannot start.
    {
        let gate = Arc::new(Gate::default());
        gate.open();
        let scheduler = open_scheduler(temp_dir.path(), gate);
        scheduler.shutdown();
    }
}
