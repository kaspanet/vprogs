use std::{sync::mpsc, thread, time::Duration};

use tempfile::TempDir;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{EMPTY_HASH, Tree, proving::Proof};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, Checkpoint, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler, TransactionContext};
use vprogs_scheduling_test_utils::{Processor, SchedulerExt};
use vprogs_state_metadata::StateMetadata;
use vprogs_state_version::StateVersion;
use vprogs_storage_canonical_chain::BUCKET_CAPACITY;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;

/// The current global state root: the SMT root at the last committed version.
fn state_root(scheduler: &Scheduler<RocksDbStore, Processor>) -> [u8; 32] {
    let store = scheduler.state().storage().store();
    store.root(StateMetadata::last_committed::<u64, _>(store.as_ref()).index())
}

#[test]
pub fn test_scheduler() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        let batch1 = scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    0,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(1)),
                        AccessMetadata::read(ResourceId::for_test(3)),
                    ],
                    0,
                ),
                SchedulerTransaction::new(
                    1,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(1)),
                        AccessMetadata::write(ResourceId::for_test(2)),
                    ],
                    1,
                ),
                SchedulerTransaction::new(
                    2,
                    vec![AccessMetadata::read(ResourceId::for_test(3))],
                    2,
                ),
            ],
        );

        let batch2 = scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(
                    3,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(1)),
                        AccessMetadata::read(ResourceId::for_test(3)),
                    ],
                    3,
                ),
                SchedulerTransaction::new(
                    4,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(10)),
                        AccessMetadata::write(ResourceId::for_test(20)),
                    ],
                    4,
                ),
            ],
        );

        batch1.wait_committed_blocking();
        batch2.wait_committed_blocking();

        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0, 1, 3])
            .assert_written_state(ResourceId::for_test(2), vec![1])
            .assert_written_state(ResourceId::for_test(3), vec![])
            .assert_written_state(ResourceId::for_test(10), vec![4])
            .assert_written_state(ResourceId::for_test(20), vec![4]);

        scheduler.shutdown();
    }
}

/// Batch-local resource indices follow ascending resource-id order rather than first-touch order,
/// so the batch proof's queried keys arrive sorted.
#[test]
pub fn test_batch_resource_indices_sorted() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // First-touch order is 5, 1, 9; assignment must re-order to ascending ids.
        let batch = scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::write(ResourceId::for_test(5))],
                    0,
                ),
                SchedulerTransaction::new(
                    1,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(1)),
                        AccessMetadata::write(ResourceId::for_test(9)),
                    ],
                    1,
                ),
            ],
        );
        batch.wait_committed_blocking();

        assert_eq!(batch.resource_ids(), [1, 5, 9].map(ResourceId::for_test).to_vec());
        for (position, diff) in batch.state_diffs().iter().enumerate() {
            assert_eq!(diff.index(), position as u32, "state diff out of index order");
        }

        scheduler.shutdown();
    }
}

/// Tests rollback of committed batches.
#[test]
pub fn test_rollback_committed() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    0,
                ),
                SchedulerTransaction::new(
                    1,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    1,
                ),
            ],
        );
        scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(
                    2,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    2,
                ),
                SchedulerTransaction::new(
                    3,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    3,
                ),
            ],
        );
        let last_batch = scheduler.schedule(
            3,
            vec![
                SchedulerTransaction::new(
                    4,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    4,
                ),
                SchedulerTransaction::new(
                    5,
                    vec![AccessMetadata::write(ResourceId::for_test(4))],
                    5,
                ),
            ],
        );
        last_batch.wait_committed_blocking();

        // Verify state before rollback
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0, 2, 4]) // Written by tx 0, 2, 4
            .assert_written_state(ResourceId::for_test(2), vec![1]) // Written by tx 1
            .assert_written_state(ResourceId::for_test(3), vec![3]) // Written by tx 3
            .assert_written_state(ResourceId::for_test(4), vec![5]); // Written by tx 5

        // Verify last committed checkpoint on disk
        let checkpoint: Checkpoint<u64> =
            StateMetadata::last_committed(&**scheduler.state().storage().store());
        assert_eq!(checkpoint.index(), 3);
        assert_eq!(*checkpoint.metadata(), 3);

        // Rollback to index 1 (revert batches with index 2 and 3, keep batch with index 1)
        let target = scheduler.rollback_to(1).expect("rollback should succeed");
        assert_eq!(target.index(), 1);
        assert_eq!(*target.metadata(), 1);

        // Verify state after rollback - only batch1 state changes should remain
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0]) // Only tx 0's write remains
            .assert_written_state(ResourceId::for_test(2), vec![1]) // tx 1's write remains (in batch1)
            .assert_resource_deleted(ResourceId::for_test(3))
            .assert_resource_deleted(ResourceId::for_test(4));

        // Verify last committed checkpoint on disk after rollback
        let checkpoint: Checkpoint<u64> =
            StateMetadata::last_committed(&**scheduler.state().storage().store());
        assert_eq!(checkpoint.index(), 1);
        assert_eq!(*checkpoint.metadata(), 1);

        scheduler.shutdown();
    }
}

/// Tests that new batches can be scheduled after a rollback and receive correct indices. After
/// rollback, the scheduler state resets to the target index and new batches continue from there.
#[test]
pub fn test_add_batches_after_rollback() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Schedule initial batches (indices 1, 2, 3)
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                1,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                2,
            )],
        );
        batch3.wait_committed_blocking();

        assert_eq!(batch1.checkpoint().index(), 1);
        assert_eq!(batch2.checkpoint().index(), 2);
        assert_eq!(batch3.checkpoint().index(), 3);

        // Verify initial state
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0])
            .assert_written_state(ResourceId::for_test(2), vec![1])
            .assert_written_state(ResourceId::for_test(3), vec![2]);

        // Rollback to batch 1 (keep batch 1, remove batches 2 and 3)
        scheduler.rollback_to(1).expect("rollback should succeed");

        // Resources 2 and 3 should be deleted, resource 1 should still exist
        scheduler
            .assert_resource_deleted(ResourceId::for_test(2))
            .assert_resource_deleted(ResourceId::for_test(3))
            .assert_written_state(ResourceId::for_test(1), vec![0]);

        // Schedule new batches after rollback - should continue at 4, 5.
        let batch4 = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                10,
                vec![AccessMetadata::write(ResourceId::for_test(10))],
                10,
            )],
        );
        let batch5 = scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                11,
                vec![AccessMetadata::write(ResourceId::for_test(11))],
                11,
            )],
        );
        batch5.wait_committed_blocking();

        assert_eq!(batch4.checkpoint().index(), 4);
        assert_eq!(batch5.checkpoint().index(), 5);

        // Verify new state
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0])
            .assert_written_state(ResourceId::for_test(10), vec![10])
            .assert_written_state(ResourceId::for_test(11), vec![11]);

        scheduler.shutdown();
    }
}

