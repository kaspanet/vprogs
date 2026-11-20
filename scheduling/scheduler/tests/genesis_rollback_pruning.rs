//! Pins that the batches a rollback to genesis orphaned are pruned, and that the node starts
//! again afterwards.
//!
//! A rollback to genesis used to reset the root, and the next batch became the root under its
//! own, higher id. The orphaned batches then sat below the root, where the pruning worker never
//! looks: everything they wrote stayed in the store. Once a pruning pass had removed the batch
//! below a later root, a restart walked the tip's ancestry down to a batch without metadata and
//! panicked, because the orphaned rows still set the chain's base below the root.

use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use borsh::{BorshDeserialize, BorshSerialize};
use tempfile::TempDir;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{
    AccessMetadata, AccessType, BatchMetadata, ChainSink, ResourceId, SchedulerTransaction,
};
use vprogs_scheduling_scheduler::{ExecutionConfig, Processor, Scheduler, TransactionContext};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

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

/// A processor that appends the transaction id to every resource it writes.
#[derive(Clone)]
struct AppendProcessor;

impl<S: Store> Processor<S> for AppendProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Self::Error> {
        let tx_id = ctx.scheduler_tx().tx;
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

/// The resource batch `id` writes: one of its own, so batches never contend.
fn resource(id: u64) -> ResourceId {
    ResourceId::for_test(id as usize)
}

/// Opens a scheduler over the store at `path`, replaying whatever is persisted there.
fn open(path: &Path) -> Scheduler<RocksDbStore, AppendProcessor> {
    let storage: RocksDbStore = RocksDbStore::open(path);
    Scheduler::new(
        ExecutionConfig::default().with_processor(AppendProcessor),
        StorageConfig::default().with_store(storage),
    )
}

/// Commits block `id` on top of batch `parent` and checks that it is batch `id`.
fn commit(scheduler: &mut Scheduler<RocksDbStore, AppendProcessor>, id: u64, parent: u64) {
    let txs = vec![SchedulerTransaction::new(0, vec![AccessMetadata::write(resource(id))], 0)];
    let batch = scheduler.schedule(Block { tag: id, parent }, txs);
    batch.wait_committed_blocking();
    assert_eq!(batch.checkpoint().index(), id);
}

/// Commits batches 1 to 3, rolls back to genesis, commits batches 4 to 6 as a new chain and
/// prunes below batch 6.
fn prune_past_a_genesis_rollback(path: &Path) -> Scheduler<RocksDbStore, AppendProcessor> {
    let mut scheduler = open(path);
    for id in 1..=3 {
        commit(&mut scheduler, id, id - 1);
    }
    scheduler.rollback_to(0).expect("rollback to genesis");

    // The new chain starts at genesis again, so its first block has no parent.
    for (id, parent) in [(4, 0), (5, 4), (6, 5)] {
        commit(&mut scheduler, id, parent);
    }

    scheduler.finalize(6);
    let deadline = Instant::now() + Duration::from_secs(30);
    while StoredBatchMetadata::exists(&**scheduler.state().storage().store(), 5) {
        assert!(Instant::now() < deadline, "the pruning pass never removed batch 5");
        thread::sleep(Duration::from_millis(10));
    }
    scheduler
}

/// Tests that a pruning pass after a rollback to genesis reclaims the batches the rollback
/// orphaned along with the canonical ones below the threshold.
#[test]
fn batches_orphaned_by_a_genesis_rollback_are_pruned() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let scheduler = prune_past_a_genesis_rollback(temp_dir.path());

    let store = scheduler.state().storage().store();
    for id in 1..=3 {
        assert!(
            !StoredBatchMetadata::exists(&**store, id),
            "orphaned batch {id} kept its metadata"
        );
        assert!(
            StateVersion::get(&**store, id, &resource(id)).is_none(),
            "orphaned batch {id} kept its write"
        );
    }
    // The new chain is canonical: its batches lose nothing they wrote.
    for id in 4..=6 {
        assert!(
            StateVersion::get(&**store, id, &resource(id)).is_some(),
            "canonical batch {id} lost its write"
        );
    }
    assert_eq!(scheduler.state().root().index(), 6, "the root is the first batch left");

    scheduler.shutdown();
}

/// Tests that the node starts again after a pruning pass that followed a rollback to genesis,
/// and goes on scheduling.
#[test]
fn restart_after_pruning_past_a_genesis_rollback() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    prune_past_a_genesis_rollback(temp_dir.path()).shutdown();

    let mut scheduler = open(temp_dir.path());
    commit(&mut scheduler, 7, 6);
    scheduler.shutdown();
}
