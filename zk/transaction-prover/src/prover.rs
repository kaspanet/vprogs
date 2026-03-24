use std::thread::JoinHandle;

use vprogs_core_atomics::AsyncQueue;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{TransactionBackend, api::Api, output::Output, worker::Worker};

/// Manager for the transaction prover background thread.
///
/// Owns the [`Api`] and the worker's [`JoinHandle`]. Not cloneable -- hold a
/// [`Api`] clone instead if you need shared access to the queues.
pub struct TransactionProver<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// Shared worker API (backend, inbox, outbox).
    pub api: Api<P, B, S>,
    /// Handle to the background worker thread.
    #[allow(dead_code)]
    pub worker: JoinHandle<()>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> TransactionProver<P, B, S> {
    /// Creates a new transaction prover and spawns the background worker thread.
    ///
    /// Proved transaction receipts are pushed to `outbox`. In transaction-only mode this is the
    /// caller's results queue; in batch mode the batch prover reads from it.
    pub fn new(backend: B, outbox: AsyncQueue<Output<P, B, S>>) -> Self {
        let api = Api::new(backend, outbox);
        Self { worker: Worker::spawn(api.clone()), api }
    }
}
