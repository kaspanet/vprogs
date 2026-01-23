use std::time::Duration;

use tempfile::TempDir;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_suite::{Access, SchedulerExt, Tx, VM};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;

#[test]
pub fn test_runtime() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        let batch1 = runtime.schedule(vec![
            Tx(0, vec![Access::Write(1), Access::Read(3)]),
            Tx(1, vec![Access::Write(1), Access::Write(2)]),
            Tx(2, vec![Access::Read(3)]),
        ]);

        let batch2 = runtime.schedule(vec![
            Tx(3, vec![Access::Write(1), Access::Read(3)]),
            Tx(4, vec![Access::Write(10), Access::Write(20)]),
        ]);

        batch1.wait_committed_blocking();
        batch2.wait_committed_blocking();

        runtime
            .assert_written_state(1, vec![0, 1, 3])
            .assert_written_state(2, vec![1])
            .assert_written_state(3, vec![])
            .assert_written_state(10, vec![4])
            .assert_written_state(20, vec![4]);

        runtime.shutdown();
    }
}

/// Tests rollback of committed batches.
#[test]
pub fn test_rollback_committed() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        runtime.schedule(vec![Tx(0, vec![Access::Write(1)]), Tx(1, vec![Access::Write(2)])]);
        runtime.schedule(vec![Tx(2, vec![Access::Write(1)]), Tx(3, vec![Access::Write(3)])]);
        let last_batch =
            runtime.schedule(vec![Tx(4, vec![Access::Write(1)]), Tx(5, vec![Access::Write(4)])]);
        last_batch.wait_committed_blocking();

        // Verify state before rollback
        runtime
            .assert_written_state(1, vec![0, 2, 4]) // Written by tx 0, 2, 4
            .assert_written_state(2, vec![1]) // Written by tx 1
            .assert_written_state(3, vec![3]) // Written by tx 3
            .assert_written_state(4, vec![5]); // Written by tx 5

        // Rollback to index 1 (revert batches with index 1 and 2, keep batch with index 0)
        runtime.rollback_to(1);

        // Verify state after rollback - only batch0 effects should remain
        runtime
            .assert_written_state(1, vec![0]) // Only tx 0's write remains
            .assert_written_state(2, vec![1]) // tx 1's write remains (in batch0)
            .assert_resource_deleted(3)
            .assert_resource_deleted(4);

        runtime.shutdown();
    }
}

/// Tests that new batches can be scheduled after a rollback and receive correct indices.
/// After rollback, the context resets to the target index and new batches continue from there.
#[test]
pub fn test_add_batches_after_rollback() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Schedule initial batches (indices 1, 2, 3)
        let batch1 = runtime.schedule(vec![Tx(0, vec![Access::Write(1)])]);
        let batch2 = runtime.schedule(vec![Tx(1, vec![Access::Write(2)])]);
        let batch3 = runtime.schedule(vec![Tx(2, vec![Access::Write(3)])]);
        batch3.wait_committed_blocking();

        assert_eq!(batch1.index(), 1);
        assert_eq!(batch2.index(), 2);
        assert_eq!(batch3.index(), 3);

        // Verify initial state
        runtime
            .assert_written_state(1, vec![0])
            .assert_written_state(2, vec![1])
            .assert_written_state(3, vec![2]);

        // Rollback to batch 1 (keep batch 1, remove batches 2 and 3)
        runtime.rollback_to(1);

        // Resources 2 and 3 should be deleted, resource 1 should still exist
        runtime
            .assert_resource_deleted(2)
            .assert_resource_deleted(3)
            .assert_written_state(1, vec![0]);

        // Schedule new batches after rollback - should continue from index 2
        let batch4 = runtime.schedule(vec![Tx(10, vec![Access::Write(10)])]);
        let batch5 = runtime.schedule(vec![Tx(11, vec![Access::Write(11)])]);
        batch5.wait_committed_blocking();

        assert_eq!(batch4.index(), 2);
        assert_eq!(batch5.index(), 3);

        // Verify new state
        runtime
            .assert_written_state(1, vec![0])
            .assert_written_state(10, vec![10])
            .assert_written_state(11, vec![11]);

        runtime.shutdown();
    }
}

