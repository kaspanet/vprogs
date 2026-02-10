use std::sync::Arc;

use tap::Tap;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::Store;

use crate::VmInterface;

/// Represents a rollback operation that reverts state changes made within an inclusive range of
/// batch indices.
///
/// A rollback walks batches in reverse order and restores each affected resource to the version it
/// had before the batch was applied.
pub struct Rollback<V: VmInterface> {
    /// Lower bound of the batch index range to roll back (inclusive).
    lower_bound: u64,
    /// Upper bound of the batch index range to roll back (inclusive).
    upper_bound: u64,
    /// Metadata for the target batch (the batch we're rolling back to), resolved by the scheduler
    /// from its in-memory state to avoid a disk read race condition.
    target_metadata: V::BatchMetadata,
    /// Signal that resolves when the rollback operation is complete.
    done_signal: Arc<AtomicAsyncLatch>,
}

impl<V: VmInterface> Rollback<V> {
    /// Creates a new rollback operation for the given inclusive batch range.
    pub fn new(
        lower_bound: u64,
        upper_bound: u64,
        target_metadata: V::BatchMetadata,
        done_signal: &Arc<AtomicAsyncLatch>,
    ) -> Self {
        Rollback { lower_bound, upper_bound, target_metadata, done_signal: done_signal.clone() }
    }

    /// Executes the rollback on `store`.
    ///
    /// Any pending writes in `write_batch` are committed first so that the rollback operates on a
    /// consistent view of state. The rollback changes are then applied and committed, and a fresh
    /// write batch is returned for further writes.
    pub fn execute<S: Store<StateSpace = StateSpace>>(
        &self,
        store: &S,
        write_batch: S::WriteBatch,
    ) -> S::WriteBatch {
        // Commit any existing changes so the rollback sees a consistent state.
        store.commit(write_batch);

        // Perform the rollback and commit the resulting changes.
        store.commit(self.build_rollback_batch(store));

        // Return a new empty write batch for further operations.
        store.write_batch()
    }

    /// Signals that the rollback operation has completed.
    pub fn done(&self) {
        self.done_signal.open()
    }

    /// Builds a write batch containing all rollback operations.
    fn build_rollback_batch<S: Store<StateSpace = StateSpace>>(&self, store: &S) -> S::WriteBatch {
        store.write_batch().tap_mut(|wb| {
            // Update tip atomically with the rollback.
            // This is written upfront but committed atomically with the reversions below.
            // Note: lower_bound is always >= 1 because `rollback_to` sets it to
            // `target.index() + 1`, and only enters this path when there is state to revert.
            let index = self.lower_bound - 1;
            StateMetadata::set_last_processed(
                wb,
                &Checkpoint::new(index, self.target_metadata.clone()),
            );

            // Walk batches from newest to oldest.
            for index in (self.lower_bound..=self.upper_bound).rev() {
                // Apply all rollback pointers associated with this batch.
                for (resource_id_bytes, old_version) in StatePtrRollback::iter_batch(store, index) {
                    let resource_id = V::ResourceId::from_bytes(&resource_id_bytes);
                    self.apply_rollback_ptr(store, wb, index, resource_id, old_version);
                }

                // Delete batch metadata entries for this batch.
                StoredBatchMetadata::delete(wb, index);
            }
        })
    }

    /// Applies a single rollback pointer to the write batch.
    ///
    /// This removes the current version of the resource (if any), restores the previous version,
    /// and deletes the rollback pointer entry.
    fn apply_rollback_ptr<S: Store<StateSpace = StateSpace>>(
        &self,
        store: &S,
        write_batch: &mut S::WriteBatch,
        batch_index: u64,
        resource_id: V::ResourceId,
        old_version: u64,
    ) {
        // Remove the currently live version, if present.
        if let Some(current_version) = StatePtrLatest::get(store, &resource_id) {
            StateVersion::delete(write_batch, current_version, &resource_id);
        }

        if old_version == 0 {
            // The resource did not exist before this batch.
            StatePtrLatest::delete(write_batch, &resource_id);
        } else {
            // Restore the resource to its previous version.
            StatePtrLatest::put(write_batch, &resource_id, old_version);
        }

        // Remove the rollback pointer itself.
        StatePtrRollback::delete(write_batch, batch_index, &resource_id);
    }
}