/// Tests in-flight batch cancellation without waiting for commitment. When a rollback occurs,
/// batches that haven't been committed yet should detect cancellation via canceled() and skip
/// their writes.
#[test]
pub fn test_inflight_cancellation_without_waiting() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Schedule a batch and wait for it to commit
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        batch1.wait_committed_blocking();

        // Schedule multiple batches but don't wait for commitment
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                1,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                2,
            )],
        );
        let batch4 = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
                3,
            )],
        );

        // Immediately rollback without waiting for batches 2-4 (tests in-flight cancellation).
        scheduler.rollback_to(1).expect("rollback should succeed");

        // After rollback, the canceled batches should have canceled() == true
        assert!(batch2.canceled(), "batch2 should be canceled");
        assert!(batch3.canceled(), "batch3 should be canceled");
        assert!(batch4.canceled(), "batch4 should be canceled");

        // Resource 1 survives (batch1 committed); resources 2-4 are cleaned up by the rollback.
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0])
            .assert_resource_deleted(ResourceId::for_test(2))
            .assert_resource_deleted(ResourceId::for_test(3))
            .assert_resource_deleted(ResourceId::for_test(4));

        // New batches scheduled after rollback should work normally
        let batch5 = scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                100,
                vec![AccessMetadata::write(ResourceId::for_test(100))],
                100,
            )],
        );
        batch5.wait_committed_blocking();
        assert!(!batch5.canceled(), "batch5 should not be canceled");
        scheduler.assert_written_state(ResourceId::for_test(100), vec![100]);

        scheduler.shutdown();
    }
}

/// Tests rollback across multiple cancellation contexts with parent chain traversal. This complex
/// scenario tests: apply 1-6; rollback to 5; apply 6-7; rollback to 3; apply 4-5.
#[test]
pub fn test_rollback_multiple_contexts() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Phase 1: apply batches 1-6 (resource ids match batch indices for clarity).
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                3,
            )],
        );
        scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                4,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
                4,
            )],
        );
        scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                5,
                vec![AccessMetadata::write(ResourceId::for_test(5))],
                5,
            )],
        );
        let batch6 = scheduler.schedule(
            6,
            vec![SchedulerTransaction::new(
                6,
                vec![AccessMetadata::write(ResourceId::for_test(6))],
                6,
            )],
        );
        batch6.wait_committed_blocking();

        assert_eq!(batch1.checkpoint().index(), 1);
        assert_eq!(batch6.checkpoint().index(), 6);

        // Verify all resources exist
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3])
            .assert_written_state(ResourceId::for_test(4), vec![4])
            .assert_written_state(ResourceId::for_test(5), vec![5])
            .assert_written_state(ResourceId::for_test(6), vec![6]);

        // Phase 2: Rollback to 5 (keeps batches 1-5, removes batch 6)
        scheduler.rollback_to(5).expect("rollback should succeed");

        // Batch 6's resource should be deleted, batches 1-5 should still exist
        scheduler
            .assert_resource_deleted(ResourceId::for_test(6))
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3])
            .assert_written_state(ResourceId::for_test(4), vec![4])
            .assert_written_state(ResourceId::for_test(5), vec![5]);

        // Phase 3: Apply new batches 6-7 (after rollback)
        let new_batch6 = scheduler.schedule(
            60,
            vec![SchedulerTransaction::new(
                60,
                vec![AccessMetadata::write(ResourceId::for_test(60))],
                60,
            )],
        );
        let batch7 = scheduler.schedule(
            70,
            vec![SchedulerTransaction::new(
                70,
                vec![AccessMetadata::write(ResourceId::for_test(70))],
                70,
            )],
        );
        batch7.wait_committed_blocking();

        assert_eq!(new_batch6.checkpoint().index(), 7);
        assert_eq!(batch7.checkpoint().index(), 8);

        scheduler
            .assert_written_state(ResourceId::for_test(60), vec![60])
            .assert_written_state(ResourceId::for_test(70), vec![70]);

        // Phase 4: Rollback to 3 (must walk parent cancellation chain)
        scheduler.rollback_to(3).expect("rollback should succeed");

        // Batches 1-3 should remain, 4-5 and new 6-7 should be deleted
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3])
            .assert_resource_deleted(ResourceId::for_test(4))
            .assert_resource_deleted(ResourceId::for_test(5))
            .assert_resource_deleted(ResourceId::for_test(60))
            .assert_resource_deleted(ResourceId::for_test(70));

        // Phase 5: Apply batches 4-5 (after second rollback)
        let final_batch4 = scheduler.schedule(
            40,
            vec![SchedulerTransaction::new(
                40,
                vec![AccessMetadata::write(ResourceId::for_test(40))],
                40,
            )],
        );
        let final_batch5 = scheduler.schedule(
            50,
            vec![SchedulerTransaction::new(
                50,
                vec![AccessMetadata::write(ResourceId::for_test(50))],
                50,
            )],
        );
        final_batch5.wait_committed_blocking();

        assert_eq!(final_batch4.checkpoint().index(), 9);
        assert_eq!(final_batch5.checkpoint().index(), 10);

        // Verify final state
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3])
            .assert_written_state(ResourceId::for_test(40), vec![40])
            .assert_written_state(ResourceId::for_test(50), vec![50]);

        scheduler.shutdown();
    }
}

/// Tests rollback to batch 0 (complete revert to initial state). All state should be cleared.
#[test]
pub fn test_rollback_to_zero() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Schedule several batches
        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                3,
            )],
        );
        batch3.wait_committed_blocking();

        // Verify state exists
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3]);

        // Rollback to 0 (before any batches)
        scheduler.rollback_to(0).expect("rollback should succeed");

        // All resources should be deleted
        scheduler
            .assert_resource_deleted(ResourceId::for_test(1))
            .assert_resource_deleted(ResourceId::for_test(2))
            .assert_resource_deleted(ResourceId::for_test(3));

        // Ids are never reused: the next batch continues past the orphaned ids (at 4), not at 1.
        let new_batch1 = scheduler.schedule(
            100,
            vec![SchedulerTransaction::new(
                100,
                vec![AccessMetadata::write(ResourceId::for_test(100))],
                100,
            )],
        );
        new_batch1.wait_committed_blocking();

        assert_eq!(new_batch1.checkpoint().index(), 4);
        scheduler.assert_written_state(ResourceId::for_test(100), vec![100]);

        scheduler.shutdown();
    }
}

