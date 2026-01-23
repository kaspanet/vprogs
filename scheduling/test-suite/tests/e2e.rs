extern crate core;

use std::time::{Duration, Instant};

use tempfile::TempDir;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;

use crate::test_framework::{Access, AssertResourceDeleted, AssertWrittenState, TestVM, Tx};

#[test]
pub fn test_runtime() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(TestVM),
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

        for assertion in [
            AssertWrittenState(1, vec![0, 1, 3]),
            AssertWrittenState(2, vec![1]),
            AssertWrittenState(3, vec![]),
            AssertWrittenState(10, vec![4]),
            AssertWrittenState(20, vec![4]),
        ] {
            assertion.assert(runtime.storage_manager().store());
        }

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
            ExecutionConfig::default().with_vm(TestVM),
            StorageConfig::default().with_store(storage),
        );

        runtime.schedule(vec![Tx(0, vec![Access::Write(1)]), Tx(1, vec![Access::Write(2)])]);
        runtime.schedule(vec![Tx(2, vec![Access::Write(1)]), Tx(3, vec![Access::Write(3)])]);
        let last_batch =
            runtime.schedule(vec![Tx(4, vec![Access::Write(1)]), Tx(5, vec![Access::Write(4)])]);
        last_batch.wait_committed_blocking();

        // Verify state before rollback
        for assertion in [
            AssertWrittenState(1, vec![0, 2, 4]), // Written by tx 0, 2, 4
            AssertWrittenState(2, vec![1]),       // Written by tx 1
            AssertWrittenState(3, vec![3]),       // Written by tx 3
            AssertWrittenState(4, vec![5]),       // Written by tx 5
        ] {
            assertion.assert(runtime.storage_manager().store());
        }

        // Rollback to index 1 (revert batches with index 1 and 2, keep batch with index 0)
        runtime.rollback_to(1);

        // Verify state after rollback - only batch0 effects should remain
        for assertion in [
            AssertWrittenState(1, vec![0]), // Only tx 0's write remains
            AssertWrittenState(2, vec![1]), // tx 1's write remains (in batch0)
        ] {
            assertion.assert(runtime.storage_manager().store());
        }

        // Resources 3 and 4 should no longer exist (created in rolled-back batches)
        AssertResourceDeleted(3).assert(runtime.storage_manager().store());
        AssertResourceDeleted(4).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
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
        AssertWrittenState(1, vec![0]).assert(runtime.storage_manager().store());
        AssertWrittenState(2, vec![1]).assert(runtime.storage_manager().store());
        AssertWrittenState(3, vec![2]).assert(runtime.storage_manager().store());

        // Rollback to batch 1 (keep batch 1, remove batches 2 and 3)
        runtime.rollback_to(1);

        // Resources 2 and 3 should be deleted
        AssertResourceDeleted(2).assert(runtime.storage_manager().store());
        AssertResourceDeleted(3).assert(runtime.storage_manager().store());

        // Resource 1 should still exist
        AssertWrittenState(1, vec![0]).assert(runtime.storage_manager().store());

        // Schedule new batches after rollback - should continue from index 2
        let batch4 = runtime.schedule(vec![Tx(10, vec![Access::Write(10)])]);
        let batch5 = runtime.schedule(vec![Tx(11, vec![Access::Write(11)])]);
        batch5.wait_committed_blocking();

        assert_eq!(batch4.index(), 2);
        assert_eq!(batch5.index(), 3);

        // Verify new state
        AssertWrittenState(1, vec![0]).assert(runtime.storage_manager().store());
        AssertWrittenState(10, vec![10]).assert(runtime.storage_manager().store());
        AssertWrittenState(11, vec![11]).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
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
        AssertWrittenState(1, vec![0]).assert(runtime.storage_manager().store());

        // Resources 2, 3, 4 might or might not exist depending on timing,
        // but the rollback should have cleaned them up
        AssertResourceDeleted(2).assert(runtime.storage_manager().store());
        AssertResourceDeleted(3).assert(runtime.storage_manager().store());
        AssertResourceDeleted(4).assert(runtime.storage_manager().store());

        // New batches scheduled after rollback should work normally
        let batch5 = runtime.schedule(vec![Tx(100, vec![Access::Write(100)])]);
        batch5.wait_committed_blocking();
        assert!(!batch5.was_canceled(), "batch5 should not be canceled");
        AssertWrittenState(100, vec![100]).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
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
        for i in 1..=6 {
            AssertWrittenState(i, vec![i]).assert(runtime.storage_manager().store());
        }

        // Phase 2: Rollback to 5 (keeps batches 1-5, removes batch 6)
        // This creates a new context with a parent pointing to the original
        runtime.rollback_to(5);

        // Batch 6's resource should be deleted
        AssertResourceDeleted(6).assert(runtime.storage_manager().store());
        // Batches 1-5 should still exist
        for i in 1..=5 {
            AssertWrittenState(i, vec![i]).assert(runtime.storage_manager().store());
        }

        // Phase 3: Apply new batches 6-7 (in the new context after rollback)
        // These get indices 6 and 7, replacing the old batch 6
        let new_batch6 = runtime.schedule(vec![Tx(60, vec![Access::Write(60)])]);
        let batch7 = runtime.schedule(vec![Tx(70, vec![Access::Write(70)])]);
        batch7.wait_committed_blocking();

        assert_eq!(new_batch6.index(), 6);
        assert_eq!(batch7.index(), 7);

        AssertWrittenState(60, vec![60]).assert(runtime.storage_manager().store());
        AssertWrittenState(70, vec![70]).assert(runtime.storage_manager().store());

        // Phase 4: Rollback to 3 (must walk parent context chain)
        // This removes batches 4, 5, 6 (new), 7
        runtime.rollback_to(3);

        // Batches 1-3 should remain
        for i in 1..=3 {
            AssertWrittenState(i, vec![i]).assert(runtime.storage_manager().store());
        }

        // Batches 4, 5 from original context should be deleted
        AssertResourceDeleted(4).assert(runtime.storage_manager().store());
        AssertResourceDeleted(5).assert(runtime.storage_manager().store());

        // New batches 6, 7 (resources 60, 70) should also be deleted
        AssertResourceDeleted(60).assert(runtime.storage_manager().store());
        AssertResourceDeleted(70).assert(runtime.storage_manager().store());

        // Phase 5: Apply batches 4-5 (in the new context after second rollback)
        let final_batch4 = runtime.schedule(vec![Tx(40, vec![Access::Write(40)])]);
        let final_batch5 = runtime.schedule(vec![Tx(50, vec![Access::Write(50)])]);
        final_batch5.wait_committed_blocking();

        assert_eq!(final_batch4.index(), 4);
        assert_eq!(final_batch5.index(), 5);

        // Verify final state: resources 1-3 from original, 40, 50 from final batches
        AssertWrittenState(1, vec![1]).assert(runtime.storage_manager().store());
        AssertWrittenState(2, vec![2]).assert(runtime.storage_manager().store());
        AssertWrittenState(3, vec![3]).assert(runtime.storage_manager().store());
        AssertWrittenState(40, vec![40]).assert(runtime.storage_manager().store());
        AssertWrittenState(50, vec![50]).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
            StorageConfig::default().with_store(storage),
        );

        // Schedule several batches
        runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(2, vec![Access::Write(2)])]);
        let batch3 = runtime.schedule(vec![Tx(3, vec![Access::Write(3)])]);
        batch3.wait_committed_blocking();

        // Verify state exists
        AssertWrittenState(1, vec![1]).assert(runtime.storage_manager().store());
        AssertWrittenState(2, vec![2]).assert(runtime.storage_manager().store());
        AssertWrittenState(3, vec![3]).assert(runtime.storage_manager().store());

        // Rollback to 0 (before any batches)
        runtime.rollback_to(0);

        // All resources should be deleted
        AssertResourceDeleted(1).assert(runtime.storage_manager().store());
        AssertResourceDeleted(2).assert(runtime.storage_manager().store());
        AssertResourceDeleted(3).assert(runtime.storage_manager().store());

        // New batches should start from index 1 again
        let new_batch1 = runtime.schedule(vec![Tx(100, vec![Access::Write(100)])]);
        new_batch1.wait_committed_blocking();

        assert_eq!(new_batch1.index(), 1);
        AssertWrittenState(100, vec![100]).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
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
        for i in 1..=5 {
            AssertWrittenState(i, vec![i]).assert(runtime.storage_manager().store());
        }

        // First rollback: to 4
        runtime.rollback_to(4);
        AssertResourceDeleted(5).assert(runtime.storage_manager().store());
        for i in 1..=4 {
            AssertWrittenState(i, vec![i]).assert(runtime.storage_manager().store());
        }

        // Second rollback: to 3
        runtime.rollback_to(3);
        AssertResourceDeleted(4).assert(runtime.storage_manager().store());
        AssertResourceDeleted(5).assert(runtime.storage_manager().store());
        for i in 1..=3 {
            AssertWrittenState(i, vec![i]).assert(runtime.storage_manager().store());
        }

        // Third rollback: to 1
        runtime.rollback_to(1);
        AssertResourceDeleted(2).assert(runtime.storage_manager().store());
        AssertResourceDeleted(3).assert(runtime.storage_manager().store());
        AssertWrittenState(1, vec![1]).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
            StorageConfig::default().with_store(storage),
        );

        // Multiple batches all writing to resource 1
        runtime.schedule(vec![Tx(10, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(20, vec![Access::Write(1)])]);
        runtime.schedule(vec![Tx(30, vec![Access::Write(1)])]);
        let batch4 = runtime.schedule(vec![Tx(40, vec![Access::Write(1)])]);
        batch4.wait_committed_blocking();

        // Resource 1 should have been written by all 4 transactions
        AssertWrittenState(1, vec![10, 20, 30, 40]).assert(runtime.storage_manager().store());

        // Rollback to batch 2 (keep writes from batch 1 and 2)
        runtime.rollback_to(2);

        // Resource 1 should only have writes from tx 10 and 20
        AssertWrittenState(1, vec![10, 20]).assert(runtime.storage_manager().store());

        // Add more writes
        let batch5 = runtime.schedule(vec![Tx(50, vec![Access::Write(1)])]);
        batch5.wait_committed_blocking();

        // Now should have 10, 20, 50
        AssertWrittenState(1, vec![10, 20, 50]).assert(runtime.storage_manager().store());

        // Rollback to batch 1
        runtime.rollback_to(1);

        // Only tx 10's write should remain
        AssertWrittenState(1, vec![10]).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
            StorageConfig::default().with_store(storage),
        );

        // Create and commit a batch to resource 1
        let batch1 = runtime.schedule(vec![Tx(1, vec![Access::Write(1)])]);
        batch1.wait_committed_blocking();

        // Schedule batches that access different resources (to avoid dependency chain timing
        // issues)
        let batch2 = runtime.schedule(vec![Tx(2, vec![Access::Write(100)])]);
        let batch3 = runtime.schedule(vec![Tx(3, vec![Access::Write(200)])]);

        // Rollback immediately - batch2 and batch3 should be canceled
        runtime.rollback_to(1);

        // Verify both batches were canceled
        assert!(batch2.was_canceled(), "batch2 should be canceled");
        assert!(batch3.was_canceled(), "batch3 should be canceled");

        // The wait functions should return immediately for canceled batches (not block forever)
        batch2.wait_committed_blocking();
        batch3.wait_committed_blocking();

        // Resource 1 should only have the write from batch1
        AssertWrittenState(1, vec![1]).assert(runtime.storage_manager().store());

        // Resources 100 and 200 should be cleaned up by rollback
        AssertResourceDeleted(100).assert(runtime.storage_manager().store());
        AssertResourceDeleted(200).assert(runtime.storage_manager().store());

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
            ExecutionConfig::default().with_vm(TestVM),
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
        AssertWrittenState(1, vec![10, 30, 40]).assert(runtime.storage_manager().store());
        AssertWrittenState(2, vec![11, 20, 41]).assert(runtime.storage_manager().store());
        AssertWrittenState(3, vec![21, 31, 42]).assert(runtime.storage_manager().store());

        // Rollback to batch 2
        runtime.rollback_to(2);

        // Resource 1: only from batch 1
        AssertWrittenState(1, vec![10]).assert(runtime.storage_manager().store());
        // Resource 2: from batch 1 and 2
        AssertWrittenState(2, vec![11, 20]).assert(runtime.storage_manager().store());
        // Resource 3: only from batch 2
        AssertWrittenState(3, vec![21]).assert(runtime.storage_manager().store());

        // Add new batches after rollback
        let batch5 = runtime.schedule(vec![
            Tx(50, vec![Access::Write(1)]),
            Tx(51, vec![Access::Write(4)]), // New resource 4
        ]);
        batch5.wait_committed_blocking();

        // Verify mixed state
        AssertWrittenState(1, vec![10, 50]).assert(runtime.storage_manager().store());
        AssertWrittenState(2, vec![11, 20]).assert(runtime.storage_manager().store());
        AssertWrittenState(3, vec![21]).assert(runtime.storage_manager().store());
        AssertWrittenState(4, vec![51]).assert(runtime.storage_manager().store());

        runtime.shutdown();
    }
}

