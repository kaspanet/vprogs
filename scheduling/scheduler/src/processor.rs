use vprogs_core_types::BatchMetadata;
use vprogs_storage_types::Store;

use crate::TransactionContext;

/// Abstract transaction processor that the scheduler invokes for each transaction.
pub trait Processor<S: Store>: Clone + Sized + Send + Sync + 'static {
    /// Executes a single transaction against its local [`TransactionContext`].
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>)
    -> Result<(), Self::Error>;

    /// The transaction payload type (e.g. kaspa `L1Transaction`, `usize` in tests).
    /// The scheduler wraps it in `SchedulerTransaction<Self::Transaction>`.
    type Transaction: Send + Sync + 'static;
    /// The effects produced by a successfully processed transaction.
    type TransactionEffects: Send + Sync + 'static;
    /// Opaque metadata attached to each batch for persistence.
    type BatchMetadata: BatchMetadata;
    /// Error type returned when transaction processing fails.
    type Error;
}