/// Tests in-flight batch cancellation without waiting for commitment.
/// When a rollback occurs, batches that haven't been committed yet should detect
/// cancellation via was_canceled() and skip their writes.
#[test]
pub fn test_inflight_cancellation_without_waiting() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Schedule a batch and wait for it to commit
        let batch1 = runtime.schedule(vec![Tx(0, vec![Access::Write(1)])]);
        batch1.wait_committed_blocking();

        // Schedule multiple batches but don't wait for commitment
        let batch2 = runtime.schedule(vec![Tx(1, vec![Access::Write(2)])]);
        let batch3 = runtime.schedule(vec![Tx(2, vec![Access::Write(3)])]);
        let batch4 = runtime.schedule(vec![Tx(3, vec![Access::Write(4)])]);

        // Immediately rollback without waiting for batches 2-4 to commit
        // This tests in-flight cancellation
        runtime.rollback_to(1);

        // After rollback, the canceled batches should have was_canceled() == true
        assert!(batch2.was_canceled(), "batch2 should be canceled");
        assert!(batch3.was_canceled(), "batch3 should be canceled");
        assert!(batch4.was_canceled(), "batch4 should be canceled");

        // Resource 1 should still exist (from batch1 which was committed)
        // Resources 2, 3, 4 should be cleaned up by rollback
        runtime
            .assert_written_state(1, vec![0])
            .assert_resource_deleted(2)
            .assert_resource_deleted(3)
            .assert_resource_deleted(4);

        // New batches scheduled after rollback should work normally
        let batch5 = runtime.schedule(vec![Tx(100, vec![Access::Write(100)])]);
        batch5.wait_committed_blocking();
        assert!(!batch5.was_canceled(), "batch5 should not be canceled");
        runtime.assert_written_state(100, vec![100]);

        runtime.shutdown();
    }
}

/// Tests rollback across multiple RuntimeContext objects with parent chain traversal.
/// This complex scenario tests: apply 1-6; rollback to 5; apply 6-7; rollback to 3; apply 4-5.
#[test]
pub fn test_rollback_multiple_contexts() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Phase 1: Apply batches 1-6
        // Using resource IDs that match batch indices for clarity
        let batch1 = runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(2, vec![Access::Write(2)])]);
        runtime.schedule(vec![Tx(3, vec![Access::Write(3)])]);
        runtime.schedule(vec![Tx(4, vec![Access::Write(4)])]);
        runtime.schedule(vec![Tx(5, vec![Access::Write(5)])]);
        let batch6 = runtime.schedule(vec![Tx(6, vec![Access::Write(6)])]);
        batch6.wait_committed_blocking();

        assert_eq!(batch1.index(), 1);
        assert_eq!(batch6.index(), 6);

        // Verify all resources exist
        runtime
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3])
            .assert_written_state(4, vec![4])
            .assert_written_state(5, vec![5])
            .assert_written_state(6, vec![6]);

        // Phase 2: Rollback to 5 (keeps batches 1-5, removes batch 6)
        runtime.rollback_to(5);

        // Batch 6's resource should be deleted, batches 1-5 should still exist
        runtime
            .assert_resource_deleted(6)
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3])
            .assert_written_state(4, vec![4])
            .assert_written_state(5, vec![5]);

        // Phase 3: Apply new batches 6-7 (in the new context after rollback)
        let new_batch6 = runtime.schedule(vec![Tx(60, vec![Access::Write(60)])]);
        let batch7 = runtime.schedule(vec![Tx(70, vec![Access::Write(70)])]);
        batch7.wait_committed_blocking();

        assert_eq!(new_batch6.index(), 6);
        assert_eq!(batch7.index(), 7);

        runtime.assert_written_state(60, vec![60]).assert_written_state(70, vec![70]);

        // Phase 4: Rollback to 3 (must walk parent context chain)
        runtime.rollback_to(3);

        // Batches 1-3 should remain, 4-5 and new 6-7 should be deleted
        runtime
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3])
            .assert_resource_deleted(4)
            .assert_resource_deleted(5)
            .assert_resource_deleted(60)
            .assert_resource_deleted(70);

        // Phase 5: Apply batches 4-5 (in the new context after second rollback)
        let final_batch4 = runtime.schedule(vec![Tx(40, vec![Access::Write(40)])]);
        let final_batch5 = runtime.schedule(vec![Tx(50, vec![Access::Write(50)])]);
        final_batch5.wait_committed_blocking();

        assert_eq!(final_batch4.index(), 4);
        assert_eq!(final_batch5.index(), 5);

        // Verify final state
        runtime
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3])
            .assert_written_state(40, vec![40])
            .assert_written_state(50, vec![50]);

        runtime.shutdown();
    }
}