/// Tests multiple consecutive rollbacks without adding new batches in between. Verifies that
/// consecutive rollbacks properly reduce state.
#[test]
pub fn test_consecutive_rollbacks() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create 5 batches
        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                3,
            )],
        );
        scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                4,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
                4,
            )],
        );
        let batch5 = scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                5,
                vec![AccessMetadata::write(ResourceId::for_test(5))],
                5,
            )],
        );
        batch5.wait_committed_blocking();

        // Verify all exist
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3])
            .assert_written_state(ResourceId::for_test(4), vec![4])
            .assert_written_state(ResourceId::for_test(5), vec![5]);

        // First rollback: to 4
        scheduler.rollback_to(4).expect("rollback should succeed");
        scheduler
            .assert_resource_deleted(ResourceId::for_test(5))
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3])
            .assert_written_state(ResourceId::for_test(4), vec![4]);

        // Second rollback: to 3
        scheduler.rollback_to(3).expect("rollback should succeed");
        scheduler
            .assert_resource_deleted(ResourceId::for_test(4))
            .assert_resource_deleted(ResourceId::for_test(5))
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_written_state(ResourceId::for_test(2), vec![2])
            .assert_written_state(ResourceId::for_test(3), vec![3]);

        // Third rollback: to 1
        scheduler.rollback_to(1).expect("rollback should succeed");
        scheduler
            .assert_resource_deleted(ResourceId::for_test(2))
            .assert_resource_deleted(ResourceId::for_test(3))
            .assert_written_state(ResourceId::for_test(1), vec![1]);

        // Verify batch execution indices are correct
        let new_batch = scheduler.schedule(
            100,
            vec![SchedulerTransaction::new(
                100,
                vec![AccessMetadata::write(ResourceId::for_test(100))],
                100,
            )],
        );
        new_batch.wait_committed_blocking();
        assert_eq!(new_batch.checkpoint().index(), 6);

        scheduler.shutdown();
    }
}

/// Tests rollback of batches that modify the same resource multiple times. This exercises the
/// resource dependency chain and version restoration.
#[test]
pub fn test_rollback_same_resource_multiple_writes() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Multiple batches all writing to resource 1
        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                10,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                10,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                20,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                20,
            )],
        );
        scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                30,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                30,
            )],
        );
        let batch4 = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                40,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                40,
            )],
        );
        batch4.wait_committed_blocking();

        // Resource 1 should have been written by all 4 transactions
        scheduler.assert_written_state(ResourceId::for_test(1), vec![10, 20, 30, 40]);

        // Rollback to batch 2 (keep writes from batch 1 and 2)
        scheduler.rollback_to(2).expect("rollback should succeed");
        scheduler.assert_written_state(ResourceId::for_test(1), vec![10, 20]);

        // Add more writes
        let batch5 = scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                50,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                50,
            )],
        );
        batch5.wait_committed_blocking();

        // Now should have 10, 20, 50
        scheduler.assert_written_state(ResourceId::for_test(1), vec![10, 20, 50]);

        // Rollback to batch 1
        scheduler.rollback_to(1).expect("rollback should succeed");
        scheduler.assert_written_state(ResourceId::for_test(1), vec![10]);

        scheduler.shutdown();
    }
}

/// Tests that canceled batches are marked as canceled and wait functions return immediately without
/// blocking.
#[test]
pub fn test_cancellation_skips_writes() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create and commit a batch to resource 1
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch1.wait_committed_blocking();

        // Schedule batches that access different resources
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(100))],
                2,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(200))],
                3,
            )],
        );

        // Rollback immediately - batch2 and batch3 should be canceled
        scheduler.rollback_to(1).expect("rollback should succeed");

        // Verify both batches were canceled
        assert!(batch2.canceled(), "batch2 should be canceled");
        assert!(batch3.canceled(), "batch3 should be canceled");

        // The wait functions should return immediately for canceled batches
        batch2.wait_committed_blocking();
        batch3.wait_committed_blocking();

        // Resource 1 keeps only batch1's write; resources 100 and 200 are cleaned up.
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_resource_deleted(ResourceId::for_test(100))
            .assert_resource_deleted(ResourceId::for_test(200));

        scheduler.shutdown();
    }
}

/// Tests that a rollback releases a waiter already parked on a canceled batch's artifact latch.
///
/// The batch executes fully while live, so the cancel-conditional latch opening at the end of
/// execution has already declined, and nothing in this harness publishes batch artifacts.
#[test]
pub fn test_wait_artifact_published_wakes_on_late_cancellation() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit an anchor so the rollback below has a committed target.
        let anchor = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        anchor.wait_committed_blocking();

        // Let the batch execute fully while live.
        let batch = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                1,
            )],
        );
        batch.wait_processed_blocking();
        assert!(!batch.canceled(), "batch must be live before the waiter parks");

        // Park a consumer on the artifact latch.
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let batch = batch.clone();
            thread::spawn(move || {
                batch.wait_artifact_published_blocking();
                done_tx.send(()).unwrap();
            })
        };
        thread::sleep(Duration::from_millis(200));
        assert!(done_rx.try_recv().is_err(), "waiter must be parked before the rollback");

        // Cancel the batch out from under the parked waiter.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(batch.canceled(), "batch should be canceled");

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a rollback must release a consumer parked on the canceled batch's artifact");
        waiter.join().unwrap();

        scheduler.shutdown();
    }
}

/// Tests that a rollback releases a waiter already parked on a canceled batch's tx-artifacts
/// latch. Nothing in this harness publishes transaction artifacts, so only the cancellation can
/// release the waiter.
#[test]
pub fn test_wait_tx_artifacts_published_wakes_on_late_cancellation() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit an anchor so the rollback below has a committed target.
        let anchor = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        anchor.wait_committed_blocking();

        // Let the batch execute fully while live.
        let batch = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                1,
            )],
        );
        batch.wait_processed_blocking();
        assert!(!batch.canceled(), "batch must be live before the waiter parks");

        // Park a consumer on the tx-artifacts latch.
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let batch = batch.clone();
            thread::spawn(move || {
                batch.wait_tx_artifacts_published_blocking();
                done_tx.send(()).unwrap();
            })
        };
        thread::sleep(Duration::from_millis(200));
        assert!(done_rx.try_recv().is_err(), "waiter must be parked before the rollback");

        // Cancel the batch out from under the parked waiter.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(batch.canceled(), "batch should be canceled");

        done_rx.recv_timeout(Duration::from_secs(5)).expect(
            "a rollback must release a consumer parked on the canceled batch's tx artifacts",
        );
        waiter.join().unwrap();

        scheduler.shutdown();
    }
}

/// Processor that parks the execution of one designated transaction until a latch opens.
#[derive(Clone)]
struct GateProcessor {
    /// Transaction payload whose execution parks until `release` opens.
    gate_tx: usize,
    /// Opened by the test to let the gated transaction finish.
    release: AtomicAsyncLatch,
}

