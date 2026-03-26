use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::BatchProver;
use vprogs_zk_transaction_prover::TransactionProver;

use crate::Backend;

/// Proving strategy for the [`Vm`](crate::Vm). Backend implementations are the extension point.
pub enum ProvingPipeline<S: Store, P: Processor<S>> {
    /// No proving -- execution only.
    None,
    /// Transaction-only proving.
    Transaction(TransactionProver<S, P>),
    /// Full batch proving (transaction prover + batch prover).
    Batch(TransactionProver<S, P>, BatchProver<S, P>),
}

impl<S: Store, P: Processor<S>> ProvingPipeline<S, P> {
    /// Creates a transaction-only proving pipeline.
    pub fn transaction<B: Backend<Receipt = P::TransactionEffects>>(backend: B) -> Self {
        Self::Transaction(TransactionProver::new(backend))
    }

    /// Creates a full batch proving pipeline. Batch proof receipts are pushed to `out`.
    pub fn batch<B: Backend<Receipt = P::TransactionEffects>>(
        backend: B,
        store: S,
        out: AsyncQueue<B::Receipt>,
    ) -> Self {
        Self::Batch(TransactionProver::new(backend.clone()), BatchProver::new(backend, store, out))
    }

    /// Submits a transaction for proving. No-op for `None`.
    pub(crate) fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) | Self::Batch(tx_prover, _) => {
                tx_prover.submit(tx, tx_inputs);
            }
        }
    }

    /// Schedules a batch for proving. No-op unless in `Batch` mode.
    pub fn schedule_batch(&self, batch: &ScheduledBatch<S, P>) {
        if let Self::Batch(_, batch_prover) = self {
            batch_prover.schedule_batch(batch);
        }
    }

    /// Rolls back the batch prover past `target_index`. No-op unless in `Batch` mode.
    pub fn rollback(&self, target_index: u64) {
        if let Self::Batch(_, batch_prover) = self {
            batch_prover.rollback(target_index);
        }
    }

    /// Shuts down the prover worker threads. No-op for `None`.
    pub fn shutdown(&self) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) => tx_prover.shutdown(),
            Self::Batch(tx_prover, batch_prover) => {
                batch_prover.shutdown();
                tx_prover.shutdown();
            }
        }
    }
}
