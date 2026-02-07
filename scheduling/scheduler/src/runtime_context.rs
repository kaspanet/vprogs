use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use vprogs_core_macros::smart_pointer;

/// Tracks the execution context for a sequence of batches, supporting rollback operations.
///
/// A `RuntimeContext` maintains batch indexing and cancellation state. When a rollback occurs, a
/// new context is created starting from the rollback point, with a weak reference to the previous
/// context. This forms a chain that allows in-flight batches to detect when they've been canceled.
#[smart_pointer]
pub struct RuntimeContext {
    /// Weak reference to the parent context, used to propagate cancellation during rollbacks.
    parent_context: Option<RuntimeContextRef>,
    /// The first batch index assigned in this context.
    first_index: u64,
    /// The most recently assigned batch index (atomically updated).
    last_batch_index: AtomicU64,
    /// Batch index threshold for cancellation. Batches with index > threshold are canceled.
    cancel_threshold: AtomicU64,
}

impl RuntimeContext {
    /// Creates a new root context with the given batch index bounds.
    ///
    /// `first_index` is the lower bound (pruning point), `last_index` is the upper bound (last
    /// processed batch). The context has no parent and an unlimited cancel threshold, meaning no
    /// batches are initially canceled.
    pub fn new(first_index: u64, last_index: u64) -> Self {
        Self(Arc::new(RuntimeContextData {
            parent_context: None,
            first_index,
            last_batch_index: AtomicU64::new(last_index),
            cancel_threshold: AtomicU64::new(u64::MAX),
        }))
    }

    /// Returns the most recently assigned batch index.
    pub fn last_batch_index(&self) -> u64 {
        self.last_batch_index.load(Ordering::Acquire)
    }

    /// Atomically increments and returns the next batch index.
    pub fn next_batch_index(&self) -> u64 {
        self.last_batch_index.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Returns the cancel threshold. Batches with index > threshold should abort.
    pub fn cancel_threshold(&self) -> u64 {
        self.cancel_threshold.load(Ordering::Acquire)
    }

    /// Performs a rollback to the given batch index.
    ///
    /// This replaces the current context with a new one starting at `index`. The previous context
    /// is linked as a parent with its cancel threshold set, allowing in-flight batches to detect
    /// cancellation.
    pub fn rollback(&mut self, index: u64) {
        self.0 = Arc::new(RuntimeContextData {
            parent_context: self.canceled_parent_context(index),
            first_index: index,
            last_batch_index: AtomicU64::new(index),
            cancel_threshold: AtomicU64::new(u64::MAX),
        });
    }

    /// Sets the cancel threshold and finds the appropriate parent context for a rollback.
    ///
    /// Walks up the context chain to find the context that contains `index` within its range.
    /// Each visited context has its cancel threshold updated.
    fn canceled_parent_context(&self, index: u64) -> Option<RuntimeContextRef> {
        // Mark batches after this index as canceled in the current context.
        self.cancel_threshold.store(index, Ordering::Release);

        // If this context contains the rollback index, it becomes the parent.
        if index >= self.first_index {
            return Some(self.downgrade());
        }

        // Otherwise, continue searching in the parent context.
        self.parent_context
            .as_ref()
            .and_then(RuntimeContextRef::upgrade)
            .and_then(|parent| parent.canceled_parent_context(index))
    }
}