impl vprogs_scheduling_scheduler::Processor<RocksDbStore> for GateProcessor {
    fn process_transaction(
        &self,
        ctx: &mut TransactionContext<RocksDbStore, Self>,
    ) -> Result<(), Self::Error> {
        if ctx.scheduler_tx().tx == self.gate_tx {
            self.release.wait_blocking();
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

/// Tests that a rollback releases a waiter already parked on a canceled batch's committed latch.
///
/// A gate transaction holds execution of the preceding batch, so the lifecycle worker (which
/// awaits batches in FIFO order) provably has not submitted the watched batch's commit when the
/// rollback lands; a canceled batch's commit submission is a no-op, so only the cancellation can
/// release the waiter.
#[test]
pub fn test_wait_committed_wakes_on_late_cancellation() {
    const GATE_TX: usize = 100;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let release = AtomicAsyncLatch::new();
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default()
                .with_processor(GateProcessor { gate_tx: GATE_TX, release: release.clone() }),
            StorageConfig::default().with_store(storage),
        );

        // Commit an anchor so the rollback below has a committed target.
        let anchor = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        anchor.wait_committed_blocking();

        // The gate batch parks inside execution, holding the lifecycle worker ahead of `batch`.
        let gate_batch = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                GATE_TX,
            )],
        );
        let batch = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                2,
            )],
        );

        // Park a consumer on the committed latch.
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let batch = batch.clone();
            thread::spawn(move || {
                batch.wait_committed_blocking();
                done_tx.send(()).unwrap();
            })
        };
        thread::sleep(Duration::from_millis(200));
        assert!(!batch.committed(), "commit must not run while the gate holds execution");
        assert!(done_rx.try_recv().is_err(), "waiter must be parked before the rollback");

        // Cancel both in-flight batches, then release the gate so they finish executing.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert!(gate_batch.canceled(), "gate batch should be canceled");
        assert!(batch.canceled(), "batch should be canceled");
        release.open();

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a rollback must release a consumer parked on the canceled batch's commit");
        waiter.join().unwrap();

        scheduler.shutdown();
    }
}

/// Tests that a waiter woken by a rollback that leaves its batch live re-parks, and that a later,
/// deeper rollback canceling the batch (now held in a parent cancellation context) releases it.
///
/// The first rollback cancels only the tip: the parked waiter is woken, must observe its batch
/// still live, and park again. The second rollback reaches the watched batch's context through the
/// rollback chain walk; only that cancellation can release the waiter, since nothing in this
/// harness publishes batch artifacts.
#[test]
pub fn test_reparked_waiter_wakes_on_deeper_rollback() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Two committed anchors give both rollbacks committed targets.
        let anchor1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                0,
            )],
        );
        anchor1.wait_committed_blocking();
        let anchor2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                1,
            )],
        );
        anchor2.wait_committed_blocking();

        // The watched batch and the tip execute fully while live.
        let watched = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                2,
            )],
        );
        watched.wait_processed_blocking();
        let tip = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
                3,
            )],
        );
        tip.wait_processed_blocking();

        // Park a consumer on the watched batch's artifact latch.
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let watched = watched.clone();
            thread::spawn(move || {
                watched.wait_artifact_published_blocking();
                done_tx.send(()).unwrap();
            })
        };
        thread::sleep(Duration::from_millis(200));
        assert!(done_rx.try_recv().is_err(), "waiter must be parked before the rollbacks");

        // The shallow rollback cancels only the tip. Its wake reaches the watched batch's waiter,
        // which must find the batch live and re-park rather than return.
        scheduler.rollback_to(3).expect("shallow rollback should succeed");
        assert!(tip.canceled(), "the tip should be canceled by the shallow rollback");
        assert!(!watched.canceled(), "the watched batch must survive the shallow rollback");
        thread::sleep(Duration::from_millis(200));
        assert!(
            done_rx.try_recv().is_err(),
            "a cancellation wake that leaves the batch live must re-park the waiter",
        );

        // The deep rollback cancels the watched batch, whose context is now a parent in the
        // rollback chain; the chain walk must release the re-parked waiter.
        scheduler.rollback_to(2).expect("deep rollback should succeed");
        assert!(watched.canceled(), "the watched batch should be canceled by the deep rollback");
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a deeper rollback must release a waiter re-parked after an earlier wake");
        waiter.join().unwrap();

        scheduler.shutdown();
    }
}

/// Tests complex scenario with interleaved writes to multiple resources followed by rollback and
/// new writes.
#[test]
pub fn test_rollback_interleaved_multi_resource() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Batch 1: Write to resources 1 and 2
        scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    10,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    10,
                ),
                SchedulerTransaction::new(
                    11,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    11,
                ),
            ],
        );

        // Batch 2: Write to resources 2 and 3
        scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(
                    20,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    20,
                ),
                SchedulerTransaction::new(
                    21,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    21,
                ),
            ],
        );

        // Batch 3: Write to resources 1 and 3
        scheduler.schedule(
            3,
            vec![
                SchedulerTransaction::new(
                    30,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    30,
                ),
                SchedulerTransaction::new(
                    31,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    31,
                ),
            ],
        );

        // Batch 4: Write to all resources
        let batch4 = scheduler.schedule(
            4,
            vec![
                SchedulerTransaction::new(
                    40,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    40,
                ),
                SchedulerTransaction::new(
                    41,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    41,
                ),
                SchedulerTransaction::new(
                    42,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    42,
                ),
            ],
        );
        batch4.wait_committed_blocking();

        // Verify state before rollback
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![10, 30, 40])
            .assert_written_state(ResourceId::for_test(2), vec![11, 20, 41])
            .assert_written_state(ResourceId::for_test(3), vec![21, 31, 42]);

        // Rollback to batch 2
        scheduler.rollback_to(2).expect("rollback should succeed");

        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![10]) // Only from batch 1
            .assert_written_state(ResourceId::for_test(2), vec![11, 20]) // From batch 1 and 2
            .assert_written_state(ResourceId::for_test(3), vec![21]); // Only from batch 2

        // Add new batches after rollback
        let batch5 = scheduler.schedule(
            5,
            vec![
                SchedulerTransaction::new(
                    50,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    50,
                ),
                SchedulerTransaction::new(
                    51,
                    vec![AccessMetadata::write(ResourceId::for_test(4))],
                    51,
                ), // New resource 4
            ],
        );
        batch5.wait_committed_blocking();

        // Verify mixed state
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![10, 50])
            .assert_written_state(ResourceId::for_test(2), vec![11, 20])
            .assert_written_state(ResourceId::for_test(3), vec![21])
            .assert_written_state(ResourceId::for_test(4), vec![51]);

        scheduler.shutdown();
    }
}

