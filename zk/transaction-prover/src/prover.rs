use std::sync::Arc;

use tap::Tap;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::{Processor, ScheduledTransaction};
use vprogs_storage_types::Store;

use crate::{Backend, worker::Worker};

/// Transaction prover that dispatches proofs to a background worker.
#[smart_pointer]
pub struct TransactionProver<S: Store, P: Processor<S>> {
    /// Transactions awaiting proving.
    pub(crate) inbox: AsyncQueue<(ScheduledTransaction<S, P>, Vec<u8>)>,
    /// Opened to signal worker shutdown.
    pub(crate) shutdown: AtomicAsyncLatch,
}

impl<S: Store, P: Processor<S>> TransactionProver<S, P> {
    /// Creates a new transaction prover and spawns its worker thread.
    pub fn new<B: Backend<Receipt = P::TransactionEffects>>(backend: B) -> Self {
        Self(Arc::new(TransactionProverData {
            inbox: AsyncQueue::new(),
            shutdown: AtomicAsyncLatch::new(),
        }))
        .tap(|p| Worker::spawn(p.clone(), backend))
    }

    /// Submits a transaction for proving.
    pub fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        self.inbox.push((tx.clone(), tx_inputs));
    }

    /// Signals the worker to shut down.
    pub fn shutdown(&self) {
        self.shutdown.open();
    }
}
