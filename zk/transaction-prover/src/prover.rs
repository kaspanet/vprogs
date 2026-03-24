use std::{sync::Arc, thread::JoinHandle};

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{
    TransactionBackend, proved_transaction::ProvedTransaction, worker::Worker,
    worker_api::WorkerApi,
};

/// Handle for the transaction prover background thread.
///
/// Access the worker API via `api` to push transactions (`api.inbox`) or read results
/// (`api.outbox`).
#[smart_pointer]
pub struct TransactionProver<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// Shared worker API (backend, inbox, outbox).
    pub api: WorkerApi<P, B, S>,
    /// Handle to the background worker thread.
    #[allow(dead_code)]
    worker: JoinHandle<()>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> TransactionProver<P, B, S> {
    /// Creates a new transaction prover and spawns the background worker thread.
    ///
    /// Proved transaction receipts are pushed to `outbox`. In transaction-only mode this is the
    /// caller's results queue; in batch mode the batch prover reads from it.
    pub fn new(backend: B, outbox: AsyncQueue<ProvedTransaction<P, B, S>>) -> Self {
        let api = WorkerApi::new(backend, outbox);
        let worker = Worker::spawn(api.clone());
        Self(Arc::new(TransactionProverData { api, worker }))
    }
}
