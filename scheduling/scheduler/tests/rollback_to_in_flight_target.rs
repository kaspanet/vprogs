//! Pins that a rollback waits for a target that is still in flight, so that a batch scheduled
//! afterwards builds on what the target writes.
//!
//! A rollback clears the scheduler's resource chains, and a batch scheduled afterwards reads a
//! resource's state from disk. A rollback used to return while its target was still in flight.
//! What the target writes was not on disk then, and the new batch built on the state before it:
//! the target's write was lost.

use std::{
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};

use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, AccessType, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// The transaction id parked by [`GatedProcessor`], carried by the rollback target.
const GATED_TX: usize = 2;

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
    type BatchMetadata = u64;
    type Error = ();
}

/// One transaction with id `tx_id` writing `resource`.
fn write_tx(tx_id: usize, resource: usize) -> Vec<SchedulerTransaction<usize>> {
    vec![SchedulerTransaction::new(
        0,
        vec![AccessMetadata::write(ResourceId::for_test(resource))],
        tx_id,
    )]
}

/// Tests that a rollback to an in-flight target returns once the target has committed, and that
/// the next batch sees what the target wrote.
///
/// Batch 1 writes R and commits. Batch 2 writes R and is held in flight, batch 3 writes another
/// resource. A rollback to batch 2 cancels batch 3 only. Batch 2 is released while the rollback
/// is under way. Batch 4 then writes R as well, and R must hold all three writes in order.
#[test]
fn a_batch_after_a_rollback_builds_on_an_in_flight_target() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let gate = Arc::new(Gate::default());
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(GatedProcessor { gate: gate.clone() }),
        StorageConfig::default().with_store(storage),
    );

    scheduler.schedule(1, write_tx(1, 1)).wait_committed_blocking();
    let target = scheduler.schedule(2, write_tx(GATED_TX, 1));
    scheduler.schedule(3, write_tx(3, 2));

    // The target is released only once the rollback is under way, so the rollback meets it in
    // flight. Released earlier, the rollback would find it committed, which was never wrong.
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        gate.open();
    });
    scheduler.rollback_to(2).expect("rollback to the in-flight batch");
    assert!(!target.canceled(), "the rollback target stays live");
    assert!(target.committed(), "the rollback returns once its target has committed");

    let after = scheduler.schedule(4, write_tx(4, 1));
    after.wait_committed_blocking();
    release.join().expect("release thread panicked");

    let store = scheduler.state().storage().store();
    let state = StateVersion::from_latest_data(store.as_ref(), ResourceId::for_test(1));
    let expected: Vec<u8> = [1usize, 2, 4].iter().flat_map(|id| id.to_be_bytes()).collect();
    assert_eq!(*state.data(), expected, "the batch after the rollback lost the target's write");

    scheduler.shutdown();
}