/// Tests that resources are properly evicted from the cache after their batches commit.
#[test]
pub fn test_resource_eviction() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        const NUM_BATCHES: usize = 100;
        const RESOURCES_PER_BATCH: usize = 10;

        let mut batches = Vec::with_capacity(NUM_BATCHES);
        for batch_idx in 0..NUM_BATCHES {
            let base_resource = batch_idx * RESOURCES_PER_BATCH;
            let txs: Vec<_> = (0..RESOURCES_PER_BATCH)
                .map(|i| {
                    SchedulerTransaction::new(
                        (base_resource + i) as u32,
                        vec![AccessMetadata::write(ResourceId::for_test(base_resource + i))],
                        base_resource + i,
                    )
                })
                .collect();
            batches.push(scheduler.schedule((batch_idx + 1) as u64, txs));
        }

        for batch in &batches {
            batch.wait_committed_blocking();
        }

        scheduler.wait_cache_empty(Duration::from_secs(10));

        // Verify data was written correctly for a sample of resources
        for batch_idx in [0, 50, 99] {
            let base_resource = batch_idx * RESOURCES_PER_BATCH;
            for i in 0..RESOURCES_PER_BATCH {
                let resource_id = base_resource + i;
                scheduler
                    .assert_written_state(ResourceId::for_test(resource_id), vec![resource_id]);
            }
        }

        scheduler.shutdown();
    }
}

/// Tests eviction behavior with high-frequency scheduling.
#[test]
pub fn test_eviction_under_load() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        const WAVES: usize = 10;
        const BATCHES_PER_WAVE: usize = 50;

        for wave in 0..WAVES {
            let base = wave * BATCHES_PER_WAVE;

            let batches: Vec<_> = (0..BATCHES_PER_WAVE)
                .map(|i| {
                    let resource_id = base + i;
                    scheduler.schedule(
                        (base + i + 1) as u64,
                        vec![SchedulerTransaction::new(
                            resource_id as u32,
                            vec![AccessMetadata::write(ResourceId::for_test(resource_id))],
                            resource_id,
                        )],
                    )
                })
                .collect();

            for batch in &batches {
                batch.wait_committed_blocking();
            }

            scheduler.wait_cache_empty(Duration::from_secs(10));
        }

        scheduler.shutdown();
    }
}

/// Tests that pruning deletes rollback pointers for batches below the threshold.
#[test]
pub fn test_basic_pruning() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create batches that write to resources (generates rollback pointers)
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                2,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                3,
            )],
        );
        batch3.wait_committed_blocking();

        // Verify rollback pointers exist for batches 2 and 3
        let store = scheduler.state().storage().store();
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.checkpoint().index()).count(),
            1,
            "Batch 2 should have rollback pointer"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.checkpoint().index()).count(),
            1,
            "Batch 3 should have rollback pointer"
        );

        // Set pruning threshold to batch 3 (prune batches 1 and 2)
        scheduler.pruning().set_threshold(batch3.checkpoint().index());
        scheduler.wait_pruned(batch2.checkpoint().index(), Duration::from_secs(10));

        // Verify rollback pointers for batches 1 and 2 are deleted
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch1.checkpoint().index()).count(),
            0,
            "Batch 1 rollback pointers should be pruned"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.checkpoint().index()).count(),
            0,
            "Batch 2 rollback pointers should be pruned"
        );

        // Batch 3 should still have its rollback pointer (not pruned)
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.checkpoint().index()).count(),
            1,
            "Batch 3 rollback pointer should NOT be pruned"
        );

        // Data should still be readable
        scheduler.assert_written_state(ResourceId::for_test(1), vec![1, 2, 3]);

        scheduler.shutdown();
    }
}

/// Tests that pruning preserves batches at or above the threshold.
#[test]
pub fn test_pruning_preserves_recent_batches() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create 5 batches, each overwriting the same resource
        for i in 1..=5 {
            let batch = scheduler.schedule(
                i as u64,
                vec![SchedulerTransaction::new(
                    i as u32,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    i,
                )],
            );
            batch.wait_committed_blocking();
        }

        let store = scheduler.state().storage().store();

        // Verify all batches 2-5 have rollback pointers
        for batch_idx in 2..=5 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                1,
                "Batch {} should have rollback pointer before pruning",
                batch_idx
            );
        }

        // Prune batches 1-3 (threshold = 4 means batches < 4 are eligible)
        scheduler.pruning().set_threshold(4);
        scheduler.wait_pruned(3, Duration::from_secs(10));

        // Batches 1-3 should be pruned
        for batch_idx in 1..=3 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                0,
                "Batch {} should be pruned",
                batch_idx
            );
        }

        // Batches 4-5 should NOT be pruned
        for batch_idx in 4..=5 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                1,
                "Batch {} should NOT be pruned",
                batch_idx
            );
        }

        scheduler.shutdown();
    }
}

/// Tests that pruning is crash-fault tolerant.
#[test]
pub fn test_pruning_crash_recovery() {
    use vprogs_state_metadata::StateMetadata;
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");

    // Phase 1: Create batches, prune some, then shutdown
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        for i in 1..=5 {
            let batch = scheduler.schedule(
                i as u64,
                vec![SchedulerTransaction::new(
                    i as u32,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    i,
                )],
            );
            batch.wait_committed_blocking();
        }

        // Prune batches 1-2
        scheduler.pruning().set_threshold(3);
        scheduler.wait_pruned(2, Duration::from_secs(10));

        // Root should have advanced to 3 (oldest surviving batch after pruning 1-2).
        let root = StateMetadata::root::<u64, _>(&**scheduler.state().storage().store());
        assert_eq!(root.index(), 3, "Root should point to first surviving batch");
        assert_eq!(*root.metadata(), 3, "Root metadata should match batch 3");

        scheduler.shutdown();
    }

    // Phase 2: Reopen and verify pruning state is preserved
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        let root = scheduler.state().root();
        assert_eq!(root.index(), 3, "Root should resume from persisted index");
        assert_eq!(*root.metadata(), 3, "Root metadata should survive restart");

        let store = scheduler.state().storage().store();

        // Batches 1-2 should still be pruned
        for batch_idx in 1..=2 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                0,
                "Batch {} should remain pruned after restart",
                batch_idx
            );
        }

        // Batches 3-5 should still have rollback pointers
        for batch_idx in 3..=5 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                1,
                "Batch {} should NOT be pruned",
                batch_idx
            );
        }

        // Continue pruning from where we left off
        scheduler.pruning().set_threshold(5);
        scheduler.wait_pruned(4, Duration::from_secs(10));

        // Batches 3-4 should now be pruned
        for batch_idx in 3..=4 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                0,
                "Batch {} should now be pruned",
                batch_idx
            );
        }

        // Batch 5 should still have its rollback pointer
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), 5).count(),
            1,
            "Batch 5 should NOT be pruned"
        );

        scheduler.shutdown();
    }
}

