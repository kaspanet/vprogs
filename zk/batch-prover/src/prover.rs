use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_macros::smart_pointer;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::{Backend, BatchProverConfig, command::Command, worker::Worker};

/// Batch prover that produces one ZK receipt per scheduled batch.
#[smart_pointer]
pub struct BatchProver<S: Store, P: Processor<S>> {
    /// Commands awaiting processing by the worker.
    pub(crate) inbox: AsyncQueue<Command<S, P>>,
    /// Opened to signal worker shutdown.
    pub(crate) shutdown: AtomicAsyncLatch,
    /// Worker thread handle, joined on shutdown so its GPU prover tears down before process exit.
    pub(crate) worker: Mutex<Option<JoinHandle<()>>>,
}

impl<S: Store, P: Processor<S>> BatchProver<S, P> {
    /// Creates a new batch prover and spawns its worker thread.
    pub fn new<B: Backend>(backend: B, store: S, config: BatchProverConfig) -> Self
    where
        P: Processor<
                S,
                TransactionArtifact = B::Receipt,
                BatchArtifact = B::Receipt,
                BatchMetadata = ChainBlockMetadata,
            >,
    {
        let prover = Self(Arc::new(BatchProverData {
            inbox: AsyncQueue::new(),
            shutdown: AtomicAsyncLatch::new(),
            worker: Mutex::new(None),
        }));
        let handle = Worker::spawn(prover.clone(), backend, store, config);
        *prover.worker.lock().expect("worker mutex") = Some(handle);
        prover
    }

    /// Enqueues a batch for proving.
    pub fn submit(&self, batch: &ScheduledBatch<S, P>) {
        self.inbox.push(Command::Batch(batch.clone()));
    }

    /// Enqueues a rollback command so the worker discards stale batches.
    pub fn rollback(&self, target_index: u64) {
        self.inbox.push(Command::Rollback(target_index));
    }

    /// Signals the worker to shut down and joins its thread, so the worker's GPU prover (and its
    /// tokio runtime) is torn down while the CUDA context is still alive. Without the join the
    /// detached worker races the process's CUDA atexit teardown and aborts ("driver shutting
    /// down").
    pub fn shutdown(&self) {
        self.shutdown.open();
        let handle = self.worker.lock().expect("worker mutex").take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}