/// Tests rollback to batch 0 (complete revert to initial state).
/// All state should be cleared.
#[test]
pub fn test_rollback_to_zero() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Schedule several batches
        runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(2, vec![Access::Write(2)])]);
        let batch3 = runtime.schedule(vec![Tx(3, vec![Access::Write(3)])]);
        batch3.wait_committed_blocking();

        // Verify state exists
        runtime
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3]);

        // Rollback to 0 (before any batches)
        runtime.rollback_to(0);

        // All resources should be deleted
        runtime.assert_resource_deleted(1).assert_resource_deleted(2).assert_resource_deleted(3);

        // New batches should start from index 1 again
        let new_batch1 = runtime.schedule(vec![Tx(100, vec![Access::Write(100)])]);
        new_batch1.wait_committed_blocking();

        assert_eq!(new_batch1.index(), 1);
        runtime.assert_written_state(100, vec![100]);

        runtime.shutdown();
    }
}

/// Tests multiple consecutive rollbacks without adding new batches in between.
/// Verifies that consecutive rollbacks properly reduce state.
#[test]
pub fn test_consecutive_rollbacks() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Create 5 batches
        runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(2, vec![Access::Write(2)])]);
        runtime.schedule(vec![Tx(3, vec![Access::Write(3)])]);
        runtime.schedule(vec![Tx(4, vec![Access::Write(4)])]);
        let batch5 = runtime.schedule(vec![Tx(5, vec![Access::Write(5)])]);
        batch5.wait_committed_blocking();

        // Verify all exist
        runtime
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3])
            .assert_written_state(4, vec![4])
            .assert_written_state(5, vec![5]);

        // First rollback: to 4
        runtime.rollback_to(4);
        runtime
            .assert_resource_deleted(5)
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3])
            .assert_written_state(4, vec![4]);

        // Second rollback: to 3
        runtime.rollback_to(3);
        runtime
            .assert_resource_deleted(4)
            .assert_resource_deleted(5)
            .assert_written_state(1, vec![1])
            .assert_written_state(2, vec![2])
            .assert_written_state(3, vec![3]);

        // Third rollback: to 1
        runtime.rollback_to(1);
        runtime
            .assert_resource_deleted(2)
            .assert_resource_deleted(3)
            .assert_written_state(1, vec![1]);

        // Verify context indices are correct
        let new_batch = runtime.schedule(vec![Tx(100, vec![Access::Write(100)])]);
        new_batch.wait_committed_blocking();
        assert_eq!(new_batch.index(), 2);

        runtime.shutdown();
    }
}

/// Tests rollback of batches that modify the same resource multiple times.
/// This exercises the resource dependency chain and version restoration.
#[test]
pub fn test_rollback_same_resource_multiple_writes() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Multiple batches all writing to resource 1
        runtime.schedule(vec![Tx(10, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(20, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(30, vec![Access::Write(1)])]);
        let batch4 = runtime.schedule(vec![Tx(40, vec![Access::Write(1)])]);
        batch4.wait_committed_blocking();

        // Resource 1 should have been written by all 4 transactions
        runtime.assert_written_state(1, vec![10, 20, 30, 40]);

        // Rollback to batch 2 (keep writes from batch 1 and 2)
        runtime.rollback_to(2);
        runtime.assert_written_state(1, vec![10, 20]);

        // Add more writes
        let batch5 = runtime.schedule(vec![Tx(50, vec![Access::Write(1)])]);
        batch5.wait_committed_blocking();

        // Now should have 10, 20, 50
        runtime.assert_written_state(1, vec![10, 20, 50]);

        // Rollback to batch 1
        runtime.rollback_to(1);
        runtime.assert_written_state(1, vec![10]);

        runtime.shutdown();
    }
}

/// Tests that canceled batches are marked as canceled and wait functions
/// return immediately without blocking.
#[test]
pub fn test_cancellation_skips_writes() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Create and commit a batch to resource 1
        let batch1 = runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        batch1.wait_committed_blocking();

        // Schedule batches that access different resources
        let batch2 = runtime.schedule(vec![Tx(2, vec![Access::Write(100)])]);
        let batch3 = runtime.schedule(vec![Tx(3, vec![Access::Write(200)])]);

        // Rollback immediately - batch2 and batch3 should be canceled
        runtime.rollback_to(1);

        // Verify both batches were canceled
        assert!(batch2.was_canceled(), "batch2 should be canceled");
        assert!(batch3.was_canceled(), "batch3 should be canceled");

        // The wait functions should return immediately for canceled batches
        batch2.wait_committed_blocking();
        batch3.wait_committed_blocking();

        // Resource 1 should only have the write from batch1
        // Resources 100 and 200 should be cleaned up by rollback
        runtime
            .assert_written_state(1, vec![1])
            .assert_resource_deleted(100)
            .assert_resource_deleted(200);

        runtime.shutdown();
    }
}

