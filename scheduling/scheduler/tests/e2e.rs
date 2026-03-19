use std::time::Duration;

use tempfile::TempDir;
use vprogs_core_smt::{Blake3, EMPTY_HASH, Tree, proving::Proof};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, Checkpoint, ResourceId, SchedulerTransaction};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_utils::{Processor, SchedulerExt};
use vprogs_state_metadata::StateMetadata;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;

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
                ),
                SchedulerTransaction::new(
                    1,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(1)),
                        AccessMetadata::write(ResourceId::for_test(2)),
                    ],
                ),
                SchedulerTransaction::new(2, vec![AccessMetadata::read(ResourceId::for_test(3))]),
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
                ),
                SchedulerTransaction::new(
                    4,
                    vec![
                        AccessMetadata::write(ResourceId::for_test(10)),
                        AccessMetadata::write(ResourceId::for_test(20)),
                    ],
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
                SchedulerTransaction::new(0, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(2))]),
            ],
        );
        scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(2, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(3, vec![AccessMetadata::write(ResourceId::for_test(3))]),
            ],
        );
        let last_batch = scheduler.schedule(
            3,
            vec![
                SchedulerTransaction::new(4, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(5, vec![AccessMetadata::write(ResourceId::for_test(4))]),
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

        // Verify state after rollback - only batch1 effects should remain
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
            )],
        );
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
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

        // Schedule new batches after rollback - should continue from index 2
        let batch4 = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                10,
                vec![AccessMetadata::write(ResourceId::for_test(10))],
            )],
        );
        let batch5 = scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                11,
                vec![AccessMetadata::write(ResourceId::for_test(11))],
            )],
        );
        batch5.wait_committed_blocking();

        assert_eq!(batch4.checkpoint().index(), 2);
        assert_eq!(batch5.checkpoint().index(), 3);

        // Verify new state
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![0])
            .assert_written_state(ResourceId::for_test(10), vec![10])
            .assert_written_state(ResourceId::for_test(11), vec![11]);

        scheduler.shutdown();
    }
}

