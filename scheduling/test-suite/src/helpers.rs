use std::time::{Duration, Instant};

use vprogs_scheduling_scheduler::Scheduler;
use vprogs_storage_rocksdb_store::RocksDbStore;

use crate::TestVM;

/// Repeatedly processes the eviction queue until the cache is empty or timeout is reached.
pub fn wait_for_empty_cache(runtime: &mut Scheduler<RocksDbStore, TestVM>, timeout: Duration) {
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

/// Waits until the last pruned index reaches the expected value or timeout is reached.
pub fn wait_for_pruning(
    runtime: &Scheduler<RocksDbStore, TestVM>,
    expected: u64,
    timeout: Duration,
) {
    let start = Instant::now();
    while runtime.last_pruned_index() < expected {
        if start.elapsed() > timeout {
            panic!(
                "Timeout waiting for pruning. Expected last_pruned_index >= {}, got {}.",
                expected,
                runtime.last_pruned_index()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
