use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

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
    /// Worker thread handle, joined on shutdown so its GPU prover tears down before process exit.
    pub(crate) worker: Mutex<Option<JoinHandle<()>>>,
}

impl<S: Store, P: Processor<S>> TransactionProver<S, P> {
    /// Creates a new transaction prover and spawns its worker thread.
    pub fn new<B: Backend<Receipt = P::TransactionArtifact>>(backend: B) -> Self {
        let prover = Self(Arc::new(TransactionProverData {
            inbox: AsyncQueue::new(),
            shutdown: AtomicAsyncLatch::new(),
            worker: Mutex::new(None),
        }));
        let handle = Worker::spawn(prover.clone(), backend);
        *prover.worker.lock().expect("worker mutex") = Some(handle);
        prover
    }

    /// Submits a transaction for proving.
    pub fn submit(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        self.inbox.push((tx.clone(), tx_inputs));
    }

    /// Signals the worker to shut down and joins its thread, so the worker's GPU prover (and its
    /// tokio runtime, which cancels any in-flight proof as it drops) is torn down while the CUDA
    /// context is still alive. Without the join the detached worker races the process's CUDA atexit
    /// teardown and aborts ("driver shutting down").
    pub fn shutdown(&self) {
        self.shutdown.open();
        let handle = self.worker.lock().expect("worker mutex").take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}
