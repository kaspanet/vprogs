use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::CancellationContext;

/// Tracks the execution context for a sequence of batches, including commit progress and
/// cancellation state.
///
/// Maintains the last processed checkpoint, a queue of pending (uncommitted) checkpoints, and a
/// shared commit frontier that worker threads advance atomically when batches commit. The
/// cancellation context is a separate shared object that in-flight batches hold to detect
/// rollbacks.
pub struct BatchExecutionContext<M: BatchMetadata> {
    /// Shared cancellation state for in-flight batch detection.
    cancellation: CancellationContext,
    /// The most recently assigned checkpoint (committed or not).
    last_processed: Checkpoint<M>,
    /// The most recently committed checkpoint, updated when draining the pending queue.
    last_committed: Checkpoint<M>,
    /// Checkpoints for batches that have not yet committed, in index order.
    pending: VecDeque<Checkpoint<M>>,
    /// Highest committed batch index, updated atomically by worker threads via `fetch_max`.
    commit_frontier: Arc<AtomicU64>,
}

impl<M: BatchMetadata> BatchExecutionContext<M> {
    /// Creates a new context from the persisted pruning point and last processed checkpoint.
    ///
    /// `first_index` is the lower bound (pruning point). `last_checkpoint` is the upper bound
    /// (last processed batch, already committed from a previous session). The commit frontier is
    /// initialized to the last checkpoint's index since all prior batches are committed.
    pub fn new(first_index: u64, last_checkpoint: Checkpoint<M>) -> Self {
        let frontier = last_checkpoint.index();
        Self {
            cancellation: CancellationContext::new(first_index),
            last_committed: last_checkpoint.clone(),
            last_processed: last_checkpoint,
            pending: VecDeque::new(),
            commit_frontier: Arc::new(AtomicU64::new(frontier)),
        }
    }

    /// Returns the checkpoint of the most recently processed batch (committed or not).
    pub fn last_processed(&self) -> &Checkpoint<M> {
        &self.last_processed
    }

    /// Returns the checkpoint of the most recently committed batch.
    pub fn last_committed(&self) -> &Checkpoint<M> {
        &self.last_committed
    }

    /// Advances to the next batch, returning its checkpoint along with the shared cancellation
    /// context and commit frontier needed by the runtime batch.
    ///
    /// Drains committed entries from the front of the pending queue before returning. This is only
    /// called from the scheduler's `&mut self` context, so no concurrent writers exist.
    pub fn next_checkpoint(
        &mut self,
        metadata: M,
    ) -> (Checkpoint<M>, CancellationContext, Arc<AtomicU64>) {
        // Drain committed checkpoints from the front of the pending queue before advancing.
        self.drain_committed();

        // Advance to the next checkpoint and add it to the pending queue.
        let checkpoint = Checkpoint::new(self.last_processed.index() + 1, metadata);
        self.last_processed = checkpoint.clone();
        self.pending.push_back(checkpoint.clone());

        // Return the checkpoint along with clones of the shared cancellation context (for
        // rollback detection by in-flight batches) and the commit frontier (advanced atomically
        // by workers when a batch commits).
        (checkpoint, self.cancellation.clone(), self.commit_frontier.clone())
    }

    /// Rolls back to the given batch index, returning the target checkpoint.
    ///
    /// Looks up the target's metadata in memory first (pending queue), falling back to disk for
    /// already-committed batches. Caps the commit frontier to prevent stale `commit_done` calls
    /// from advancing past the target, truncates the pending queue, and delegates cancellation to
    /// the shared context.
    pub fn rollback<S: Store<StateSpace = StateSpace>>(
        &mut self,
        target_index: u64,
        store: &S,
    ) -> Checkpoint<M> {
        let target = self.lookup_checkpoint(target_index, store);

        // Cap the commit frontier so stale commit_done calls don't advance past the target.
        self.commit_frontier.fetch_min(target_index, Ordering::Release);

        // Remove pending entries beyond the target.
        self.pending.retain(|cp| cp.index() <= target_index);

        // Update last processed and last committed to reflect the rollback.
        self.last_processed = target.clone();
        if target_index < self.last_committed.index() {
            self.last_committed = target.clone();
        }

        // Propagate cancellation through the context chain.
        self.cancellation.rollback(target_index);

        target
    }

    /// Looks up a checkpoint by index, searching the in-memory pending queue first, then disk.
    fn lookup_checkpoint<S: Store<StateSpace = StateSpace>>(
        &self,
        index: u64,
        store: &S,
    ) -> Checkpoint<M> {
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
        Checkpoint::new(index, StoredBatchMetadata::get(store, index))
    }

    /// Drains checkpoints from the front of the pending queue that have been committed,
    /// updating `last_committed` to the most recently drained entry.
    fn drain_committed(&mut self) {
        let frontier = self.commit_frontier.load(Ordering::Acquire);
        while self.pending.front().is_some_and(|cp| cp.index() <= frontier) {
            if let Some(committed) = self.pending.pop_front() {
                self.last_committed = committed;
            }
        }
    }
}
