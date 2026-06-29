use std::sync::Arc;

use tap::Tap;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_storage_canonical_chain::CanonicalChainSnapshot;
use vprogs_storage_types::Store;

use crate::{Processor, state::SchedulerState};

/// Represents a rollback operation that reverts the canonical batches after a target checkpoint.
///
/// Walks the canonical batches from `upper_bound` down to `target.index() + 1` in reverse order,
/// restoring each affected resource to the version it had before the batch was applied.
pub struct Rollback<S: Store, P: Processor<S>> {
    /// The checkpoint we're rolling back to. Its metadata is resolved by the scheduler from
    /// in-memory state to avoid a disk read race condition.
    target: Checkpoint<P::BatchMetadata>,
    /// Upper bound of the batch index range to roll back (inclusive).
    upper_bound: u64,
    /// Canonical chain snapshot from before the rollback.
    snapshot: Arc<CanonicalChainSnapshot>,
    /// Shared scheduler state. Used to update `last_committed` and `root` alongside disk writes.
    state: SchedulerState<S, P>,
    /// Signal that resolves when the rollback operation is complete.
    done_signal: Arc<AtomicAsyncLatch>,
}

impl<S: Store, P: Processor<S>> Rollback<S, P> {
    /// Creates a new rollback operation that reverts all batches from `target.index() + 1` through
    /// `upper_bound` (inclusive).
    pub fn new(
        target: Checkpoint<P::BatchMetadata>,
        upper_bound: u64,
        snapshot: Arc<CanonicalChainSnapshot>,
        state: SchedulerState<S, P>,
        done_signal: &Arc<AtomicAsyncLatch>,
    ) -> Self {
        Rollback { target, upper_bound, snapshot, state, done_signal: done_signal.clone() }
    }

    /// Executes the rollback on `store`.
    ///
    /// Any pending writes in `write_batch` are committed first so that the rollback operates on a
    /// consistent view of state. The rollback changes are then applied and committed, and a fresh
    /// write batch is returned for further writes.
    pub fn execute<ST: Store>(&self, store: &ST, write_batch: ST::WriteBatch) -> ST::WriteBatch {
        // Commit any existing changes so the rollback sees a consistent state.
        store.commit(write_batch);

        // Update last_committed in memory only if target already committed; else commit_done does.
        let target_committed = self.target.index() < self.state.last_committed().index();
        if target_committed {
            self.state.set_last_committed(Arc::new(self.target.clone()));
        }

        // On rollback-to-genesis, reset root so next_checkpoint can re-initialize it.
        let rollback_to_genesis = self.target.index() == 0;
        if rollback_to_genesis {
            self.state.set_root(Arc::new(self.target.clone()));
        }

        // Commit the latest-pointer repoints atomically.
        store.commit(store.write_batch().tap_mut(|wb| {
            // Walk batches from newest to oldest.
            for index in (self.target.index() + 1..=self.upper_bound).rev() {
                // Apply all rollback pointers associated with this batch.
                if self.snapshot.is_canonical(index) {
                    for (resource_id, old_version) in StatePtrRollback::iter_batch(store, index) {
                        let resource_id: ResourceId = borsh::from_slice(&resource_id)
                            .expect("corrupted store: unrecoverable");
                        self.restore_latest_ptr::<ST>(wb, resource_id, old_version);
                    }
                }
            }

            // Update last_committed on disk only if target already committed; else commit_done
            // does.
            if target_committed {
                StateMetadata::set_last_committed(wb, &self.target);
            }

            // Persist root reset for crash-fault tolerance.
            if rollback_to_genesis {
                StateMetadata::set_root(wb, &self.target);
            }

            // Reset the persisted state root to the target version's (canonical) root.
            StateMetadata::set_state_root(wb, &store.root(self.target.index()));
        }));

        // Return a new empty write batch for further operations.
        store.write_batch()
    }

    /// Signals that the rollback operation has completed.
    pub fn done(&self) {
        self.done_signal.open();
    }

    /// Repoints one resource's latest pointer to the version it had before the orphaned batch.
    fn restore_latest_ptr<ST: Store>(
        &self,
        write_batch: &mut ST::WriteBatch,
        resource_id: ResourceId,
        old_version: u64,
    ) {
        if old_version == 0 {
            // The resource did not exist before this batch.
            StatePtrLatest::delete(write_batch, &resource_id);
        } else {
            // Restore the resource to its previous (canonical) version.
            StatePtrLatest::put(write_batch, &resource_id, old_version);
        }
    }
}
