use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::{BatchBackend, BatchProver};
use vprogs_zk_transaction_prover::{TransactionBackend, TransactionProver};

/// Proving strategy for the [`Vm`](crate::Vm).
///
/// This is a closed set -- the backend implementations are the extension point, not the pipeline
/// structure itself.
pub enum ProvingPipeline<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// No proving -- execution only.
    None,
    /// Transaction-only proving (individual receipts, no batch aggregation).
    Transaction(TransactionProver<P, TB, S>),
    /// Full batch proving (transaction proofs + batch aggregation with SMT witnesses).
    Batch(BatchProver<P, TB, S>),
}

impl<P, TB, S> ProvingPipeline<P, TB, S>
where
    P: Processor<S, TransactionEffects = TB::Receipt>,
    TB: TransactionBackend,
    S: Store,
{
    /// Creates a transaction-only proving pipeline.
    pub fn transaction(backend: TB) -> Self {
        Self::Transaction(TransactionProver::new(backend))
    }
}

impl<P, TB, S> ProvingPipeline<P, TB, S>
where
    P: Processor<S, TransactionEffects = TB::Receipt>,
    TB: BatchBackend,
    S: Store,
{
    /// Creates a full batch proving pipeline (transaction proofs + batch aggregation).
    ///
    /// Spawns both the transaction prover and batch prover worker threads. The backend handles
    /// both transaction and batch proving. The store provides SMT state proofs. Batch proof
    /// receipts are pushed to `results`.
    pub fn batch(backend: TB, store: S, results: AsyncQueue<TB::Receipt>) -> Self {
        Self::Batch(BatchProver::new(backend, store, results))
    }
}

impl<P: Processor<S>, TB: TransactionBackend, S: Store> ProvingPipeline<P, TB, S> {
    /// Submits a transaction for proving. No-op for `None`.
    ///
    /// In batch mode, the first submission for a given batch also registers the batch with the
    /// batch prover.
    pub(crate) fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) => {
                tx_prover
                    .api
                    .inbox
                    .push(vprogs_zk_transaction_prover::Input { tx: tx.clone(), tx_inputs });
            }
            Self::Batch(batch_prover) => {
                // Register batch for proving (first time only).
                if let Some(batch) = tx.batch().upgrade() {
                    if batch.mark_effects_processed() {
                        batch_prover.api.inbox.push(batch);
                    }
                }
                batch_prover
                    .tx_prover
                    .api
                    .inbox
                    .push(vprogs_zk_transaction_prover::Input { tx: tx.clone(), tx_inputs });
            }
        }
    }
}
