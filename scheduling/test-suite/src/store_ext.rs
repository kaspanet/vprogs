use std::sync::Arc;

use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::ReadStore;

/// Extension trait for store test assertions.
pub trait StoreExt {
    /// Asserts that a resource has the expected version and writer log.
    fn assert_written_state(&self, resource_id: usize, writers: Vec<usize>);

    /// Asserts that a resource has been deleted (no latest pointer exists).
    fn assert_resource_deleted(&self, resource_id: usize);
}

impl<S: ReadStore<StateSpace = StateSpace>> StoreExt for Arc<S> {
    fn assert_written_state(&self, resource_id: usize, writers: Vec<usize>) {
        let writer_count = writers.len();
        let writer_log: Vec<u8> = writers.iter().flat_map(|id| id.to_be_bytes()).collect();

        let versioned_state = StateVersion::<usize>::from_latest_data(self.as_ref(), resource_id);
        assert_eq!(versioned_state.version(), writer_count as u64);
        assert_eq!(*versioned_state.data(), writer_log);
    }

    fn assert_resource_deleted(&self, resource_id: usize) {
        let id_bytes = resource_id.to_be_bytes();
        assert!(
            self.get(StateSpace::StatePtrLatest, &id_bytes).is_none(),
            "Resource {} should have been deleted but still exists",
            resource_id
        );
    }
}