/// Tests that resources are properly evicted from the cache after their batches commit.
/// Verifies that the resource cache doesn't grow unboundedly when processing many batches
/// with unique resources.
#[test]
pub fn test_resource_eviction() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(TestVM),
            StorageConfig::default().with_store(storage),
        );

        // Schedule many batches, each with unique resources.
        // Each batch touches 10 unique resources.
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

        // Wait for all batches to commit.
        for batch in &batches {
            batch.wait_committed_blocking();
        }

        // Wait for cache to be fully evicted (with timeout).
        wait_for_empty_cache(&mut runtime, Duration::from_secs(10));

        // Verify that the data was actually written correctly for a sample of resources.
        for batch_idx in [0, 50, 99] {
            let base_resource = batch_idx * RESOURCES_PER_BATCH;
            for i in 0..RESOURCES_PER_BATCH {
                let resource_id = base_resource + i;
                AssertWrittenState(resource_id, vec![resource_id])
                    .assert(runtime.storage_manager().store());
            }
        }

        runtime.shutdown();
    }
}

/// Tests eviction behavior with high-frequency scheduling.
/// Rapidly schedules batches in waves and verifies cache is cleared after each wave.
#[test]
pub fn test_eviction_under_load() {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    {
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let mut runtime = Scheduler::new(
            ExecutionConfig::default().with_vm(TestVM),
            StorageConfig::default().with_store(storage),
        );

        // Schedule batches in waves, checking cache is cleared after each wave.
        const WAVES: usize = 10;
        const BATCHES_PER_WAVE: usize = 50;

        for wave in 0..WAVES {
            let base = wave * BATCHES_PER_WAVE;

            // Schedule a wave of batches.
            let batches: Vec<_> = (0..BATCHES_PER_WAVE)
                .map(|i| {
                    let resource_id = base + i;
                    runtime.schedule(vec![Tx(resource_id, vec![Access::Write(resource_id)])])
                })
                .collect();

            // Wait for all batches in this wave to commit.
            for batch in &batches {
                batch.wait_committed_blocking();
            }

            // Wait for cache to be fully evicted (with timeout).
            wait_for_empty_cache(&mut runtime, Duration::from_secs(10));
        }

        runtime.shutdown();
    }
}

