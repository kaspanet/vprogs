use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicU64, Ordering},
};

use crossbeam_deque::{Injector, Steal, Worker};
use crossbeam_queue::SegQueue;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_macros::smart_pointer;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_space::StateSpace;
use vprogs_storage_manager::StorageManager;
use vprogs_storage_types::{Store, WriteBatch};

use crate::{
    Read, RuntimeContext, RuntimeTx, Scheduler, StateDiff, Write, cpu_task::ManagerTask,
    vm_interface::VmInterface,
};

#[smart_pointer]
pub struct RuntimeBatch<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    runtime_context: RuntimeContext,
    index: u64,
    batch_metadata: V::BatchMetadata,
    storage: StorageManager<S, Read<S, V>, Write<S, V>>,
    eviction_queue: Arc<SegQueue<V::ResourceId>>,
    txs: Vec<RuntimeTx<S, V>>,
    state_diffs: Vec<StateDiff<S, V>>,
    available_txs: Injector<ManagerTask<S, V>>,
    pending_txs: AtomicU64,
    pending_writes: AtomicI64,
    was_processed: AtomicAsyncLatch,
    was_persisted: AtomicAsyncLatch,
    was_committed: AtomicAsyncLatch,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> RuntimeBatch<S, V> {
    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn txs(&self) -> &[RuntimeTx<S, V>] {
        &self.txs
    }

    pub fn state_diffs(&self) -> &[StateDiff<S, V>] {
        &self.state_diffs
    }

    pub fn num_available(&self) -> u64 {
        self.available_txs.len() as u64
    }

    pub fn num_pending(&self) -> u64 {
        self.pending_txs.load(Ordering::Acquire)
    }

    pub fn was_canceled(&self) -> bool {
        self.index > self.runtime_context.cancel_threshold()
    }

    pub fn was_processed(&self) -> bool {
        self.was_processed.is_open()
    }

    pub async fn wait_processed(&self) {
        if !self.was_canceled() {
            self.was_processed.wait().await
        }
    }

    pub fn wait_processed_blocking(&self) -> &Self {
        if !self.was_canceled() {
            self.was_processed.wait_blocking();
        }
        self
    }

    pub fn was_persisted(&self) -> bool {
        self.was_persisted.is_open()
    }

    pub async fn wait_persisted(&self) {
        if !self.was_canceled() {
            self.was_persisted.wait().await
        }
    }

    pub fn wait_persisted_blocking(&self) -> &Self {
        if !self.was_canceled() {
            self.was_persisted.wait_blocking();
        }
        self
    }

    pub fn was_committed(&self) -> bool {
        self.was_committed.is_open()
    }

    pub async fn wait_committed(&self) {
        if !self.was_canceled() {
            self.was_committed.wait().await
        }
    }

    pub fn wait_committed_blocking(&self) -> &Self {
        if !self.was_canceled() {
            self.was_committed.wait_blocking();
        }
        self
    }

    pub(crate) fn new(
        vm: V,
        manager: &mut Scheduler<S, V>,
        txs: Vec<V::Transaction>,
        metadata: V::BatchMetadata,
    ) -> Self {
        Self(Arc::new_cyclic(|this| {
            let mut state_diffs = Vec::new();
            let runtime_context = manager.context().clone();

            RuntimeBatchData {
                index: runtime_context.next_batch_index(),
                batch_metadata: metadata,
                storage: manager.storage_manager().clone(),
                eviction_queue: manager.eviction_queue(),
                pending_txs: AtomicU64::new(txs.len() as u64),
                pending_writes: AtomicI64::new(0),
                txs: txs
                    .into_iter()
                    .map(|tx| {
                        RuntimeTx::new(
                            &vm,
                            manager,
                            &mut state_diffs,
                            RuntimeBatchRef(this.clone()),
                            tx,
                        )
                    })
                    .collect(),
                state_diffs,
                runtime_context,
                available_txs: Injector::new(),
                was_processed: Default::default(),
                was_persisted: Default::default(),
                was_committed: Default::default(),
            }
        }))
    }

    pub(crate) fn connect(&self) {
        for tx in self.txs() {
            for resource in tx.accessed_resources() {
                resource.connect(&self.storage);
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
            self.storage.submit_write(write);
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

    pub fn schedule_commit(&self) {
        if !self.was_canceled() {
            self.storage.submit_write(Write::CommitBatch(self.clone()));
        }
    }

    pub(crate) fn commit<W>(&self, store: &mut W)
    where
        W: WriteBatch<StateSpace = StateSpace>,
    {
        if !self.was_canceled() {
            for state_diff in self.state_diffs() {
                state_diff.written_state().write_latest_ptr(store);
            }
            StoredBatchMetadata::set(store, self.index, &self.batch_metadata);
            StateMetadata::set_last_processed(store, self.index, &self.batch_metadata);
        }
    }

    pub(crate) fn commit_done(self) {
        // Mark the batch as committed.
        self.was_committed.open();

        // Register all resources accessed by this batch for potential eviction. The scheduler will
        // check if each resource's last access still belongs to a committed batch before actually
        // evicting it.
        for state_diff in self.state_diffs() {
            self.eviction_queue.push(state_diff.resource_id().clone());
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
