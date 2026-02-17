use std::collections::VecDeque;

use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{CancellationContext, scheduler_context::SchedulerContext};

/// Tracks the execution context for a sequence of batches, including cancellation state.
///
/// Maintains a queue of pending (uncommitted) checkpoints and a shared cancellation context that
/// in-flight batches hold to detect rollbacks. Checkpoint progression (last_processed,
/// last_committed) is managed through the shared [`SchedulerContext`].
pub(crate) struct BatchExecutionContext<S: Store<StateSpace = StateSpace>, M: BatchMetadata> {
    /// Shared scheduler state (store, root, last_committed, last_processed).
    context: SchedulerContext<S, M>,
    /// Shared cancellation state for in-flight batch detection.
    cancellation: CancellationContext,
    /// Checkpoints for batches that have not yet committed, in index order.
    /// Retained for `lookup_checkpoint` during rollback.
    pending: VecDeque<Checkpoint<M>>,
}

impl<S: Store<StateSpace = StateSpace>, M: BatchMetadata> BatchExecutionContext<S, M> {
    /// Creates a new context from the shared scheduler state.
    ///
    /// The cancellation context starts at the root index (oldest surviving batch).
    pub fn new(context: SchedulerContext<S, M>) -> Self {
        let root_index = context.root().index();
        Self {
            context,
            cancellation: CancellationContext::new(root_index),
            pending: VecDeque::new(),
        }
    }

    /// Advances to the next batch, returning its checkpoint along with the shared cancellation
    /// context needed by the runtime batch.
    ///
    /// This is only called from the scheduler's `&mut self` context, so no concurrent writers
    /// exist for `last_processed`.
    pub fn next_checkpoint(&mut self, metadata: M) -> (Checkpoint<M>, CancellationContext) {
        self.drain_committed();

        let checkpoint = Checkpoint::new(self.context.last_processed().index() + 1, metadata);
        self.context.last_processed.store(checkpoint.clone().into());
        self.pending.push_back(checkpoint.clone());

        // Initialize root when the first batch is scheduled. On a fresh database or after
        // rollback-to-genesis, root is default (index 0). The disk write is deferred to
        // commit() for crash-fault tolerance.
        if self.context.root().index() == 0 {
            self.context.root.store(checkpoint.clone().into());
        }

        (checkpoint, self.cancellation.clone())
    }

    /// Rolls back to the given batch index, returning the target checkpoint.
    ///
    /// Looks up the target's metadata in memory first (pending queue), falling back to disk for
    /// already-committed batches. Updates the shared context and propagates cancellation, then
    /// truncates canceled entries from the tip.
    pub fn rollback(&mut self, target_index: u64) -> Checkpoint<M> {
        let target = self.lookup_checkpoint(target_index);

        // Update last_processed and last_committed via the shared context.
        self.context.last_processed.store(target.clone().into());
        if target_index < self.context.last_committed().index() {
            self.context.last_committed.store(target.clone().into());
        }

        // Propagate cancellation through the context chain so in-flight batches detect it.
        self.cancellation.rollback(target_index);

        // Pop canceled entries from the tip. Must happen after lookup_checkpoint (which
        // searches the pending queue) but before returning.
        while self.pending.back().is_some_and(|cp| cp.index() > target_index) {
            self.pending.pop_back();
        }

        target
    }

    /// Drains committed entries from the front of the pending queue (already persisted to disk).
    fn drain_committed(&mut self) {
        let committed = self.context.last_committed().index();
        while self.pending.front().is_some_and(|cp| cp.index() <= committed) {
            self.pending.pop_front();
        }
    }

    /// Looks up a checkpoint by index, searching the in-memory pending queue first, then disk.
    fn lookup_checkpoint(&self, index: u64) -> Checkpoint<M> {
        // Search pending queue (uncommitted batches still in memory).
        for cp in &self.pending {
            if cp.index() == index {
                return cp.clone();
            }
        }

        // Index 0 is the genesis state — no batch exists on disk for it.
        if index == 0 {
            return Checkpoint::default();
        }

        // Fall back to disk for committed batches.
        Checkpoint::new(index, StoredBatchMetadata::get(&**self.context.store(), index))
    }
}
