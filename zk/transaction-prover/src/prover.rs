use std::thread::JoinHandle;

use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{TransactionBackend, api::Api, worker::Worker};

/// Manager for the transaction prover background thread.
///
/// Owns the [`Api`] and the worker's [`JoinHandle`]. Not cloneable -- hold a
/// [`Api`] clone instead if you need shared access to the queues.
pub struct TransactionProver<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// Shared worker API (backend, inbox).
    pub api: Api<P, B, S>,
    /// Handle to the background worker thread.
    #[allow(dead_code)]
    pub worker: JoinHandle<()>,
}

impl<P, B, S> TransactionProver<P, B, S>
where
    P: Processor<S, TransactionEffects = B::Receipt>,
    B: TransactionBackend,
    S: Store,
{
    /// Creates a new transaction prover and spawns the background worker thread.
    pub fn new(backend: B) -> Self {
        let api = Api::new(backend);
        Self { worker: Worker::spawn(api.clone()), api }
    }
}
