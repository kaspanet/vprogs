use std::thread::JoinHandle;

use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::{TransactionBackend, TransactionProver};

use crate::{BatchBackend, api::Api, worker::Worker};

/// Manager for the batch prover pipeline.
///
/// Owns a [`TransactionProver`] and the batch worker's [`JoinHandle`]. Not cloneable -- hold a
/// [`Api`] clone instead if you need shared access.
pub struct BatchProver<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// The inner transaction prover.
    pub tx_prover: TransactionProver<P, TB, S>,
    /// Shared batch worker API.
    pub api: Api<P, TB, S>,
    /// Handle to the batch worker thread.
    #[allow(dead_code)]
    pub worker: JoinHandle<()>,
}

impl<P: Processor<S>, BB: BatchBackend, S: Store> BatchProver<P, BB, S> {
    /// Creates a new batch prover and spawns both the transaction and batch worker threads.
    ///
    /// The store provides SMT state proofs for batch witness assembly. Batch proof receipts
    /// are pushed to `results`.
    pub fn new(backend: BB, store: S, results: AsyncQueue<BB::Receipt>) -> Self {
        let tx_prover = TransactionProver::new(backend, AsyncQueue::new());
        let api = Api::new(&tx_prover.api, store, results);
        Self { tx_prover, worker: Worker::spawn(api.clone()), api }
    }
}
