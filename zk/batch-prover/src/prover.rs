use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::{TransactionBackend, TransactionProver};

use crate::{BatchBackend, worker::Worker};

/// Handle for the batch prover background thread.
///
/// Owns a [`TransactionProver`] internally and spawns an additional worker that accumulates
/// proved transaction receipts, assembles batch witnesses (with SMT proofs), and proves each
/// completed batch. Batch proof receipts are pushed to the caller-provided results queue.
#[derive(Clone)]
pub struct BatchProver<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// The inner transaction prover -- submit transactions via `tx_prover.api.inbox`.
    pub tx_prover: TransactionProver<P, TB, S>,
}

impl<P: Processor<S>, BB: BatchBackend, S: Store> BatchProver<P, BB, S> {
    /// Creates a new batch prover and spawns both the transaction and batch worker threads.
    ///
    /// The store provides SMT state proofs for batch witness assembly. Batch proof receipts
    /// are pushed to `results`.
    pub fn new(backend: BB, store: S, results: AsyncQueue<BB::Receipt>) -> Self {
        let tx_prover = TransactionProver::new(backend, AsyncQueue::new());
        Worker::spawn(tx_prover.api.clone(), store, results);
        Self { tx_prover }
    }
}
