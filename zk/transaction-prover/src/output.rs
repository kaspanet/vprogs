use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::TransactionBackend;

/// A completed transaction proof with its receipt.
///
/// Produced by the [`TransactionProver`](crate::TransactionProver) for each proved transaction.
/// In batch mode, the batch prover accumulates these into per-batch receipt sets. In
/// transaction-only mode, these flow directly to the caller's results queue.
pub struct Output<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The batch this transaction belongs to.
    pub batch: ScheduledBatch<S, P>,
    /// The transaction's position within its batch.
    pub index: u32,
    /// The proof receipt.
    pub receipt: B::Receipt,
}
