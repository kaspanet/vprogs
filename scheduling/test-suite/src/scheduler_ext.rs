use std::time::{Duration, Instant};

use vprogs_scheduling_scheduler::Scheduler;
use vprogs_storage_rocksdb_store::RocksDbStore;

use crate::VM;

/// Extension trait for scheduler test helpers.
pub trait SchedulerExt {
    /// Waits until the last pruned index reaches the expected value or timeout is reached.
    fn wait_pruned(&self, expected: u64, timeout: Duration);

    /// Repeatedly processes the eviction queue until the cache is empty or timeout is reached.
    fn wait_cache_empty(&mut self, timeout: Duration);
}

impl SchedulerExt for Scheduler<RocksDbStore, VM> {
    fn wait_pruned(&self, expected: u64, timeout: Duration) {
        let start = Instant::now();
        while self.last_pruned_index() < expected {
            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for pruning. Expected last_pruned_index >= {}, got {}.",
                    expected,
                    self.last_pruned_index()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_cache_empty(&mut self, timeout: Duration) {
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
    }
}
