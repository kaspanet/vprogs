use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use vprogs_core_types::{BatchMetadata, Checkpoint};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
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
pub struct BatchExecutionContext<S: Store<StateSpace = StateSpace>, M: BatchMetadata> {
    /// Backing store for checkpoint lookups during rollback.
    store: Arc<S>,
    /// Shared cancellation state for in-flight batch detection.
    cancellation: CancellationContext,
    /// The most recently processed checkpoint (committed or not).
    last_processed: Checkpoint<M>,
    /// The most recently committed checkpoint, updated when draining the pending queue.
    last_committed: Checkpoint<M>,
    /// Checkpoints for batches that have not yet committed, in index order.
    pending: VecDeque<Checkpoint<M>>,
    /// Highest committed batch index, updated atomically by worker threads via `fetch_max`.
    commit_frontier: Arc<AtomicU64>,
}

impl<S: Store<StateSpace = StateSpace>, M: BatchMetadata> BatchExecutionContext<S, M> {
    /// Creates a new context by reading persisted state from the store.
    ///
    /// The cancellation context starts from the pruning point (lower bound). Both `last_processed`
    /// and `last_committed` are initialized to the last committed checkpoint on disk, since no new
    /// batches have been scheduled yet. The commit frontier matches this index.
    pub fn new(store: Arc<S>) -> Self {
        let last_pruned: Checkpoint<M> = StateMetadata::last_pruned(&*store);
        let last_committed: Checkpoint<M> = StateMetadata::last_committed(&*store);
        let frontier = last_committed.index();
        Self {
            store,
            cancellation: CancellationContext::new(last_pruned.index()),
            last_processed: last_committed.clone(),
            last_committed,
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

    /// Rolls back to the given batch index, returning the target checkpoint and a clone of the
    /// commit frontier (needed by the `Rollback` command to reset after completion).
    ///
    /// Looks up the target's metadata in memory first (pending queue), falling back to disk for
    /// already-committed batches. Caps the commit frontier to prevent stale `commit_done` calls
    /// from advancing past the target, truncates the pending queue, and delegates cancellation to
    /// the shared context.
    pub fn rollback(&mut self, target_index: u64) -> (Checkpoint<M>, Arc<AtomicU64>) {
        let target = self.lookup_checkpoint(target_index);

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

        (target, self.commit_frontier.clone())
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
        Checkpoint::new(index, StoredBatchMetadata::get(&*self.store, index))
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
