use std::{
    thread,
    time::{Duration, Instant},
};

use vprogs_core_types::ResourceId;
use vprogs_l1_types::{BlockHash, ChainBlockMetadata};
use vprogs_node_framework::NodeApi;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::{ReadStore, StateSpace};

use crate::TestNodeVm;

/// Convenience helpers for [`NodeApi`] in test scenarios.
pub trait NodeExt {
    /// Blocks until `last_committed` index reaches `expected`, or panics on timeout.
    fn wait_committed(&self, expected: u64, timeout: Duration);

    /// Blocks until pruning has progressed past `expected` (root index > expected), or panics on
    /// timeout.
    fn wait_pruned(&self, expected: u64, timeout: Duration);

    /// Asserts that a resource was written by the given L1 transactions (in order).
    fn assert_written_state(&self, resource_id: usize, tx_hashes: &[BlockHash]);

    /// Asserts that a resource has been deleted (no latest pointer exists).
    fn assert_resource_deleted(&self, resource_id: usize);
}

impl NodeExt for NodeApi<RocksDbStore, TestNodeVm> {
    fn wait_committed(&self, expected: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            // Read the persisted value from disk, because the in-memory last_committed
            // is lazily updated only when the next batch is scheduled.
            let index =
                StateMetadata::last_committed::<ChainBlockMetadata, _>(&**self.storage().store())
                    .index();
            if index >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Timeout waiting for committed index >= {expected}, got {index}",
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_pruned(&self, expected: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let root_index = self.root().index();
            if root_index > expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Timeout waiting for root index > {expected}, got {root_index}",
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn assert_written_state(&self, resource_id: usize, writers: &[BlockHash]) {
        let store = self.storage().store();
        let writer_count = writers.len();
        let writer_log: Vec<u8> = writers.iter().flat_map(|h| h.as_bytes()).collect();

        let versioned_state = StateVersion::from_latest_data(store.as_ref(), resource_id.into());
        assert_eq!(
            versioned_state.version(),
            writer_count as u64,
            "resource {resource_id}: expected version {writer_count}, got {}",
            versioned_state.version()
        );
        assert_eq!(
            *versioned_state.data(),
            writer_log,
            "resource {resource_id}: unexpected tx hash log"
        );
    }

    fn assert_resource_deleted(&self, resource_id: usize) {
        let store = self.storage().store();
        let id_bytes =
            borsh::to_vec(&ResourceId::from(resource_id)).expect("failed to serialize ResourceId");
        assert!(
            store.get(StateSpace::StatePtrLatest, &id_bytes).is_none(),
            "Resource {resource_id} should have been deleted but still exists",
        );
    }
}