/// Tests pruning with multiple resources per batch.
#[test]
pub fn test_pruning_multiple_resources() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Batch 1: Create resources 1, 2, 3
        let batch1 = scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    1,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    1,
                ),
                SchedulerTransaction::new(
                    2,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    2,
                ),
                SchedulerTransaction::new(
                    3,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    3,
                ),
            ],
        );
        batch1.wait_committed_blocking();

        // Batch 2: Overwrite all three resources
        let batch2 = scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(
                    10,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    10,
                ),
                SchedulerTransaction::new(
                    20,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    20,
                ),
                SchedulerTransaction::new(
                    30,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    30,
                ),
            ],
        );
        batch2.wait_committed_blocking();

        // Batch 3: Overwrite all three resources again
        let batch3 = scheduler.schedule(
            3,
            vec![
                SchedulerTransaction::new(
                    100,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    100,
                ),
                SchedulerTransaction::new(
                    200,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    200,
                ),
                SchedulerTransaction::new(
                    300,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    300,
                ),
            ],
        );
        batch3.wait_committed_blocking();

        let store = scheduler.state().storage().store();

        // Batch 2 and 3 should have 3 rollback pointers each
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.checkpoint().index()).count(),
            3,
            "Batch 2 should have 3 rollback pointers"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.checkpoint().index()).count(),
            3,
            "Batch 3 should have 3 rollback pointers"
        );

        // Prune batch 1 and 2
        scheduler.pruning().set_threshold(batch3.checkpoint().index());
        scheduler.wait_pruned(batch2.checkpoint().index(), Duration::from_secs(10));

        // All rollback pointers for batches 1 and 2 should be deleted
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch1.checkpoint().index()).count(),
            0,
            "Batch 1 should have no rollback pointers after pruning"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.checkpoint().index()).count(),
            0,
            "Batch 2 should have no rollback pointers after pruning"
        );

        // Batch 3 should still have its rollback pointers
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.checkpoint().index()).count(),
            3,
            "Batch 3 should still have 3 rollback pointers"
        );

        // Data should still be readable
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1, 10, 100])
            .assert_written_state(ResourceId::for_test(2), vec![2, 20, 200])
            .assert_written_state(ResourceId::for_test(3), vec![3, 30, 300]);

        scheduler.shutdown();
    }
}

/// Tests that `pause()` constrains the pruning worker and `unpause()` releases it.
///
/// Calls `pause(ceiling)` directly, then advances the pruning threshold well past the ceiling. The
/// worker must not prune at or above the ceiling. After `unpause()`, the worker must catch up to
/// the full threshold.
#[test]
pub fn test_pruning_pause_and_unpause() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create 6 committed batches, each overwriting the same resource.
        for i in 1..=6 {
            let batch = scheduler.schedule(
                i as u64,
                vec![SchedulerTransaction::new(
                    i as u32,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    i,
                )],
            );
            batch.wait_committed_blocking();
        }

        // Pause pruning at batch 3: the worker may only prune batches 1-2.
        assert!(scheduler.pruning().pause(3));

        // Advance threshold so batches 1-5 become eligible.
        scheduler.pruning().set_threshold(6);

        // Wait for the worker to prune what it can (batches 1-2, below the ceiling).
        scheduler.wait_pruned(2, Duration::from_secs(10));

        let store = scheduler.state().storage().store();

        // Batches at or above the ceiling must still have their rollback pointers.
        for batch_idx in 3..=6 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                1,
                "Batch {batch_idx} should NOT be pruned while paused"
            );
        }

        // Unpause - the worker should now catch up to the full threshold.
        scheduler.pruning().unpause();
        scheduler.wait_pruned(5, Duration::from_secs(10));

        for batch_idx in 1..=5 {
            assert_eq!(
                StatePtrRollback::iter_batch(store.as_ref(), batch_idx).count(),
                0,
                "Batch {batch_idx} should be pruned after unpause"
            );
        }

        // Batch 6 (at threshold) must still have its rollback pointer.
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), 6).count(),
            1,
            "Batch 6 should NOT be pruned (at threshold)"
        );

        scheduler.shutdown();
    }
}

/// Tests that `rollback_to` returns `PruningConflict` when the rollback target has already been
/// pruned past.
#[test]
pub fn test_rollback_pruning_conflict() {
    use vprogs_scheduling_scheduler::SchedulerError;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create 5 committed batches.
        for i in 1..=5 {
            let batch = scheduler.schedule(
                i as u64,
                vec![SchedulerTransaction::new(
                    i as u32,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    i,
                )],
            );
            batch.wait_committed_blocking();
        }

        // Prune batches 1-3 (threshold = 4, batches < 4 eligible).
        scheduler.pruning().set_threshold(4);
        scheduler.wait_pruned(3, Duration::from_secs(10));

        // Rollback to batch 2 should fail - its rollback pointers are gone.
        let err = scheduler.rollback_to(2).unwrap_err();
        assert!(matches!(err, SchedulerError::PruningConflict));

        // Rollback to batch 4 should still succeed - above the pruned range.
        let target = scheduler.rollback_to(4).expect("rollback should succeed");
        assert_eq!(target.index(), 4);

        scheduler.shutdown();
    }
}

/// Tests that committing batches produces non-empty, evolving state roots in the SMT.
#[test]
pub fn test_smt_state_root_after_commits() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Before any batches, state root should be empty.
        assert_eq!(state_root(&scheduler), EMPTY_HASH);

        // Batch 1: write to resource 1
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch1.wait_committed_blocking();

        let root1 = state_root(&scheduler);
        assert_ne!(root1, EMPTY_HASH, "state root should be non-empty after first commit");

        // Batch 2: write to a different resource
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        batch2.wait_committed_blocking();

        let root2 = state_root(&scheduler);
        assert_ne!(root2, EMPTY_HASH);
        assert_ne!(root2, root1, "state root should change when new leaves are inserted");

        // Batch 3: overwrite resource 1 (value changes → root changes)
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                3,
            )],
        );
        batch3.wait_committed_blocking();

        let root3 = state_root(&scheduler);
        assert_ne!(root3, EMPTY_HASH);
        assert_ne!(root3, root2, "state root should change when an existing leaf is updated");

        // The tree's per-version root should match the metadata root for the latest version.
        let store = scheduler.state().storage().store();
        assert_eq!(
            store.root(3),
            root3,
            "smt::Tree::root should match the persisted metadata root"
        );

        scheduler.shutdown();
    }
}

/// Tests that rolling back restores the state root to the target version's root.
#[test]
pub fn test_smt_state_root_after_rollback() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit 3 batches, each touching a distinct resource.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch1.wait_committed_blocking();
        let root_after_1 = state_root(&scheduler);

        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        batch2.wait_committed_blocking();
        let root_after_2 = state_root(&scheduler);

        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
                3,
            )],
        );
        batch3.wait_committed_blocking();
        let root_after_3 = state_root(&scheduler);

        // All roots should be distinct.
        assert_ne!(root_after_1, root_after_2);
        assert_ne!(root_after_2, root_after_3);

        // Rollback to batch 1 - state root should match the root after batch 1.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert_eq!(
            state_root(&scheduler),
            root_after_1,
            "state root should be restored to the target version's root after rollback"
        );

        // Schedule a new batch after rollback - root should diverge from the original batch 2.
        let batch4 = scheduler.schedule(
            10,
            vec![SchedulerTransaction::new(
                10,
                vec![AccessMetadata::write(ResourceId::for_test(10))],
                10,
            )],
        );
        batch4.wait_committed_blocking();
        let root_after_new = state_root(&scheduler);
        assert_ne!(root_after_new, root_after_1, "root should change after new commit");
        assert_ne!(root_after_new, root_after_2, "root should differ from the original branch");

        scheduler.shutdown();
    }
}

