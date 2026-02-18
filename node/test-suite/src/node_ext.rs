use std::time::Duration;

use vprogs_node_framework::NodeApi;
use vprogs_node_l1_bridge::ChainBlockMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::ReadStore;

use crate::TestNodeVm;

/// Convenience helpers for [`NodeApi`] in test scenarios.
pub trait NodeExt {
    /// Polls until `last_committed` index reaches `expected`, or panics on timeout.
    fn wait_committed(
        &self,
        expected: u64,
        timeout: Duration,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// Polls until pruning has progressed past `expected` (root index > expected), or panics on
    /// timeout.
    fn wait_pruned(
        &self,
        expected: u64,
        timeout: Duration,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// Asserts that a resource was written by the given writer IDs (in order).
    fn assert_written_state(
        &self,
        resource_id: usize,
        writers: Vec<usize>,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// Asserts that a resource has been deleted (no latest pointer exists).
    fn assert_resource_deleted(
        &self,
        resource_id: usize,
    ) -> impl std::future::Future<Output = ()> + Send;
}

impl NodeExt for NodeApi<RocksDbStore, TestNodeVm> {
    async fn wait_committed(&self, expected: u64, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Read the persisted value from disk, because the in-memory last_committed
            // is lazily updated only when the next batch is scheduled.
            let index = self
                .with_scheduler(|scheduler| {
                    StateMetadata::last_committed::<ChainBlockMetadata, _>(
                        &**scheduler.state().storage().store(),
                    )
                    .index()
                })
                .await
                .expect("api call failed");
            if index >= expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Timeout waiting for committed index >= {expected}, got {index}",
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_pruned(&self, expected: u64, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let root_index = self.root().index();
            if root_index > expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Timeout waiting for root index > {expected}, got {root_index}",
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn assert_written_state(&self, resource_id: usize, writers: Vec<usize>) {
        self.with_scheduler(move |scheduler| {
            let store = scheduler.state().storage().store();
            let writer_count = writers.len();
            let writer_log: Vec<u8> = writers.iter().flat_map(|id| id.to_be_bytes()).collect();

            let versioned_state =
                StateVersion::<usize>::from_latest_data(store.as_ref(), resource_id);
            assert_eq!(
                versioned_state.version(),
                writer_count as u64,
                "resource {resource_id}: expected version {writer_count}, got {}",
                versioned_state.version()
            );
            assert_eq!(
                *versioned_state.data(),
                writer_log,
                "resource {resource_id}: unexpected writer log"
            );
        })
        .await
        .expect("api call failed");
    }

    async fn assert_resource_deleted(&self, resource_id: usize) {
        self.with_scheduler(move |scheduler| {
            let store = scheduler.state().storage().store();
            let id_bytes = resource_id.to_be_bytes();
            assert!(
                store.get(StateSpace::StatePtrLatest, &id_bytes).is_none(),
                "Resource {resource_id} should have been deleted but still exists",
            );
        })
        .await
        .expect("api call failed");
    }
}
