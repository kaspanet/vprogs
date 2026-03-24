use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::{BatchBackend, BatchProver};
use vprogs_zk_transaction_prover::{PendingTransaction, TransactionBackend, TransactionProver};

/// Proving strategy for the [`Vm`](crate::Vm).
///
/// This is a closed set -- the backend implementations are the extension point, not the
/// pipeline structure itself.
pub enum ProvingPipeline<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// No proving -- execution only.
    None,
    /// Transaction-only proving (individual receipts, no batch aggregation).
    Transaction(TransactionProver<P, TB, S>),
    /// Full batch proving (transaction proofs + batch aggregation with SMT witnesses).
    Batch { tx_prover: TransactionProver<P, TB, S>, proof_queue: AsyncQueue<TB::Receipt> },
}

impl<P: Processor<S>, TB: TransactionBackend, S: Store> ProvingPipeline<P, TB, S> {
    /// Creates a transaction-only proving pipeline.
    pub fn transaction(backend: TB) -> Self {
        Self::Transaction(TransactionProver::new(backend))
    }

    /// Creates a full batch proving pipeline (transaction proofs + batch aggregation).
    ///
    /// Spawns both the transaction prover and batch prover worker threads. The backend handles
    /// both transaction and batch proving. The store provides SMT state proofs.
    pub fn batch(backend: TB, store: S) -> Self
    where
        TB: BatchBackend,
    {
        let tx_prover = TransactionProver::new(backend);
        let batch_prover = BatchProver::new(tx_prover.clone(), store);
        let proof_queue = batch_prover.proof_queue.clone();
        // batch_prover drops here; both workers stay alive via shared queue references.
        Self::Batch { tx_prover, proof_queue }
    }

    /// Returns the batch proof receipt queue, if batch proving is enabled.
    pub fn proof_queue(&self) -> Option<&AsyncQueue<TB::Receipt>> {
        match self {
            Self::Batch { proof_queue, .. } => Some(proof_queue),
            _ => Option::None,
        }
    }

    /// Submits a transaction for proving. No-op for `None`.
    pub(crate) fn submit(&self, batch: &ScheduledBatch<S, P>, tx_inputs: Vec<u8>) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) | Self::Batch { tx_prover, .. } => {
                tx_prover.inbox.push(PendingTransaction { batch: batch.clone(), tx_inputs });
            }
        }
    }
}

impl<P: Processor<S>, TB: TransactionBackend, S: Store> Clone for ProvingPipeline<P, TB, S> {
    fn clone(&self) -> Self {
        match self {
            Self::None => Self::None,
            Self::Transaction(tp) => Self::Transaction(tp.clone()),
            Self::Batch { tx_prover, proof_queue } => {
                Self::Batch { tx_prover: tx_prover.clone(), proof_queue: proof_queue.clone() }
            }
        }
    }
}
