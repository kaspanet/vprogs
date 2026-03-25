use std::thread::JoinHandle;

use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::TransactionProver;

use crate::{Backend, api::Api, worker::Worker};

/// Manages the batch and transaction prover worker threads.
pub struct BatchProver<S: Store, P: Processor<S>> {
    /// Inner transaction prover.
    tx_prover: TransactionProver<S, P>,
    /// Shared worker state.
    api: Api<S, P>,
    /// Worker thread handle.
    #[allow(dead_code)]
    worker: JoinHandle<()>,
}

impl<S: Store, P: Processor<S>> BatchProver<S, P> {
    /// Creates a new batch prover and spawns the worker threads.
    pub fn new<B: Backend<Receipt = P::TransactionEffects>>(
        backend: B,
        store: S,
        results: AsyncQueue<B::Receipt>,
    ) -> Self {
        let tx_prover = TransactionProver::new(backend.clone());
        let api = Api::new();
        Self { tx_prover, worker: Worker::spawn(api.clone(), backend, store, results), api }
    }

    /// Submits a transaction for proving. Registers the batch on first call.
    pub fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        if let Some(batch) = tx.batch().upgrade() {
            if batch.mark_effects_processed() {
                self.api.inbox.push(batch);
            }
        }
        self.tx_prover.submit(tx, tx_inputs);
    }

    /// Signals both the batch and transaction worker threads to shut down.
    pub fn shutdown(&self) {
        self.api.shutdown.open();
        self.tx_prover.shutdown();
    }
}
