use std::thread::JoinHandle;

use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;

use crate::{Backend, api::Api, worker::Worker};

/// Manages the transaction prover worker thread.
pub struct TransactionProver<S: Store, P: Processor<S>> {
    /// Shared worker state.
    api: Api<S, P>,
    /// Worker thread handle.
    #[allow(dead_code)]
    worker: JoinHandle<()>,
}

impl<S: Store, P: Processor<S>> TransactionProver<S, P> {
    /// Creates a new transaction prover and spawns its worker thread.
    pub fn new<B: Backend<Receipt = P::TransactionEffects>>(backend: B) -> Self {
        let api = Api::new();
        Self { worker: Worker::spawn(api.clone(), backend), api }
    }

    /// Submits a transaction for proving.
    pub fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        self.api.inbox.push((tx.clone(), tx_inputs));
    }

    /// Signals the background worker to shut down.
    pub fn shutdown(&self) {
        self.api.shutdown.open();
    }
}