/// Tests complex scenario with interleaved writes to multiple resources
/// followed by rollback and new writes.
#[test]
pub fn test_rollback_interleaved_multi_resource() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Batch 1: Write to resources 1 and 2
        runtime.schedule(vec![Tx(10, vec![Access::Write(1)]), Tx(11, vec![Access::Write(2)])]);

        // Batch 2: Write to resources 2 and 3
        runtime.schedule(vec![Tx(20, vec![Access::Write(2)]), Tx(21, vec![Access::Write(3)])]);

        // Batch 3: Write to resources 1 and 3
        runtime.schedule(vec![Tx(30, vec![Access::Write(1)]), Tx(31, vec![Access::Write(3)])]);

        // Batch 4: Write to all resources
        let batch4 = runtime.schedule(vec![
            Tx(40, vec![Access::Write(1)]),
            Tx(41, vec![Access::Write(2)]),
            Tx(42, vec![Access::Write(3)]),
        ]);
        batch4.wait_committed_blocking();

        // Verify state before rollback
        runtime
            .assert_written_state(1, vec![10, 30, 40])
            .assert_written_state(2, vec![11, 20, 41])
            .assert_written_state(3, vec![21, 31, 42]);

        // Rollback to batch 2
        runtime.rollback_to(2);

        runtime
            .assert_written_state(1, vec![10]) // Only from batch 1
            .assert_written_state(2, vec![11, 20]) // From batch 1 and 2
            .assert_written_state(3, vec![21]); // Only from batch 2

        // Add new batches after rollback
        let batch5 = runtime.schedule(vec![
            Tx(50, vec![Access::Write(1)]),
            Tx(51, vec![Access::Write(4)]), // New resource 4
        ]);
        batch5.wait_committed_blocking();

        // Verify mixed state
        runtime
            .assert_written_state(1, vec![10, 50])
            .assert_written_state(2, vec![11, 20])
            .assert_written_state(3, vec![21])
            .assert_written_state(4, vec![51]);

        runtime.shutdown();
    }
}

/// Tests that resources are properly evicted from the cache after their batches commit.
#[test]
pub fn test_resource_eviction() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        const NUM_BATCHES: usize = 100;
        const RESOURCES_PER_BATCH: usize = 10;

        let mut batches = Vec::with_capacity(NUM_BATCHES);
        for batch_idx in 0..NUM_BATCHES {
            let base_resource = batch_idx * RESOURCES_PER_BATCH;
            let txs: Vec<_> = (0..RESOURCES_PER_BATCH)
                .map(|i| Tx(base_resource + i, vec![Access::Write(base_resource + i)]))
                .collect();
            batches.push(runtime.schedule(txs));
        }

        for batch in &batches {
            batch.wait_committed_blocking();
        }

        runtime.wait_cache_empty(Duration::from_secs(10));

        // Verify data was written correctly for a sample of resources
        for batch_idx in [0, 50, 99] {
            let base_resource = batch_idx * RESOURCES_PER_BATCH;
            for i in 0..RESOURCES_PER_BATCH {
                let resource_id = base_resource + i;
                runtime.assert_written_state(resource_id, vec![resource_id]);
            }
        }

        runtime.shutdown();
    }
}

/// Tests eviction behavior with high-frequency scheduling.
#[test]
pub fn test_eviction_under_load() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        const WAVES: usize = 10;
        const BATCHES_PER_WAVE: usize = 50;

        for wave in 0..WAVES {
            let base = wave * BATCHES_PER_WAVE;

            let batches: Vec<_> = (0..BATCHES_PER_WAVE)
                .map(|i| {
                    let resource_id = base + i;
                    runtime.schedule(vec![Tx(resource_id, vec![Access::Write(resource_id)])])
                })
                .collect();

            for batch in &batches {
                batch.wait_committed_blocking();
            }

            runtime.wait_cache_empty(Duration::from_secs(10));
        }

        runtime.shutdown();
    }
}

