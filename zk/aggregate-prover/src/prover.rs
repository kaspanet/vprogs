use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_macros::smart_pointer;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::LaneProofSource;

use crate::{AggregateProverConfig, Backend, command::Command, worker::Worker};

/// Aggregate prover that composes a bundle of per-batch receipts into one settlement receipt.
///
/// Unlike the batch prover, the aggregate prover never reads SMT state: its only inputs are the
/// per-batch receipts published onto the [`ScheduledBatch`] handles (by the batch prover) and the
/// bundle's final-block lane proof (from the configured [`LaneProofSource`]). It therefore takes no
/// [`Store`] in [`new`](Self::new).
#[smart_pointer]
pub struct AggregateProver<S: Store, P: Processor<S>> {
    /// Commands awaiting processing by the worker.
    pub(crate) inbox: AsyncQueue<Command<S, P>>,
    /// Opened to signal worker shutdown.
    pub(crate) shutdown: AtomicAsyncLatch,
    /// Worker thread handle, joined on shutdown so its GPU prover tears down before process exit.
    pub(crate) worker: Mutex<Option<JoinHandle<()>>>,
}

impl<S: Store, P: Processor<S>> AggregateProver<S, P> {
    /// Creates a new aggregate prover and spawns its worker thread.
    pub fn new<B: Backend, L: LaneProofSource>(
        backend: B,
        config: AggregateProverConfig<L, B::Receipt>,
    ) -> Self
    where
        P: Processor<
                S,
                TransactionArtifact = B::Receipt,
                BatchArtifact = B::Receipt,
                AggregatorArtifact = B::Receipt,
                BatchMetadata = ChainBlockMetadata,
            >,
    {
        let prover = Self(Arc::new(AggregateProverData {
            inbox: AsyncQueue::new(),
            shutdown: AtomicAsyncLatch::new(),
            worker: Mutex::new(None),
        }));
        let handle = Worker::spawn(prover.clone(), backend, config);
        *prover.worker.lock().expect("worker mutex") = Some(handle);
        prover
    }

    /// Enqueues a batch for bundling.
    pub fn submit(&self, batch: &ScheduledBatch<S, P>) {
        self.inbox.push(Command::Batch(batch.clone()));
    }

    /// Enqueues a rollback command so the worker discards queued batches beyond the target.
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
