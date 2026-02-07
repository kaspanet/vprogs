use std::time::{Duration, Instant};

use vprogs_scheduling_scheduler::Scheduler;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::RocksDbStore;

use crate::VM;

/// Extension trait for scheduler test helpers.
pub trait SchedulerExt {
    /// Waits until the last pruned index reaches the expected value or timeout is reached.
    fn wait_pruned(&self, expected: u64, timeout: Duration) -> &Self;

    /// Repeatedly processes the eviction queue until the cache is empty or timeout is reached.
    fn wait_cache_empty(&mut self, timeout: Duration) -> &mut Self;

    /// Asserts that a resource has the expected version and writer log.
    fn assert_written_state(&self, resource_id: usize, writers: Vec<usize>) -> &Self;

    /// Asserts that a resource has been deleted (no latest pointer exists).
    fn assert_resource_deleted(&self, resource_id: usize) -> &Self;
}

impl SchedulerExt for Scheduler<RocksDbStore, VM> {
    fn wait_pruned(&self, expected: u64, timeout: Duration) -> &Self {
        let start = Instant::now();
        while self.pruning().last_pruned().0 < expected {
            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for pruning. Expected last_pruned >= {}, got {}.",
                    expected,
                    self.pruning().last_pruned().0
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self
    }

    fn wait_cache_empty(&mut self, timeout: Duration) -> &mut Self {
        let start = Instant::now();
        while self.cached_resource_count() > 0 {
            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for cache to empty. Still have {} cached resources.",
                    self.cached_resource_count()
                );
            }
            self.process_eviction_queue();
            std::thread::sleep(Duration::from_millis(10));
        }
        self
    }

    fn assert_written_state(&self, resource_id: usize, writers: Vec<usize>) -> &Self {
        let store = self.storage_manager().store();
        let writer_count = writers.len();
        let writer_log: Vec<u8> = writers.iter().flat_map(|id| id.to_be_bytes()).collect();

        let versioned_state = StateVersion::<usize>::from_latest_data(store.as_ref(), resource_id);
        assert_eq!(versioned_state.version(), writer_count as u64);
        assert_eq!(*versioned_state.data(), writer_log);
        self
    }

    fn assert_resource_deleted(&self, resource_id: usize) -> &Self {
        use vprogs_storage_types::ReadStore;

        let store = self.storage_manager().store();
        let id_bytes = resource_id.to_be_bytes();
        assert!(
            store.get(StateSpace::StatePtrLatest, &id_bytes).is_none(),
            "Resource {} should have been deleted but still exists",
            resource_id
        );
        self
    }
}