/// Tests that pruning deletes rollback pointers for batches below the threshold.
#[test]
pub fn test_basic_pruning() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Create batches that write to resources (generates rollback pointers)
        let batch1 = runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        let batch2 = runtime.schedule(vec![Tx(2, vec![Access::Write(1)])]);
        let batch3 = runtime.schedule(vec![Tx(3, vec![Access::Write(1)])]);
        batch3.wait_committed_blocking();

        // Verify rollback pointers exist for batches 2 and 3
        let store = runtime.storage_manager().store();
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.index()).count(),
            1,
            "Batch 2 should have rollback pointer"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.index()).count(),
            1,
            "Batch 3 should have rollback pointer"
        );

        // Set pruning threshold to batch 3 (prune batches 1 and 2)
        runtime.set_pruning_threshold(batch3.index());
        runtime.wait_pruned(batch2.index(), Duration::from_secs(10));

        // Verify rollback pointers for batches 1 and 2 are deleted
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch1.index()).count(),
            0,
            "Batch 1 rollback pointers should be pruned"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.index()).count(),
            0,
            "Batch 2 rollback pointers should be pruned"
        );

        // Batch 3 should still have its rollback pointer (not pruned)
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.index()).count(),
            1,
            "Batch 3 rollback pointer should NOT be pruned"
        );

        // Data should still be readable
        runtime.assert_written_state(1, vec![1, 2, 3]);

        runtime.shutdown();
    }
}

/// Tests that pruning preserves batches at or above the threshold.
#[test]
pub fn test_pruning_preserves_recent_batches() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Create 5 batches, each overwriting the same resource
        for i in 1..=5 {
            let batch = runtime.schedule(vec![Tx(i, vec![Access::Write(1)])]);
            batch.wait_committed_blocking();
        }

        let store = runtime.storage_manager().store();

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
        runtime.set_pruning_threshold(4);
        runtime.wait_pruned(3, Duration::from_secs(10));

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

        runtime.shutdown();
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
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        for i in 1..=5 {
            let batch = runtime.schedule(vec![Tx(i, vec![Access::Write(1)])]);
            batch.wait_committed_blocking();
        }

        // Prune batches 1-2
        runtime.set_pruning_threshold(3);
        runtime.wait_pruned(2, Duration::from_secs(10));

        // Verify last_pruned_index is persisted
        assert_eq!(
            StateMetadata::get_last_pruned_index(runtime.storage_manager().store().as_ref()),
            Some(2),
            "Last pruned index should be persisted"
        );

        runtime.shutdown();
    }

    // Phase 2: Reopen and verify pruning state is preserved
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        assert_eq!(runtime.last_pruned_index(), 2, "Pruning should resume from persisted index");

        let store = runtime.storage_manager().store();

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
        runtime.set_pruning_threshold(5);
        runtime.wait_pruned(4, Duration::from_secs(10));

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

        runtime.shutdown();
    }
}

/// Tests pruning with multiple resources per batch.
#[test]
pub fn test_pruning_multiple_resources() {
    use vprogs_state_ptr_rollback::StatePtrRollback;

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(VM),
            StorageConfig::default().with_store(storage),
        );

        // Batch 1: Create resources 1, 2, 3
        let batch1 = runtime.schedule(vec![
            Tx(1, vec![Access::Write(1)]),
            Tx(2, vec![Access::Write(2)]),
            Tx(3, vec![Access::Write(3)]),
        ]);
        batch1.wait_committed_blocking();

        // Batch 2: Overwrite all three resources
        let batch2 = runtime.schedule(vec![
            Tx(10, vec![Access::Write(1)]),
            Tx(20, vec![Access::Write(2)]),
            Tx(30, vec![Access::Write(3)]),
        ]);
        batch2.wait_committed_blocking();

        // Batch 3: Overwrite all three resources again
        let batch3 = runtime.schedule(vec![
            Tx(100, vec![Access::Write(1)]),
            Tx(200, vec![Access::Write(2)]),
            Tx(300, vec![Access::Write(3)]),
        ]);
        batch3.wait_committed_blocking();

        let store = runtime.storage_manager().store();

        // Batch 2 and 3 should have 3 rollback pointers each
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.index()).count(),
            3,
            "Batch 2 should have 3 rollback pointers"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.index()).count(),
            3,
            "Batch 3 should have 3 rollback pointers"
        );

        // Prune batch 1 and 2
        runtime.set_pruning_threshold(batch3.index());
        runtime.wait_pruned(batch2.index(), Duration::from_secs(10));

        // All rollback pointers for batches 1 and 2 should be deleted
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch1.index()).count(),
            0,
            "Batch 1 should have no rollback pointers after pruning"
        );
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch2.index()).count(),
            0,
            "Batch 2 should have no rollback pointers after pruning"
        );

        // Batch 3 should still have its rollback pointers
        assert_eq!(
            StatePtrRollback::iter_batch(store.as_ref(), batch3.index()).count(),
            3,
            "Batch 3 should still have 3 rollback pointers"
        );

        // Data should still be readable
        runtime
            .assert_written_state(1, vec![1, 10, 100])
            .assert_written_state(2, vec![2, 20, 200])
            .assert_written_state(3, vec![3, 30, 300]);

        runtime.shutdown();
    }
}