/// Repeatedly processes the eviction queue until the cache is empty or timeout is reached.
fn wait_for_empty_cache(runtime: &mut Scheduler<RocksDbStore, TestVM>, timeout: Duration) {
    let start = Instant::now();
    while runtime.cached_resource_count() > 0 {
        if start.elapsed() > timeout {
            panic!(
                "Timeout waiting for cache to empty. Still have {} cached resources.",
                runtime.cached_resource_count()
            );
        }
        runtime.process_eviction_queue();
        std::thread::sleep(Duration::from_millis(10));
    }
}

mod test_framework {
    use std::sync::Arc;

    use vprogs_core_types::{AccessMetadata, AccessType, Transaction};
    use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, VmInterface};
    use vprogs_state_space::StateSpace;
    use vprogs_state_version::StateVersion;
    use vprogs_storage_types::{ReadStore, Store};

    #[derive(Clone)]
    pub struct TestVM;

    impl VmInterface for TestVM {
        type Transaction = Tx;
        type TransactionEffects = ();
        type ResourceId = usize;
        type AccessMetadata = Access;
        type Error = ();

        fn process_transaction<S: Store<StateSpace = StateSpace>>(
            &self,
            tx: &Self::Transaction,
            resources: &mut [AccessHandle<S, Self>],
        ) -> Result<(), Self::Error> {
            for resource in resources {
                if resource.access_metadata().access_type() == AccessType::Write {
                    resource.data_mut().extend_from_slice(&tx.0.to_be_bytes());
                }
            }
            Ok::<(), ()>(())
        }

        fn notarize_batch<S: Store<StateSpace = StateSpace>>(&self, batch: &RuntimeBatch<S, Self>) {
            eprintln!(
                ">> Processed batch with {} transactions and {} state changes",
                batch.txs().len(),
                batch.state_diffs().len()
            );
        }
    }

    pub struct Tx(pub usize, pub Vec<Access>);

    impl Transaction<usize, Access> for Tx {
        fn accessed_resources(&self) -> &[<TestVM as VmInterface>::AccessMetadata] {
            &self.1
        }
    }

    #[derive(Clone)]
    pub enum Access {
        Read(usize),
        Write(usize),
    }

    impl AccessMetadata<usize> for Access {
        fn id(&self) -> usize {
            match self {
                Access::Read(id) => *id,
                Access::Write(id) => *id,
            }
        }

        fn access_type(&self) -> AccessType {
            match self {
                Access::Read(_) => AccessType::Read,
                Access::Write(_) => AccessType::Write,
            }
        }
    }

    pub struct AssertWrittenState(pub usize, pub Vec<usize>);

    impl AssertWrittenState {
        pub fn assert<S: ReadStore<StateSpace = StateSpace>>(&self, store: &Arc<S>) {
            let writer_count = self.1.len();
            let writer_log: Vec<u8> = self.1.iter().flat_map(|id| id.to_be_bytes()).collect();

            let versioned_state = StateVersion::<usize>::from_latest_data(store.as_ref(), self.0);
            assert_eq!(versioned_state.version(), writer_count as u64);
            assert_eq!(*versioned_state.data(), writer_log);
        }
    }

    pub struct AssertResourceDeleted(pub usize);

    impl AssertResourceDeleted {
        pub fn assert<S: ReadStore<StateSpace = StateSpace>>(&self, store: &Arc<S>) {
            let id_bytes = self.0.to_be_bytes();
            assert!(
                store.get(StateSpace::StatePtrLatest, &id_bytes).is_none(),
                "Resource {} should have been deleted but still exists",
                self.0
            );
        }
    }
}
