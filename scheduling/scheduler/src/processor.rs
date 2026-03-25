use vprogs_core_types::BatchMetadata;
use vprogs_storage_types::Store;

use crate::{ScheduledBatch, TransactionContext};

/// Abstract transaction processor that the scheduler invokes for each transaction.
pub trait Processor<S: Store>: Clone + Sized + Send + Sync + 'static {
    /// Executes a single transaction against its local [`TransactionContext`].
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>)
    -> Result<(), Self::Error>;

    /// Called before a new batch gets scheduled (runs in the scheduler's single-threaded context).
    fn on_batch_scheduled(&self, _batch: &ScheduledBatch<S, Self>) {
        // Default implementation does nothing (override if needed).
    }

    /// Called after a rollback to `target_index` (runs in the scheduler's single-threaded context).
    fn on_rollback(&self, _target_index: u64) {
        // Default implementation does nothing (override if needed).
    }

    /// The transaction payload type (e.g. kaspa `L1Transaction`, `usize` in tests).
    /// The scheduler wraps it in `SchedulerTransaction<Self::Transaction>`.
    type Transaction: Send + Sync + 'static;
    /// Effects type for a processed transaction, published via
    /// [`ScheduledTransaction::set_effects`].
    type TransactionEffects: Send + Sync + 'static;
    /// Opaque metadata attached to each batch for persistence.
    type BatchMetadata: BatchMetadata;
    /// Error type returned when transaction processing fails.
    type Error;
}
