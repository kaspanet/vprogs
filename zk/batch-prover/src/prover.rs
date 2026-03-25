use std::thread::JoinHandle;

use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::TransactionProver;

use crate::{Backend, api::Api, worker::Worker};

/// Manager for the batch prover pipeline.
///
/// Owns a [`TransactionProver`] and the batch worker's [`JoinHandle`].
pub struct BatchProver<S: Store, P: Processor<S>> {
    tx_prover: TransactionProver<S, P>,
    api: Api<S, P>,
    #[allow(dead_code)]
    worker: JoinHandle<()>,
}

impl<S: Store, P: Processor<S>> BatchProver<S, P> {
    /// Creates a new batch prover and spawns both the transaction and batch worker threads.
    ///
    /// The store provides SMT state proofs for batch witness assembly. Batch proof receipts
    /// are pushed to `results`.
    pub fn new<B: Backend<Receipt = P::TransactionEffects>>(
        backend: B,
        store: S,
        results: AsyncQueue<B::Receipt>,
    ) -> Self {
        let tx_prover = TransactionProver::new(backend.clone());
        let api = Api::new();
        Self { tx_prover, worker: Worker::spawn(api.clone(), backend, store, results), api }
    }

    /// Submits a transaction for proving.
    ///
    /// On the first call per batch, also registers the batch with the batch prover worker.
    pub fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        if let Some(batch) = tx.batch().upgrade() {
            if batch.mark_effects_processed() {
                self.api.inbox.push(batch);
            }
        }
        self.tx_prover.submit(tx, tx_inputs);
    }
}
