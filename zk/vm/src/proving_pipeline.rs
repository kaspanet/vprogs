use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::{BatchBackend, BatchProver};
use vprogs_zk_transaction_prover::{
    PendingTransaction, ProvedTransaction, TransactionBackend, TransactionProver,
};

/// Proving strategy for the [`Vm`](crate::Vm).
///
/// This is a closed set -- the backend implementations are the extension point, not the
/// pipeline structure itself. Both proving modes accept a caller-owned results queue at
/// construction time.
#[derive(Clone)]
pub enum ProvingPipeline<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// No proving -- execution only.
    None,
    /// Transaction-only proving (individual receipts, no batch aggregation).
    Transaction(TransactionProver<P, TB, S>),
    /// Full batch proving (transaction proofs + batch aggregation with SMT witnesses).
    Batch(BatchProver<P, TB, S>),
}

impl<P: Processor<S>, TB: TransactionBackend, S: Store> ProvingPipeline<P, TB, S> {
    /// Creates a transaction-only proving pipeline.
    ///
    /// Individual proved transaction receipts are pushed to `results`.
    pub fn transaction(backend: TB, results: AsyncQueue<ProvedTransaction<P, TB, S>>) -> Self {
        Self::Transaction(TransactionProver::new(backend, results))
    }

    /// Creates a full batch proving pipeline (transaction proofs + batch aggregation).
    ///
    /// Spawns both the transaction prover and batch prover worker threads. The backend handles
    /// both transaction and batch proving. The store provides SMT state proofs. Batch proof
    /// receipts are pushed to `results`.
    pub fn batch(backend: TB, store: S, results: AsyncQueue<TB::Receipt>) -> Self
    where
        TB: BatchBackend,
    {
        Self::Batch(BatchProver::new(backend, store, results))
    }

    /// Submits a transaction for proving. No-op for `None`.
    pub(crate) fn submit(&self, batch: &ScheduledBatch<S, P>, tx_inputs: Vec<u8>) {
        let api = match self {
            Self::None => return,
            Self::Transaction(tx_prover) => &tx_prover.api,
            Self::Batch(batch_prover) => &batch_prover.tx_prover.api,
        };
        api.inbox.push(PendingTransaction { batch: batch.clone(), tx_inputs });
    }
}