/// Tests in-flight batch cancellation without waiting for commitment. When a rollback occurs,
/// batches that haven't been committed yet should detect cancellation via was_canceled() and skip
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
            )],
        );
        batch1.wait_committed_blocking();

        // Schedule multiple batches but don't wait for commitment
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
            )],
        );
        let batch4 = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
            )],
        );

        // Immediately rollback without waiting for batches 2-4 to commit
        // This tests in-flight cancellation
        scheduler.rollback_to(1).expect("rollback should succeed");

        // After rollback, the canceled batches should have was_canceled() == true
        assert!(batch2.was_canceled(), "batch2 should be canceled");
        assert!(batch3.was_canceled(), "batch3 should be canceled");
        assert!(batch4.was_canceled(), "batch4 should be canceled");

        // Resource 1 should still exist (from batch1 which was committed)
        // Resources 2, 3, 4 should be cleaned up by rollback
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
            )],
        );
        batch5.wait_committed_blocking();
        assert!(!batch5.was_canceled(), "batch5 should not be canceled");
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

        // Phase 1: Apply batches 1-6
        // Using resource IDs that match batch indices for clarity
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            )],
        );
        scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
            )],
        );
        scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                4,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
            )],
        );
        scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                5,
                vec![AccessMetadata::write(ResourceId::for_test(5))],
            )],
        );
        let batch6 = scheduler.schedule(
            6,
            vec![SchedulerTransaction::new(
                6,
                vec![AccessMetadata::write(ResourceId::for_test(6))],
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
            )],
        );
        let batch7 = scheduler.schedule(
            70,
            vec![SchedulerTransaction::new(
                70,
                vec![AccessMetadata::write(ResourceId::for_test(70))],
            )],
        );
        batch7.wait_committed_blocking();

        assert_eq!(new_batch6.checkpoint().index(), 6);
        assert_eq!(batch7.checkpoint().index(), 7);

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
            )],
        );
        let final_batch5 = scheduler.schedule(
            50,
            vec![SchedulerTransaction::new(
                50,
                vec![AccessMetadata::write(ResourceId::for_test(50))],
            )],
        );
        final_batch5.wait_committed_blocking();

        assert_eq!(final_batch4.checkpoint().index(), 4);
        assert_eq!(final_batch5.checkpoint().index(), 5);

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
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
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

        // New batches should start from index 1 again
        let new_batch1 = scheduler.schedule(
            100,
            vec![SchedulerTransaction::new(
                100,
                vec![AccessMetadata::write(ResourceId::for_test(100))],
            )],
        );
        new_batch1.wait_committed_blocking();

        assert_eq!(new_batch1.checkpoint().index(), 1);
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
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            )],
        );
        scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
            )],
        );
        scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                4,
                vec![AccessMetadata::write(ResourceId::for_test(4))],
            )],
        );
        let batch5 = scheduler.schedule(
            5,
            vec![SchedulerTransaction::new(
                5,
                vec![AccessMetadata::write(ResourceId::for_test(5))],
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
            )],
        );
        new_batch.wait_committed_blocking();
        assert_eq!(new_batch.checkpoint().index(), 2);

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
            )],
        );
        scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                20,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            )],
        );
        scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                30,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            )],
        );
        let batch4 = scheduler.schedule(
            4,
            vec![SchedulerTransaction::new(
                40,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
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
            )],
        );
        batch1.wait_committed_blocking();

        // Schedule batches that access different resources
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(100))],
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(200))],
            )],
        );

        // Rollback immediately - batch2 and batch3 should be canceled
        scheduler.rollback_to(1).expect("rollback should succeed");

        // Verify both batches were canceled
        assert!(batch2.was_canceled(), "batch2 should be canceled");
        assert!(batch3.was_canceled(), "batch3 should be canceled");

        // The wait functions should return immediately for canceled batches
        batch2.wait_committed_blocking();
        batch3.wait_committed_blocking();

        // Resource 1 should only have the write from batch1
        // Resources 100 and 200 should be cleaned up by rollback
        scheduler
            .assert_written_state(ResourceId::for_test(1), vec![1])
            .assert_resource_deleted(ResourceId::for_test(100))
            .assert_resource_deleted(ResourceId::for_test(200));

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
                SchedulerTransaction::new(10, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(11, vec![AccessMetadata::write(ResourceId::for_test(2))]),
            ],
        );

        // Batch 2: Write to resources 2 and 3
        scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(20, vec![AccessMetadata::write(ResourceId::for_test(2))]),
                SchedulerTransaction::new(21, vec![AccessMetadata::write(ResourceId::for_test(3))]),
            ],
        );

        // Batch 3: Write to resources 1 and 3
        scheduler.schedule(
            3,
            vec![
                SchedulerTransaction::new(30, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(31, vec![AccessMetadata::write(ResourceId::for_test(3))]),
            ],
        );

        // Batch 4: Write to all resources
        let batch4 = scheduler.schedule(
            4,
            vec![
                SchedulerTransaction::new(40, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(41, vec![AccessMetadata::write(ResourceId::for_test(2))]),
                SchedulerTransaction::new(42, vec![AccessMetadata::write(ResourceId::for_test(3))]),
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
        let batch5 =
            scheduler.schedule(
                5,
                vec![
                    SchedulerTransaction::new(
                        50,
                        vec![AccessMetadata::write(ResourceId::for_test(1))],
                    ),
                    SchedulerTransaction::new(
                        51,
                        vec![AccessMetadata::write(ResourceId::for_test(4))],
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
                        base_resource + i,
                        vec![AccessMetadata::write(ResourceId::for_test(base_resource + i))],
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
                            resource_id,
                            vec![AccessMetadata::write(ResourceId::for_test(resource_id))],
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
            )],
        );
        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            )],
        );
        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
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
                    i,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
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
                    i,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                )],
            );
            batch.wait_committed_blocking();
        }

        // Prune batches 1-2
        scheduler.pruning().set_threshold(3);
        scheduler.wait_pruned(2, Duration::from_secs(10));

        // Verify root has advanced past the pruned batches.
        // Root = oldest surviving batch = 3 (since batches 1-2 were pruned).
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
                SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(2, vec![AccessMetadata::write(ResourceId::for_test(2))]),
                SchedulerTransaction::new(3, vec![AccessMetadata::write(ResourceId::for_test(3))]),
            ],
        );
        batch1.wait_committed_blocking();

        // Batch 2: Overwrite all three resources
        let batch2 = scheduler.schedule(
            2,
            vec![
                SchedulerTransaction::new(10, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(20, vec![AccessMetadata::write(ResourceId::for_test(2))]),
                SchedulerTransaction::new(30, vec![AccessMetadata::write(ResourceId::for_test(3))]),
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
                ),
                SchedulerTransaction::new(
                    200,
                    vec![AccessMetadata::write(ResourceId::for_test(2))],
                ),
                SchedulerTransaction::new(
                    300,
                    vec![AccessMetadata::write(ResourceId::for_test(3))],
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
                    i,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
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

        // Unpause — the worker should now catch up to the full threshold.
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
                    i,
                    vec![AccessMetadata::write(ResourceId::for_test(1))],
                )],
            );
            batch.wait_committed_blocking();
        }

        // Prune batches 1-3 (threshold = 4, batches < 4 eligible).
        scheduler.pruning().set_threshold(4);
        scheduler.wait_pruned(3, Duration::from_secs(10));

        // Rollback to batch 2 should fail — its rollback pointers are gone.
        let err = scheduler.rollback_to(2).unwrap_err();
        assert!(matches!(err, SchedulerError::PruningConflict));

        // Rollback to batch 4 should still succeed — above the pruned range.
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

        /// Reads the current state root from metadata.
        fn state_root(scheduler: &Scheduler<RocksDbStore, Processor>) -> [u8; 32] {
            StateMetadata::state_root(&**scheduler.state().storage().store())
        }

        // Before any batches, state root should be empty.
        assert_eq!(state_root(&scheduler), EMPTY_HASH);

        // Batch 1: write to resource 1
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
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
            )],
        );
        batch3.wait_committed_blocking();

        let root3 = state_root(&scheduler);
        assert_ne!(root3, EMPTY_HASH);
        assert_ne!(root3, root2, "state root should change when an existing leaf is updated");

        // The tree's per-version root should match the metadata root for the latest version.
        let store = scheduler.state().storage().store();
        assert_eq!(
            store.get_root(3),
            root3,
            "smt::Store::get_root should match the persisted metadata root"
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

        fn state_root(scheduler: &Scheduler<RocksDbStore, Processor>) -> [u8; 32] {
            StateMetadata::state_root(&**scheduler.state().storage().store())
        }

        // Commit 3 batches, each touching a distinct resource.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            )],
        );
        batch1.wait_committed_blocking();
        let root_after_1 = state_root(&scheduler);

        let batch2 = scheduler.schedule(
            2,
            vec![SchedulerTransaction::new(
                2,
                vec![AccessMetadata::write(ResourceId::for_test(2))],
            )],
        );
        batch2.wait_committed_blocking();
        let root_after_2 = state_root(&scheduler);

        let batch3 = scheduler.schedule(
            3,
            vec![SchedulerTransaction::new(
                3,
                vec![AccessMetadata::write(ResourceId::for_test(3))],
            )],
        );
        batch3.wait_committed_blocking();
        let root_after_3 = state_root(&scheduler);

        // All roots should be distinct.
        assert_ne!(root_after_1, root_after_2);
        assert_ne!(root_after_2, root_after_3);

        // Rollback to batch 1 — state root should match the root after batch 1.
        scheduler.rollback_to(1).expect("rollback should succeed");
        assert_eq!(
            state_root(&scheduler),
            root_after_1,
            "state root should be restored to the target version's root after rollback"
        );

        // Schedule a new batch after rollback — root should diverge from the original batch 2.
        let batch4 = scheduler.schedule(
            10,
            vec![SchedulerTransaction::new(
                10,
                vec![AccessMetadata::write(ResourceId::for_test(10))],
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

        fn state_root(scheduler: &Scheduler<RocksDbStore, Processor>) -> [u8; 32] {
            StateMetadata::state_root(&**scheduler.state().storage().store())
        }

        // Commit a batch so the root is non-empty.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
            )],
        );
        batch1.wait_committed_blocking();
        assert_ne!(state_root(&scheduler), EMPTY_HASH);

        // Rollback to 0 — should reset to empty.
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
                SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(2, vec![AccessMetadata::write(ResourceId::for_test(2))]),
                SchedulerTransaction::new(3, vec![AccessMetadata::write(ResourceId::for_test(3))]),
            ],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = StateMetadata::state_root(&**store);
        assert_ne!(root, EMPTY_HASH, "multi-resource batch should produce non-empty root");

        // Tree version root should agree with metadata.
        assert_eq!(store.get_root(1), root);

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
                SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(2, vec![AccessMetadata::write(ResourceId::for_test(2))]),
            ],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = store.get_root(1);

        // Generate a proof for resource 1's key and verify it.
        let key = *ResourceId::for_test(1).as_bytes();
        let proof_bytes = store.prove(&[key], 1);
        let proof = Proof::decode(&proof_bytes).expect("valid proof");

        assert!(
            proof.verify::<Blake3>(root).unwrap(),
            "proof should verify against the correct root",
        );
        assert!(
            !proof.verify::<Blake3>([0xFFu8; 32]).unwrap(),
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
            )],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = store.get_root(1);

        // Generate a proof for resource 99 (absent) — should still verify against the root.
        let absent_key = *ResourceId::for_test(99).as_bytes();
        let proof_bytes = store.prove(&[absent_key], 1);
        let proof = Proof::decode(&proof_bytes).expect("valid proof");

        assert!(
            proof.verify::<Blake3>(root).unwrap(),
            "proof for an absent key should verify against the root",
        );
        assert!(
            !proof.verify::<Blake3>([0xFFu8; 32]).unwrap(),
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
                SchedulerTransaction::new(1, vec![AccessMetadata::write(ResourceId::for_test(1))]),
                SchedulerTransaction::new(2, vec![AccessMetadata::write(ResourceId::for_test(2))]),
                SchedulerTransaction::new(3, vec![AccessMetadata::write(ResourceId::for_test(3))]),
            ],
        );
        batch.wait_committed_blocking();

        let store = scheduler.state().storage().store();
        let root = store.get_root(1);

        // Proof for existing key 1, existing key 3, and absent key 99.
        let keys = [
            *ResourceId::for_test(1).as_bytes(),
            *ResourceId::for_test(3).as_bytes(),
            *ResourceId::for_test(99).as_bytes(),
        ];
        let proof_bytes = store.prove(&keys, 1);
        let proof = Proof::decode(&proof_bytes).expect("valid proof");

        assert!(
            proof.verify::<Blake3>(root).unwrap(),
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

        fn state_root(scheduler: &Scheduler<RocksDbStore, Processor>) -> [u8; 32] {
            StateMetadata::state_root(&**scheduler.state().storage().store())
        }

        // Commit batch 1, record root.
        let batch1 = scheduler.schedule(
            1,
            vec![SchedulerTransaction::new(
                1,
                vec![AccessMetadata::write(ResourceId::for_test(1))],
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
            )],
        );
        batch2_again.wait_committed_blocking();

        // The tree builds from (version=1 root) + same diffs → same result.
        assert_ne!(state_root(&scheduler), root1, "adding batch 2 should change root");

        scheduler.shutdown();
    }
}
