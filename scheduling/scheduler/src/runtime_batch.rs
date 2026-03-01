use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicU64, Ordering},
};

use crossbeam_deque::{Injector, Steal, Worker};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::Checkpoint;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{Store, WriteBatch};

use crate::{
    CancellationContext, RuntimeTx, Scheduler, StateDiff, Write, cpu_task::ManagerTask,
    state::SchedulerState, vm_interface::VmInterface,
};

/// A batch of transactions progressing through the scheduler's lifecycle.
///
/// Each batch moves through three stages: processed (all transactions executed), persisted (all
/// state diffs written to disk), and committed (batch metadata finalized). Callers can observe
/// progress via the `was_*` / `wait_*` methods. A batch may be canceled by a rollback, in which
/// case the wait methods return immediately.
#[smart_pointer]
pub struct RuntimeBatch<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// Cancellation context captured at creation time for rollback detection.
    cancellation: CancellationContext,
    /// Shared scheduler state for storage access and eviction.
    state: SchedulerState<S, V>,
    /// This batch's sequential index and metadata.
    checkpoint: Checkpoint<V::BatchMetadata>,
    /// All transactions in this batch.
    txs: Vec<RuntimeTx<S, V>>,
    /// One state diff per unique resource accessed by this batch.
    state_diffs: Vec<StateDiff<S, V>>,
    /// Work-stealing queue of transactions ready for execution.
    available_txs: Injector<ManagerTask<S, V>>,
    /// Number of transactions not yet fully executed.
    pending_txs: AtomicU64,
    /// Number of state diff writes not yet persisted to disk.
    pending_writes: AtomicI64,
    /// Opens when all transactions have been executed.
    was_processed: AtomicAsyncLatch,
    /// Opens when all state diffs have been written to disk.
    was_persisted: AtomicAsyncLatch,
    /// Opens when batch metadata has been committed.
    was_committed: AtomicAsyncLatch,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> RuntimeBatch<S, V> {
    /// Returns the checkpoint (index + metadata) identifying this batch.
    pub fn checkpoint(&self) -> &Checkpoint<V::BatchMetadata> {
        &self.checkpoint
    }

    /// Returns the transactions in this batch.
    pub fn txs(&self) -> &[RuntimeTx<S, V>] {
        &self.txs
    }

    /// Returns the state diffs produced by this batch (one per unique resource).
    pub fn state_diffs(&self) -> &[StateDiff<S, V>] {
        &self.state_diffs
    }

    /// Returns the number of transactions ready for execution.
    pub fn num_available(&self) -> u64 {
        self.available_txs.len() as u64
    }

    /// Returns the number of transactions not yet fully executed.
    pub fn num_pending(&self) -> u64 {
        self.pending_txs.load(Ordering::Acquire)
    }

    /// Returns true if this batch was canceled by a rollback.
    pub fn was_canceled(&self) -> bool {
        self.checkpoint.index() > self.cancellation.threshold()
    }

    /// Returns true if all transactions have been executed.
    pub fn was_processed(&self) -> bool {
        self.was_processed.is_open()
    }

    /// Waits until all transactions have been executed, or returns immediately if canceled.
    pub async fn wait_processed(&self) {
        if !self.was_canceled() {
            self.was_processed.wait().await
        }
    }

    /// Blocking version of [`wait_processed`](Self::wait_processed).
    pub fn wait_processed_blocking(&self) -> &Self {
        if !self.was_canceled() {
            self.was_processed.wait_blocking();
        }
        self
    }

    /// Returns true if all state diffs have been written to disk.
    pub fn was_persisted(&self) -> bool {
        self.was_persisted.is_open()
    }

    /// Waits until all state diffs have been written to disk, or returns immediately if canceled.
    pub async fn wait_persisted(&self) {
        if !self.was_canceled() {
            self.was_persisted.wait().await
        }
    }

    /// Blocking version of [`wait_persisted`](Self::wait_persisted).
    pub fn wait_persisted_blocking(&self) -> &Self {
        if !self.was_canceled() {
            self.was_persisted.wait_blocking();
        }
        self
    }

    /// Returns true if the batch metadata has been committed to disk.
    pub fn was_committed(&self) -> bool {
        self.was_committed.is_open()
    }

    /// Waits until the batch has been committed, or returns immediately if canceled.
    pub async fn wait_committed(&self) {
        if !self.was_canceled() {
            self.was_committed.wait().await
        }
    }

    /// Blocking version of [`wait_committed`](Self::wait_committed).
    pub fn wait_committed_blocking(&self) -> &Self {
        if !self.was_canceled() {
            self.was_committed.wait_blocking();
        }
        self
    }

    /// Submits this batch for commit on the write worker. No-op if canceled.
    pub fn schedule_commit(&self) {
        if !self.was_canceled() {
            self.state.storage().submit_write(Write::CommitBatch(self.clone()));
        }
    }

    pub(crate) fn new(
        scheduler: &mut Scheduler<S, V>,
        txs: Vec<V::Transaction>,
        checkpoint: Checkpoint<V::BatchMetadata>,
    ) -> Self {
        Self(Arc::new_cyclic(|this| {
            let was_processed = AtomicAsyncLatch::default();
            let was_persisted = AtomicAsyncLatch::default();

            // An empty batch has nothing to process or persist — open the latches
            // immediately so the lifecycle worker can commit it right away.
            if txs.is_empty() {
                was_processed.open();
                was_persisted.open();
            }

            let mut state_diffs = Vec::new();

            RuntimeBatchData {
                checkpoint,
                cancellation: scheduler.cancellation().clone(),
                state: scheduler.state().clone(),
                pending_txs: AtomicU64::new(txs.len() as u64),
                pending_writes: AtomicI64::new(0),
                txs: txs
                    .into_iter()
                    .map(|tx| {
                        RuntimeTx::new(
                            scheduler,
                            &mut state_diffs,
                            RuntimeBatchRef(this.clone()),
                            tx,
                        )
                    })
                    .collect(),
                state_diffs,
                available_txs: Injector::new(),
                was_processed,
                was_persisted,
                was_committed: Default::default(),
            }
        }))
    }

    pub(crate) fn connect(&self) {
        for tx in self.txs() {
            for resource in tx.accessed_resources() {
                resource.connect(self.state.storage());
            }
        }
    }

    pub(crate) fn push_available_tx(&self, tx: &RuntimeTx<S, V>) {
        self.available_txs.push(ManagerTask::ExecuteTransaction(tx.clone()));
    }

    pub(crate) fn decrease_pending_txs(&self) {
        if self.pending_txs.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.was_processed.open();

            // Also check if was_persisted should open (handles case where last TX has no writes)
            if self.pending_writes.load(Ordering::Acquire) == 0 {
                self.was_persisted.open();
            }
        }
    }

    pub(crate) fn submit_write(&self, write: Write<S, V>) {
        if !self.was_canceled() {
            self.pending_writes.fetch_add(1, Ordering::AcqRel);
            self.state.storage().submit_write(write);
        }
    }

    pub(crate) fn decrease_pending_writes(&self) {
        if self.pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Double-check: once pending_txs == 0, no new writes can be submitted, so if
            // pending_writes is still 0, it will stay 0.
            if self.num_pending() == 0 && self.pending_writes.load(Ordering::Acquire) == 0 {
                self.was_persisted.open();
            }
        }
    }

    pub(crate) fn commit<W>(&self, wb: &mut W)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        if !self.was_canceled() {
            for state_diff in self.state_diffs() {
                state_diff.written_state().write_latest_ptr(wb);
            }
            StoredBatchMetadata::set(wb, self.checkpoint.index(), self.checkpoint.metadata());
            StateMetadata::set_last_committed(wb, &self.checkpoint);

            // Persist root on first commit for crash-fault tolerance. Root was already set
            // in-memory when this batch was scheduled (see next_checkpoint).
            if self.checkpoint.index() == self.state.root().index() {
                StateMetadata::set_root(wb, &self.checkpoint);
            }
        }
    }

    pub(crate) fn commit_done(self) {
        if !self.was_canceled() {
            // Eagerly update last_committed in the shared state.
            self.state.set_last_committed(Arc::new(self.checkpoint.clone()));
        }

        // Mark the batch as committed.
        self.was_committed.open();

        // Register all resources accessed by this batch for potential eviction. The scheduler will
        // check if each resource's last access still belongs to a committed batch before actually
        // evicting it.
        for state_diff in self.state_diffs() {
            self.state.eviction_queue().push(state_diff.resource_id().clone());
        }
    }
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface>
    vprogs_scheduling_execution_workers::Batch<ManagerTask<S, V>> for RuntimeBatch<S, V>
{
    fn steal_available_tasks(
        &self,
        worker: &Worker<ManagerTask<S, V>>,
    ) -> Option<ManagerTask<S, V>> {
        loop {
            match self.available_txs.steal_batch_and_pop(worker) {
                Steal::Success(task) => return Some(task),
                Steal::Retry => continue,
                Steal::Empty => return None,
            }
        }
    }

    fn is_depleted(&self) -> bool {
        self.num_pending() == 0 && self.available_txs.is_empty()
    }
}