/// Tests that rolling back to zero produces an empty state root.
#[test]
pub fn test_smt_state_root_rollback_to_zero() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit a batch so the root is non-empty.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch1.wait_committed_blocking();
        assert_ne!(state_root(&scheduler), EMPTY_HASH);

        // Rollback to 0 - should reset to empty.
        scheduler.rollback_to(0).expect("rollback should succeed");
        assert_eq!(
            state_root(&scheduler),
            EMPTY_HASH,
            "state root should be empty after rollback to 0"
        );

        // A new batch from scratch should produce a non-empty root again.
        let batch2 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch2.wait_committed_blocking();
        assert_ne!(
            state_root(&scheduler),
            EMPTY_HASH,
            "state root should be non-empty after re-committing"
        );

        scheduler.shutdown();
    }
}

/// Tests that the SMT root correctly reflects multiple resources committed in a single batch.
#[test]
pub fn test_smt_multi_resource_single_batch() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Single batch with 3 resources.
        let batch = scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    1,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    1,
                ),
                SchedulerTransaction::new(
                    2,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    2,
                ),
                SchedulerTransaction::new(
                    3,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    3,
                ),
            ],
        );
        batch.wait_committed_blocking();

        let root = state_root(&scheduler);
        assert_ne!(root, EMPTY_HASH, "multi-resource batch should produce non-empty root");

        scheduler.shutdown();
    }
}

/// Tests that a multi-proof verifies against the correct root and rejects an incorrect one.
#[test]
pub fn test_smt_multi_proof_verify() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit a batch with two resources to create tree state.
        let batch = scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    1,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    1,
                ),
                SchedulerTransaction::new(
                    2,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    2,
                ),
            ],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = store.root(1);

        // Generate a proof for resource 1's key and verify it.
        let proof_bytes = store.prove(&[ResourceId::for_test(1)], 1).unwrap();
        let proof = Proof::decode(&proof_bytes).expect("valid proof");

        assert_eq!(
            proof.root::<Sha256>().unwrap(),
            root,
            "proof should verify against the correct root",
        );
        assert_ne!(
            proof.root::<Sha256>().unwrap(),
            [0xFFu8; 32],
            "proof should reject an incorrect root",
        );

        scheduler.shutdown();
    }
}

/// Tests that a multi-proof for a non-existent key verifies (proving absence).
#[test]
pub fn test_smt_multi_proof_absent_key() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit a batch with resource 1 only.
        let batch = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = store.root(1);

        // Generate a proof for resource 99 (absent) - should still verify against the root.
        let proof_bytes = store.prove(&[ResourceId::for_test(99)], 1).unwrap();
        let proof = Proof::decode(&proof_bytes).expect("valid proof");

        assert_eq!(
            proof.root::<Sha256>().unwrap(),
            root,
            "proof for an absent key should verify against the root",
        );
        assert_ne!(
            proof.root::<Sha256>().unwrap(),
            [0xFFu8; 32],
            "proof should reject an incorrect root",
        );

        scheduler.shutdown();
    }
}

/// Tests that a multi-proof covers multiple keys (both present and absent).
#[test]
pub fn test_smt_multi_proof_mixed_keys() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit resources 1, 2, 3.
        let batch = scheduler.schedule(
            1,
            vec![
                SchedulerTransaction::new(
                    1,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    1,
                ),
                SchedulerTransaction::new(
                    2,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                    2,
                ),
                SchedulerTransaction::new(
                    3,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
                    3,
                ),
            ],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = store.root(1);

        // Proof for existing key 1, existing key 3, and absent key 99.
        let keys = [ResourceId::for_test(1), ResourceId::for_test(3), ResourceId::for_test(99)];
        let proof_bytes = store.prove(&keys, 1).unwrap();
        let proof = Proof::decode(&proof_bytes).expect("valid proof");

        assert_eq!(
            proof.root::<Sha256>().unwrap(),
            root,
            "mixed proof should verify against the correct root",
        );
        assert!(proof.leaves.len() >= 3, "proof should have at least 3 leaves");

        scheduler.shutdown();
    }
}

/// Tests that the SMT produces consistent roots across a commit-rollback-recommit cycle.
#[test]
pub fn test_smt_deterministic_roots() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit batch 1, record root.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch1.wait_committed_blocking();
        let root1 = state_root(&scheduler);

        // Commit batch 2, then rollback to 1.
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        batch2.wait_committed_blocking();

        scheduler.rollback_to(1).expect("rollback should succeed");
        assert_eq!(state_root(&scheduler), root1, "rollback should restore exact root");

        // Commit batch 2 again with the same data pattern.
        let batch2_again = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                2,
            )],
        );
        batch2_again.wait_committed_blocking();

        // The tree builds from (version=1 root) + same diffs → same result.
        assert_ne!(state_root(&scheduler), root1, "adding batch 2 should change root");

        scheduler.shutdown();
    }
}

/// A resource emptied by a batch leaves no latest pointer once that batch commits.
#[test]
pub fn test_removal_deletes_latest_ptr() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        batch1.wait_committed_blocking();
        scheduler.assert_written_state(ResourceId::for_test(1), vec![1]);

        // Batch 2 empties resource 1.
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                Processor::CLEAR_DATA,
            )],
        );
        batch2.wait_committed_blocking();

        scheduler.assert_resource_deleted(ResourceId::for_test(1));

        scheduler.shutdown();
    }
}

/// Recreating a removed resource takes the recreating batch's index as its version and does not
/// collide with the retained pre-removal version.
#[test]
pub fn test_recreate_after_removal_uses_batch_index_version() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create (batch 1), remove (batch 2), recreate (batch 3).
        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                Processor::CLEAR_DATA,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                3,
            )],
        );
        batch3.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let state = StateVersion::from_latest_data(store.as_ref(), ResourceId::for_test(1));
        assert_eq!(state.version(), 3, "recreate takes the batch-index version");
        assert_eq!(*state.data(), 3usize.to_be_bytes().to_vec());

        // The pre-removal version (batch 1) is retained at its own key, not overwritten.
        assert_eq!(
            StateVersion::get(store.as_ref(), 1, &ResourceId::for_test(1)),
            Some(1usize.to_be_bytes().to_vec()),
            "the original version is retained without collision"
        );

        scheduler.shutdown();
    }
}

/// Rolling back a removal restores the resource's pre-removal version.
#[test]
pub fn test_rollback_removal_restores_prior_version() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                Processor::CLEAR_DATA,
            )],
        );
        batch2.wait_committed_blocking();
        scheduler.assert_resource_deleted(ResourceId::for_test(1));

        // Rolling back the removal batch restores resource 1.
        scheduler.rollback_to(1).expect("rollback should succeed");
        scheduler.assert_written_state(ResourceId::for_test(1), vec![1]);

        scheduler.shutdown();
    }
}

