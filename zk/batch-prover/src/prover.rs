use std::sync::Arc;

use tap::Tap;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

use crate::{Backend, command::Command, worker::Worker};

/// Batch prover that assembles batch witnesses and dispatches proofs to a background worker.
#[smart_pointer]
pub struct BatchProver<S: Store, P: Processor<S>> {
    /// Commands awaiting processing by the worker.
    pub(crate) inbox: AsyncQueue<Command<S, P>>,
    /// Opened to signal worker shutdown.
    pub(crate) shutdown: AtomicAsyncLatch,
    /// The kip21 seq-commit lane key (= `H_lane_key(subnetwork_id)`) this prover binds all
    /// produced batch proofs to. Pre-computed at node bootstrap and fixed for the life of the
    /// prover instance.
    pub(crate) lane_key: [u8; 32],
}

impl<S: Store, P: Processor<S>> BatchProver<S, P> {
    /// Creates a new batch prover and spawns its worker thread.
    pub fn new<B: Backend>(backend: B, store: S, lane_key: [u8; 32]) -> Self
    where
        P: Processor<S, TransactionArtifact = B::Receipt, BatchArtifact = B::Receipt>,
    {
        Self(Arc::new(BatchProverData {
            inbox: AsyncQueue::new(),
            shutdown: AtomicAsyncLatch::new(),
            lane_key,
        }))
        .tap(|p| Worker::spawn(p.clone(), backend, store))
    }

    /// Enqueues a batch for proving.
    pub fn submit(&self, batch: &ScheduledBatch<S, P>) {
        self.inbox.push(Command::Batch(batch.clone()));
    }

    /// Enqueues a rollback command so the worker discards stale batches.
    pub fn rollback(&self, target_index: u64) {
        self.inbox.push(Command::Rollback(target_index));
    }

    /// Signals the worker to shut down.
    pub fn shutdown(&self) {
        self.shutdown.open();
    }
}
