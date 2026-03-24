use std::sync::Arc;

use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use super::{
    batch_prover::BatchProver, pending_transaction::PendingTransaction,
    transaction_prover::TransactionProver,
};
use crate::{AsyncQueue, Backend};

/// Background orchestrator that manages proving state and delegates proof generation to a
/// [`Backend`].
///
/// Runs two dedicated worker threads:
/// - [`TransactionProver`]: dispatches transaction proofs to the backend concurrently, tracks
///   per-batch receipt accumulation, and pushes completed batches forward.
/// - [`BatchProver`]: waits for commit ordering, then assembles and proves batch witnesses.
#[smart_pointer]
pub struct ProvingOrchestrator<P: Processor<S>, R: Send + 'static, S: Store> {
    /// Queue of transactions awaiting proving.
    inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Output queue for completed batch proof receipts.
    proof_queue: AsyncQueue<R>,
}

impl<P: Processor<S>, R: Send + 'static, S: Store> ProvingOrchestrator<P, R, S> {
    /// Creates a new orchestrator and starts the transaction and batch prover threads.
    pub fn new<B: Backend<Receipt = R>>(backend: B, store: S, image_id: [u8; 32]) -> Self {
        let inbox = AsyncQueue::new();
        let proof_queue = AsyncQueue::new();
        let outbox = AsyncQueue::new();

        TransactionProver::<P, B, S>::spawn(backend.clone(), inbox.clone(), outbox.clone());
        BatchProver::<P, B, S>::spawn(backend, store, image_id, proof_queue.clone(), outbox);

        Self(Arc::new(ProvingOrchestratorData { inbox, proof_queue }))
    }

    /// Returns the output queue for completed batch proof receipts.
    pub fn proof_queue(&self) -> &AsyncQueue<R> {
        &self.proof_queue
    }

    /// Registers a transaction for proving. Called from execution worker threads.
    pub(crate) fn register_transaction(&self, batch: &ScheduledBatch<S, P>, input_bytes: Vec<u8>) {
        self.inbox.push(PendingTransaction { batch: batch.clone(), input_bytes });
    }
}
