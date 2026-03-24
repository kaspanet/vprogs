use std::{marker::PhantomData, sync::Arc};

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::{PendingBatch, TransactionProver};

use crate::{BatchBackend, worker::BatchProverWorker};

/// Handle for the batch prover background thread.
///
/// Reads completed batches from the transaction prover's outbox, assembles batch witnesses with
/// SMT proofs, and produces batch receipts on `proof_queue`.
#[smart_pointer]
pub struct BatchProver<P: Processor<S>, BB: BatchBackend, S: Store> {
    /// Output queue for completed batch proof receipts.
    pub proof_queue: AsyncQueue<BB::Receipt>,
    _phantom: PhantomData<(P, S)>,
}

impl<P: Processor<S>, BB: BatchBackend, S: Store> BatchProver<P, BB, S> {
    /// Creates a new batch prover and spawns the background worker thread.
    ///
    /// The `store` provides SMT state proofs for batch witness assembly.
    pub fn new(tx_prover: TransactionProver<P, BB, S>, store: S) -> Self {
        let inbox: AsyncQueue<PendingBatch<P, BB, S>> = tx_prover.outbox.clone();
        let proof_queue = AsyncQueue::new();

        let worker =
            BatchProverWorker::new(tx_prover.backend.clone(), store, inbox, proof_queue.clone());
        worker.spawn();

        Self(Arc::new(BatchProverData { proof_queue, _phantom: PhantomData }))
    }
}
