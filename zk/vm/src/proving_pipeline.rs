use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::BatchProver;
use vprogs_zk_transaction_prover::TransactionProver;

use crate::Backend;

/// Proving strategy for the [`Vm`](crate::Vm). Backend implementations are the extension point.
pub enum ProvingPipeline<S: Store, P: Processor<S>> {
    /// No proving -- execution only.
    None,
    /// Transaction-only proving (individual receipts, no batch aggregation).
    Transaction(TransactionProver<S, P>),
    /// Full batch proving (transaction proofs + batch aggregation with SMT witnesses).
    Batch(BatchProver<S, P>),
}

impl<S: Store, P: Processor<S>> ProvingPipeline<S, P> {
    /// Creates a transaction-only proving pipeline.
    pub fn transaction<B: Backend<Receipt = P::TransactionEffects>>(backend: B) -> Self {
        Self::Transaction(TransactionProver::new(backend))
    }

    /// Creates a full batch proving pipeline. Batch proof receipts are pushed to `results`.
    pub fn batch<B: Backend<Receipt = P::TransactionEffects>>(
        backend: B,
        store: S,
        results: AsyncQueue<B::Receipt>,
    ) -> Self {
        Self::Batch(BatchProver::new(backend, store, results))
    }

    /// Submits a transaction for proving. No-op for `None`.
    pub(crate) fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) => tx_prover.submit(tx, tx_inputs),
            Self::Batch(batch_prover) => batch_prover.submit(tx, tx_inputs),
        }
    }

    /// Shuts down the prover worker threads. No-op for `None`.
    pub fn shutdown(&self) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) => tx_prover.shutdown(),
            Self::Batch(batch_prover) => batch_prover.shutdown(),
        }
    }
}
