use std::thread::JoinHandle;

use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;

use crate::{Backend, api::Api, worker::Worker};

/// Manager for the transaction prover background thread.
pub struct TransactionProver<S: Store, P: Processor<S>> {
    api: Api<S, P>,
    #[allow(dead_code)]
    worker: JoinHandle<()>,
}

impl<S: Store, P: Processor<S>> TransactionProver<S, P> {
    /// Creates a new transaction prover and spawns the background worker thread.
    pub fn new<B: Backend<Receipt = P::TransactionEffects>>(backend: B) -> Self {
        let api = Api::new();
        Self { worker: Worker::spawn(api.clone(), backend), api }
    }

    /// Submits a transaction for proving.
    pub fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        self.api.inbox.push((tx.clone(), tx_inputs));
    }
}
