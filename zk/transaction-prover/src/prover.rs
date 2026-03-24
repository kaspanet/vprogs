use std::sync::Arc;

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{
    TransactionBackend, pending_batch::PendingBatch, pending_transaction::PendingTransaction,
    worker::TransactionProverWorker,
};

/// Handle for the transaction prover background thread.
///
/// Push transactions to `inbox` for proving. Completed batches (with all receipts collected)
/// appear on `outbox`.
#[smart_pointer]
pub struct TransactionProver<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The backend used for proving.
    pub backend: B,
    /// Queue of transactions awaiting proving.
    pub inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Completed batches forwarded to the batch prover.
    pub outbox: AsyncQueue<PendingBatch<P, B, S>>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> TransactionProver<P, B, S> {
    /// Creates a new transaction prover and spawns the background worker thread.
    pub fn new(backend: B) -> Self {
        let inbox = AsyncQueue::new();
        let outbox = AsyncQueue::new();

        let worker = TransactionProverWorker::new(backend.clone(), inbox.clone(), outbox.clone());
        worker.spawn();

        Self(Arc::new(TransactionProverData { backend, inbox, outbox }))
    }
}
