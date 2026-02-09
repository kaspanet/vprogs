use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwap;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::{BatchMetadata, Checkpoint};

/// Tracks the execution context for a sequence of batches, supporting rollback operations.
///
/// A `BatchExecutionContext` maintains the last checkpoint of the most recently assigned
/// batch, and cancellation state. When a rollback occurs, a new context is created starting from
/// the rollback point, with a weak reference to the previous context. This forms a chain that
/// allows in-flight batches to detect when they've been canceled.
#[smart_pointer]
pub struct BatchExecutionContext<M: BatchMetadata> {
    /// Weak reference to the parent context, used to propagate cancellation during rollbacks.
    parent_context: Option<BatchExecutionContextRef<M>>,
    /// The first batch index assigned in this context.
    first_index: u64,
    /// Checkpoint of the most recently assigned batch.
    last_checkpoint: ArcSwap<Checkpoint<M>>,
    /// Batch index threshold for cancellation. Batches with index > threshold are canceled.
    cancel_threshold: AtomicU64,
}

impl<M: BatchMetadata> BatchExecutionContext<M> {
    /// Creates a new root context with the given first index and last checkpoint.
    ///
    /// `first_index` is the lower bound (pruning point). `last_checkpoint` is the upper bound
    /// (last processed batch). The context has no parent and an unlimited cancel threshold,
    /// meaning no batches are initially canceled.
    pub fn new(first_index: u64, last_checkpoint: Checkpoint<M>) -> Self {
        Self(Arc::new(BatchExecutionContextData {
            parent_context: None,
            first_index,
            last_checkpoint: ArcSwap::from_pointee(last_checkpoint),
            cancel_threshold: AtomicU64::new(u64::MAX),
        }))
    }

    /// Returns the checkpoint of the most recently assigned batch.
    pub fn last_checkpoint(&self) -> Checkpoint<M> {
        self.last_checkpoint.load().as_ref().clone()
    }

    /// Increments the batch index and updates the checkpoint. Returns the new batch index.
    pub fn assign_next_batch(&self, metadata: M) -> u64 {
        let index = self.last_checkpoint.load().index() + 1;
        self.last_checkpoint.store(Arc::new(Checkpoint::new(index, metadata)));
        index
    }

    /// Returns the cancel threshold. Batches with index > threshold should abort.
    pub fn cancel_threshold(&self) -> u64 {
        self.cancel_threshold.load(Ordering::Acquire)
    }

    /// Performs a rollback to the given checkpoint.
    ///
    /// This replaces the current context with a new one starting at the checkpoint's index. The
    /// previous context is linked as a parent with its cancel threshold set, allowing in-flight
    /// batches to detect cancellation.
    pub fn rollback(&mut self, target: Checkpoint<M>) {
        let index = target.index();
        self.0 = Arc::new(BatchExecutionContextData {
            parent_context: self.canceled_parent_context(index),
            first_index: index,
            last_checkpoint: ArcSwap::from_pointee(target),
            cancel_threshold: AtomicU64::new(u64::MAX),
        });
    }

    /// Sets the cancel threshold and finds the appropriate parent context for a rollback.
    ///
    /// Walks up the context chain to find the context that contains `index` within its range.
    /// Each visited context has its cancel threshold updated.
    fn canceled_parent_context(&self, index: u64) -> Option<BatchExecutionContextRef<M>> {
        // Mark batches after this index as canceled in the current context.
        self.cancel_threshold.store(index, Ordering::Release);

        // If this context contains the rollback index, it becomes the parent.
        if index >= self.first_index {
            return Some(self.downgrade());
        }

        // Otherwise, continue searching in the parent context.
        self.parent_context
            .as_ref()
            .and_then(BatchExecutionContextRef::upgrade)
            .and_then(|parent| parent.canceled_parent_context(index))
    }
}