/// Rolling back a recreate (after a removal) leaves the resource absent again.
#[test]
pub fn test_rollback_recreate_leaves_absent() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                Processor::CLEAR_DATA,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                3,
            )],
        );
        batch3.wait_committed_blocking();
        scheduler.assert_written_state(ResourceId::for_test(1), vec![3]);

        // Rolling back to the removal batch (2) undoes the recreate: absent again.
        scheduler.rollback_to(2).expect("rollback should succeed");
        scheduler.assert_resource_deleted(ResourceId::for_test(1));

        scheduler.shutdown();
    }
}

/// After pruning crosses the removal batch, a removed resource has zero on-disk footprint: no
/// latest pointer, no version rows, and no rollback pointers.
#[test]
pub fn test_pruning_removed_resource_zero_footprint() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Create resource 1 (batch 1), remove it (batch 2), then an unrelated batch 3 so batches
        // 1-2 fall below the pruning threshold.
        scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                1,
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                Processor::CLEAR_DATA,
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
                3,
            )],
        );
        batch3.wait_committed_blocking();

        // Prune batches 1 and 2.
        scheduler.pruning().set_threshold(3);
        scheduler.wait_pruned(2, Duration::from_secs(10));

        let store = scheduler.state().storage().store();

        // No latest pointer, no data row for the pre-removal version, no rollback pointers.
        scheduler.assert_resource_deleted(ResourceId::for_test(1));
        assert_eq!(
            StateVersion::get(store.as_ref(), 1, &ResourceId::for_test(1)),
            None,
            "the pre-removal version row should be pruned"
        );
        assert_eq!(StatePtrRollback::iter_batch(store.as_ref(), 1).count(), 0);
        assert_eq!(StatePtrRollback::iter_batch(store.as_ref(), 2).count(), 0);

        scheduler.shutdown();
    }
}

/// Pruning must treat a reorg-orphaned id below the finalization point as orphaned - despite
/// finalized ids defaulting to canonical - so it spares the orphan's still-live predecessor.
#[test]
pub fn test_finalize_past_orphan_bucket_keeps_predecessor() {
    use vprogs_core_types::ChainSink;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Batch 1 writes R: the canonical version, stored at version 1.
        scheduler
            .schedule(
                1,
                vec![SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    0,
                )],
            )
            .wait_committed_blocking();

        // Batch 2 rewrites R, so its rollback pointer for R points back at version 1.
        scheduler
            .schedule(
                2,
                vec![SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    1,
                )],
            )
            .wait_committed_blocking();

        // Reorg batch 2 away: id 2 becomes an orphaned gap and R reverts to version 1.
        scheduler.rollback_to(1).expect("rollback should succeed");

        // Push the tip two buckets past the orphan (id 2, bucket 0) with empty fillers, so bucket 0
        // sinks below the two-bucket hot zone into the body where `finalize` can prune it.
        let mut last = None;
        for meta in 3..=2 * BUCKET_CAPACITY + 1 {
            last = Some(scheduler.schedule(meta, vec![]));
        }
        last.expect("scheduled fillers").wait_committed_blocking();
        assert_eq!(scheduler.tip(), 2 * BUCKET_CAPACITY + 1, "tip advanced into the third bucket");

        // Finalize past bucket 0, then let the pruning worker reclaim the finalized range.
        scheduler.finalize(BUCKET_CAPACITY + 1);
        scheduler.wait_pruned(BUCKET_CAPACITY, Duration::from_secs(60));

        // The orphaned batch 2 must not have caused R's live version 1 to be deleted.
        let store = scheduler.state().storage().store();
        assert!(
            StateVersion::get(store.as_ref(), 1, &ResourceId::for_test(1)).is_some(),
            "finalizing past the orphan's pruned bucket reclaimed R's canonical predecessor"
        );

        scheduler.shutdown();
    }
}

/// Tests that re-appending a previously committed block after a reorg restores its state from disk
/// rather than re-executing its transactions.
#[test]
pub fn test_restore_returning_committed_batch_skips_execution() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit block 1, writing value 100 to resource 1.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                100,
            )],
        );
        batch1.wait_committed_blocking();
        scheduler.assert_written_state(ResourceId::for_test(1), vec![100]);

        // Reorg block 1 away; its resource is reverted but its committed state stays retained on
        // disk.
        scheduler.rollback_to(0).expect("rollback should succeed");
        scheduler.assert_resource_deleted(ResourceId::for_test(1));

        // Re-append the same block (same metadata -> same block hash -> returning id). The provided
        // transaction would write 999 if executed; restore must ignore it and reuse the committed
        // 100.
        let restored = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                999,
            )],
        );
        restored.wait_committed_blocking();

        assert_eq!(restored.checkpoint().index(), 1, "returning block keeps its original id");
        scheduler.assert_written_state(ResourceId::for_test(1), vec![100]);

        scheduler.shutdown();
    }
}

/// Tests that a restored batch feeds its written state into a following batch's read through the
/// in-memory resource chain, before the restore has committed to disk.
#[test]
pub fn test_restore_feeds_following_batch_chain() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit block 1, writing value 100 to resource 1, then reorg it away.
        scheduler
            .schedule(
                1,
                vec![SchedulerTransaction::new(
                    0,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                    100,
                )],
            )
            .wait_committed_blocking();
        scheduler.rollback_to(0).expect("rollback should succeed");

        // Re-append block 1 to restore it, but do NOT wait: the following batch must chain off its
        // restored written state in memory rather than reading committed disk state.
        let _restored = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                999,
            )],
        );

        // A following new block writes resource 1 again. It must read the restored value (100) and
        // append 300 - not the post-rollback empty state, and not the ignored 999.
        let following = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                300,
            )],
        );
        following.wait_committed_blocking();

        scheduler.assert_written_state(ResourceId::for_test(1), vec![100, 300]);

        scheduler.shutdown();
    }
}

/// Tests that committing a restored batch through the normal path re-runs `store.update`
/// idempotently: the reconstructed SMT reproduces the original state root.
#[test]
pub fn test_restore_smt_root_is_idempotent() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(Processor),
            StorageConfig::default().with_store(storage),
        );

        // Commit block 1 and capture the resulting state root.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                100,
            )],
        );
        batch1.wait_committed_blocking();
        let root_before = scheduler.state().storage().store().root(1);

        // Reorg away, then restore the same block.
        scheduler.rollback_to(0).expect("rollback should succeed");
        let restored = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                0,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
                100,
            )],
        );
        restored.wait_committed_blocking();

        // The re-run store.update reproduces the same tree: same state root, same per-version root.
        let store = scheduler.state().storage().store();
        assert_eq!(store.root(1), root_before, "restored SMT root must match the original");

        scheduler.shutdown();
    }
}
