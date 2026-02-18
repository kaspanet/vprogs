use std::sync::Arc;

use tap::Tap;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::Checkpoint;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::Store;

use crate::{VmInterface, state::SchedulerState};

/// Represents a rollback operation that reverts all batches after a target checkpoint.
///
/// Walks batches from `upper_bound` down to `target.index() + 1` in reverse order, restoring each
/// affected resource to the version it had before the batch was applied.
pub struct Rollback<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// The checkpoint we're rolling back to. Its metadata is resolved by the scheduler from
    /// in-memory state to avoid a disk read race condition.
    target: Checkpoint<V::BatchMetadata>,
    /// Upper bound of the batch index range to roll back (inclusive).
    upper_bound: u64,
    /// Shared scheduler state. Used to update `last_committed` and `root` alongside disk writes.
    state: SchedulerState<S, V>,
    /// Signal that resolves when the rollback operation is complete.
    done_signal: Arc<AtomicAsyncLatch>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> Rollback<S, V> {
    /// Creates a new rollback operation that reverts all batches from `target.index() + 1` through
    /// `upper_bound` (inclusive).
    pub fn new(
        target: Checkpoint<V::BatchMetadata>,
        upper_bound: u64,
        state: SchedulerState<S, V>,
        done_signal: &Arc<AtomicAsyncLatch>,
    ) -> Self {
        Rollback { target, upper_bound, state, done_signal: done_signal.clone() }
    }

    /// Executes the rollback on `store`.
    ///
    /// Any pending writes in `write_batch` are committed first so that the rollback operates on a
    /// consistent view of state. The rollback changes are then applied and committed, and a fresh
    /// write batch is returned for further writes.
    pub fn execute<ST: Store<StateSpace = StateSpace>>(
        &self,
        store: &ST,
        write_batch: ST::WriteBatch,
    ) -> ST::WriteBatch {
        // Commit any existing changes so the rollback sees a consistent state.
        store.commit(write_batch);

        // Only update `last_committed` in memory if the target is already committed. If the target
        // batch hasn't committed yet, its `commit_done()` will advance `last_committed`.
        let target_committed = self.target.index() < self.state.last_committed().index();
        if target_committed {
            self.state.set_last_committed(Arc::new(self.target.clone()));
        }

        // On rollback-to-genesis, reset root to default so that `next_checkpoint` can
        // re-initialize it for the next scheduled batch.
        let rollback_to_genesis = self.target.index() == 0;
        if rollback_to_genesis {
            self.state.set_root(Arc::new(self.target.clone()));
        }

        // Commit all deletions atomically.
        store.commit(store.write_batch().tap_mut(|wb| {
            // Walk batches from newest to oldest.
            for index in (self.target.index() + 1..=self.upper_bound).rev() {
                // Apply all rollback pointers associated with this batch.
                for (resource_id, old_version) in StatePtrRollback::iter_batch(store, index) {
                    let resource_id =
                        borsh::from_slice(&resource_id).expect("corrupted store: unrecoverable");
                    self.apply_rollback_ptr(store, wb, index, resource_id, old_version);
                }

                // Delete batch metadata entries for this batch.
                StoredBatchMetadata::delete(wb, index);
            }

            // Only update `last_committed` on disk if the target is already committed. If the
            // target batch hasn't committed yet, its `commit_done()` will advance `last_committed`.
            if target_committed {
                StateMetadata::set_last_committed(wb, &self.target);
            }

            // Persist root reset for crash-fault tolerance.
            if rollback_to_genesis {
                StateMetadata::set_root(wb, &self.target);
            }
        }));

        // Return a new empty write batch for further operations.
        store.write_batch()
    }

    /// Signals that the rollback operation has completed.
    pub fn done(&self) {
        self.done_signal.open();
    }

    /// Applies a single rollback pointer to the write batch.
    ///
    /// This removes the current version of the resource (if any), restores the previous version,
    /// and deletes the rollback pointer entry.
    fn apply_rollback_ptr<ST: Store<StateSpace = StateSpace>>(
        &self,
        store: &ST,
        write_batch: &mut ST::WriteBatch,
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
