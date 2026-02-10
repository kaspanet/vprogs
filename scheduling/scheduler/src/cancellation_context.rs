use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use vprogs_core_macros::smart_pointer;

/// Shared cancellation state for in-flight batches, supporting rollback chains.
///
/// When a rollback occurs, a new context is created starting from the rollback point, with a weak
/// reference to the previous context. This forms a chain that allows in-flight batches to detect
/// when they've been canceled by comparing their index against the cancel threshold.
///
/// The `Arc` wrapper allows parallel reads (e.g. cancel threshold checks from worker threads)
/// while the scheduler's `&mut self` guarantee ensures only a single writer mutates the chain.
#[smart_pointer]
pub struct CancellationContext {
    /// Weak reference to the parent context, used to propagate cancellation during rollbacks.
    parent_context: Option<CancellationContextRef>,
    /// The first batch index assigned in this context.
    first_index: u64,
    /// Batch index threshold for cancellation. Batches with index > threshold are canceled.
    cancel_threshold: AtomicU64,
}

impl CancellationContext {
    /// Creates a new root context starting at the given index with no parent.
    pub fn new(first_index: u64) -> Self {
        Self(Arc::new(CancellationContextData {
            parent_context: None,
            first_index,
            cancel_threshold: AtomicU64::new(u64::MAX),
        }))
    }

    /// Returns the cancel threshold. Batches with index > threshold should abort.
    pub fn threshold(&self) -> u64 {
        self.cancel_threshold.load(Ordering::Acquire)
    }

    /// Performs a rollback to the given index.
    ///
    /// Replaces the current context with a new one starting at `index`. The previous context is
    /// linked as a parent with its cancel threshold set, allowing in-flight batches to detect
    /// cancellation.
    pub fn rollback(&mut self, index: u64) {
        self.0 = Arc::new(CancellationContextData {
            parent_context: self.canceled_parent_context(index),
            first_index: index,
            cancel_threshold: AtomicU64::new(u64::MAX),
        });
    }

    /// Sets the cancel threshold and finds the appropriate parent context for a rollback.
    ///
    /// Walks up the context chain to find the context that contains `index` within its range.
    /// Each visited context has its cancel threshold updated.
    fn canceled_parent_context(&self, index: u64) -> Option<CancellationContextRef> {
        // Mark batches after this index as canceled in the current context.
        self.cancel_threshold.store(index, Ordering::Release);

        // If this context contains the rollback index, it becomes the parent.
        if index >= self.first_index {
            return Some(self.downgrade());
        }

        // Otherwise, continue searching in the parent context.
        self.parent_context
            .as_ref()
            .and_then(CancellationContextRef::upgrade)
            .and_then(|parent| parent.canceled_parent_context(index))
    }
}
