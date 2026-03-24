use std::sync::Arc;

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{
    TransactionBackend, pending_transaction::PendingTransaction,
    proved_transaction::ProvedTransaction, worker::TransactionProverWorker,
};

/// Handle for the transaction prover background thread.
///
/// Push transactions to `inbox` for proving. Individual proved receipts appear on `outbox`.
#[smart_pointer]
pub struct TransactionProver<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The backend used for proving.
    pub backend: B,
    /// Queue of transactions awaiting proving.
    pub inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Proved transaction receipts (consumed by caller or batch prover).
    pub outbox: AsyncQueue<ProvedTransaction<P, B, S>>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> TransactionProver<P, B, S> {
    /// Creates a new transaction prover and spawns the background worker thread.
    ///
    /// Proved transaction receipts are pushed to `outbox`. In transaction-only mode this is the
    /// caller's results queue; in batch mode the batch prover reads from it.
    pub fn new(backend: B, outbox: AsyncQueue<ProvedTransaction<P, B, S>>) -> Self {
        let inbox = AsyncQueue::new();

        let worker = TransactionProverWorker::new(backend.clone(), inbox.clone(), outbox.clone());
        worker.spawn();

        Self(Arc::new(TransactionProverData { backend, inbox, outbox }))
    }
}
