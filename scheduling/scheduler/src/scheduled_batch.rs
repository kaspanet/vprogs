use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwapOption;
use crossbeam_deque::{Injector, Steal, Worker};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_macros::smart_pointer;
use vprogs_core_smt::Commitment;
use vprogs_core_types::{BatchMetadata, Checkpoint, ResourceId, SchedulerTransaction};
use vprogs_scheduling_execution_workers::Batch;
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_proof_receipt::{BatchKey, Prefix};
use vprogs_storage_types::{ReadStore, Store};

use crate::{
    CancellationContext, ReceiptRead, ScheduledTransaction, Scheduler, StateDiff, Write,
    cpu_task::ManagerTask, processor::Processor, state::SchedulerState, storage_cmd::ReceiptLookup,
};

/// A batch of transactions progressing through the scheduler's lifecycle.
///
/// Each batch moves through two stages: processed (all transactions executed) and committed (batch
/// metadata finalized). State-diff writes are handed to the storage manager during execution and
/// land asynchronously under the eventual-consistency model, so there is no separate persist stage.
/// When proving is active, additional latches track asynchronous transaction and batch artifact
/// publication. Callers can observe progress via the query / `wait_*` methods. A batch may be
/// canceled by a rollback, in which case the wait methods return immediately.
#[smart_pointer]
pub struct ScheduledBatch<S: Store, P: Processor<S>> {
    /// Cancellation context captured at creation time for rollback detection.
    cancellation: CancellationContext,
    /// Processor handle for deriving the program image ids that key this batch's proof receipts.
    processor: P,
    /// Shared scheduler state for storage access and eviction.
    state: SchedulerState<S, P>,
    /// This batch's sequential index and metadata.
    checkpoint: Checkpoint<P::BatchMetadata>,
    /// True if restored from committed disk state rather than executed.
    restored: bool,
    /// All transactions in this batch.
    txs: Vec<ScheduledTransaction<S, P>>,
    /// One state diff per unique resource accessed by this batch.
    state_diffs: Vec<StateDiff<S, P>>,
    /// Batch artifact (e.g. batch proof receipt), set via `publish_artifact`.
    artifact: ArcSwapOption<P::BatchArtifact>,
    /// Work-stealing queue of transactions ready for execution.
    available_txs: Injector<ManagerTask<S, P>>,
    /// Number of transactions not yet fully executed.
    pending_txs: AtomicU64,
    /// Number of transactions whose artifacts haven't been published yet.
    pending_tx_artifacts: AtomicU64,
    /// Opens when all transactions have been executed.
    processed: AtomicAsyncLatch,
    /// Opens when all transaction artifacts have been published.
    tx_artifacts_published: AtomicAsyncLatch,
    /// Opens when the batch artifact has been published via `publish_artifact`.
    artifact_published: AtomicAsyncLatch,
    /// Opens when batch metadata has been committed.
    committed: AtomicAsyncLatch,
}

impl<S: Store, P: Processor<S>> ScheduledBatch<S, P> {
    /// Returns the checkpoint (index + metadata) identifying this batch.
    #[inline(always)]
    pub fn checkpoint(&self) -> &Checkpoint<P::BatchMetadata> {
        &self.checkpoint
    }

    /// Returns the transactions in this batch.
    #[inline(always)]
    pub fn txs(&self) -> &[ScheduledTransaction<S, P>] {
        &self.txs
    }

    /// Returns the state diffs produced by this batch (one per unique resource).
    #[inline(always)]
    pub fn state_diffs(&self) -> &[StateDiff<S, P>] {
        &self.state_diffs
    }

    /// Returns the state diffs whose written version advanced past the read version.
    pub fn updated_state_diffs(&self) -> Vec<&StateDiff<S, P>> {
        self.state_diffs.iter().filter(|d| d.data_updated()).collect()
    }

    /// Returns the resource IDs touched by this batch.
    pub fn resource_ids(&self) -> Vec<ResourceId> {
        self.state_diffs.iter().map(|d| *d.resource_id()).collect()
    }

    /// Returns an iterator over the transaction artifacts in this batch.
    pub fn tx_artifacts(&self) -> impl Iterator<Item = Arc<P::TransactionArtifact>> + '_ {
        self.txs.iter().map(|tx| tx.artifact())
    }

    /// Returns the number of transactions ready for execution.
    #[inline(always)]
    pub fn num_available(&self) -> u64 {
        self.available_txs.len() as u64
    }

    /// Returns the number of transactions not yet fully executed.
    #[inline(always)]
    pub fn num_pending(&self) -> u64 {
        self.pending_txs.load(Ordering::Acquire)
    }

    /// Returns true if this batch was canceled by a rollback.
    #[inline(always)]
    pub fn canceled(&self) -> bool {
        self.checkpoint.index() > self.cancellation.threshold()
    }

    /// Returns true if this batch was restored from committed disk state rather than executed.
    #[inline(always)]
    pub fn restored(&self) -> bool {
        self.restored
    }

    /// Returns true if all transactions have been executed.
    #[inline(always)]
    pub fn processed(&self) -> bool {
        self.processed.is_open()
    }

    /// Waits until `latch` opens or the batch is canceled by a rollback.
    async fn wait_open_or_canceled(&self, latch: &AtomicAsyncLatch) {
        tokio::select! {
            () = latch.wait() => {}
            () = self.cancellation.wait_canceled(self.checkpoint.index()) => {}
        }
    }

    /// Waits until all transactions have been executed, or until the batch is canceled by a
    /// rollback.
    pub async fn wait_processed(&self) {
        self.wait_open_or_canceled(&self.processed).await
    }

    /// Blocking version of [`wait_processed`](Self::wait_processed).
    pub fn wait_processed_blocking(&self) -> &Self {
        futures::executor::block_on(self.wait_processed());
        self
    }

    /// Returns true if the batch metadata has been committed to disk.
    #[inline(always)]
    pub fn committed(&self) -> bool {
        self.committed.is_open()
    }

    /// Waits until the batch has been committed, or until the batch is canceled by a rollback.
    pub async fn wait_committed(&self) {
        self.wait_open_or_canceled(&self.committed).await
    }

    /// Blocking version of [`wait_committed`](Self::wait_committed).
    pub fn wait_committed_blocking(&self) -> &Self {
        futures::executor::block_on(self.wait_committed());
        self
    }

    /// Returns true if all transaction artifacts have been published.
    #[inline(always)]
    pub fn tx_artifacts_published(&self) -> bool {
        self.tx_artifacts_published.is_open()
    }

    /// Waits until all transaction artifacts are published, or until the batch is canceled by a
    /// rollback.
    pub async fn wait_tx_artifacts_published(&self) {
        self.wait_open_or_canceled(&self.tx_artifacts_published).await
    }

    /// Blocking version of [`wait_tx_artifacts_published`](Self::wait_tx_artifacts_published).
    pub fn wait_tx_artifacts_published_blocking(&self) -> &Self {
        futures::executor::block_on(self.wait_tx_artifacts_published());
        self
    }

    /// Returns the batch artifact; panics if called before `publish_artifact`.
    #[inline(always)]
    pub fn artifact(&self) -> Arc<P::BatchArtifact> {
        self.artifact.load_full().expect("batch artifact not ready")
    }

    /// Publishes the batch artifact and opens the `artifact_published` latch.
    pub fn publish_artifact(&self, artifact: Option<P::BatchArtifact>) {
        if let Some(artifact) = artifact {
            self.artifact.store(Some(Arc::new(artifact)));
        }
        self.artifact_published.open();
    }

    /// Returns true if the batch artifact has been published.
    #[inline(always)]
    pub fn artifact_published(&self) -> bool {
        self.artifact_published.is_open()
    }

    /// Waits until the batch artifact has been published, or until the batch is canceled by a
    /// rollback.
    pub async fn wait_artifact_published(&self) {
        self.wait_open_or_canceled(&self.artifact_published).await
    }

    /// Blocking version of [`wait_artifact_published`](Self::wait_artifact_published).
    pub fn wait_artifact_published_blocking(&self) -> &Self {
        futures::executor::block_on(self.wait_artifact_published());
        self
    }

    /// Looks up this batch's cached per-batch receipt, returning a handle that resolves to the
    /// deserialized receipt, or `None` on a cache miss. Served by the read worker so the caller
    /// never blocks its async runtime on a store read.
    pub fn read_batch_receipt(&self) -> ReceiptRead<S, P, P::BatchArtifact> {
        self.submit_read_receipt(self.batch_key())
    }

    /// Stores this batch's per-batch receipt through the write worker, returning a latch that opens
    /// once it commits. Independent of the batch's own persistence latches.
    pub fn write_batch_receipt(&self, receipt: P::BatchArtifact) -> AtomicAsyncLatch {
        self.submit_store_receipt(self.batch_key(), receipt)
    }

    /// The per-batch receipt key at this batch's coordinate.
    fn batch_key(&self) -> BatchKey {
        BatchKey {
            prefix: Prefix { checkpoint_index: self.checkpoint.index().into() },
            block_hash: self.checkpoint.metadata().block_hash(),
            image_id: self.processor.batch_image_id(),
        }
    }

    /// Submits a proof-receipt lookup for the typed `key` through the shared receipt store,
    /// returning the typed [`ReceiptRead`] handle the caller awaits. The key type determines the
    /// stored value's kind and how the served value projects back to the concrete receipt.
    pub(crate) fn submit_read_receipt<K: ReceiptLookup<S, P>>(
        &self,
        key: K,
    ) -> ReceiptRead<S, P, K::Artifact> {
        self.state.receipt_store().read(key)
    }

    /// Submits `receipt` under the typed `key` through the shared receipt store, returning a latch
    /// that opens once it commits. The key type pins the receipt to its matching stored variant.
    pub(crate) fn submit_store_receipt<K: ReceiptLookup<S, P>>(
        &self,
        key: K,
        receipt: K::Artifact,
    ) -> AtomicAsyncLatch {
        self.state.receipt_store().write(key, receipt)
    }

    /// Submits this batch for commit on the write worker. No-op if canceled.
    pub fn schedule_commit(&self) {
        if !self.canceled() {
            self.state.storage().submit_write(Write::CommitBatch(self.clone()));
        }
    }

    /// Creates the batch, building a scheduled transaction and state diffs for each input.
    pub(crate) fn new(
        scheduler: &mut Scheduler<S, P>,
        txs: Vec<SchedulerTransaction<P::Transaction>>,
        checkpoint: Checkpoint<P::BatchMetadata>,
        restored: bool,
    ) -> Self {
        Self(Arc::new_cyclic(|this| {
            let processed = AtomicAsyncLatch::default();
            let tx_artifacts_published = AtomicAsyncLatch::default();
            let artifact_published = AtomicAsyncLatch::default();

            // An empty batch has nothing to process or prove - open the latches immediately.
            if txs.is_empty() {
                processed.open();
                tx_artifacts_published.open();
                artifact_published.open();
            }

            // Batch-local resource indices follow ascending resource-id order: the batch proof's
            // queried keys inherit this order, and the batch verifier rejects any other.
            let mut resource_indices: BTreeMap<ResourceId, u32> =
                txs.iter().flat_map(|tx| tx.resources.iter()).map(|a| (a.resource_id, 0)).collect();
            resource_indices.values_mut().enumerate().for_each(|(i, idx)| *idx = i as u32);

            let tx_count = txs.len() as u64;
            let mut state_diffs = Vec::new();
            let txs: Vec<_> = txs
                .into_iter()
                .enumerate()
                .map(|(i, tx)| {
                    ScheduledTransaction::new(
                        scheduler,
                        &mut state_diffs,
                        ScheduledBatchRef(this.clone()),
                        i as u32,
                        tx,
                        &resource_indices,
                    )
                })
                .collect();

            // State diffs are created on first touch; put them in index order so positions match
            // the assigned indices.
            state_diffs.sort_unstable_by_key(StateDiff::index);

            ScheduledBatchData {
                cancellation: scheduler.cancellation().clone(),
                processor: scheduler.processor().clone(),
                state: scheduler.state().clone(),
                checkpoint,
                restored,
                pending_txs: AtomicU64::new(tx_count),
                pending_tx_artifacts: AtomicU64::new(tx_count),
                txs,
                state_diffs,
                available_txs: Injector::new(),
                processed,
                tx_artifacts_published,
                artifact: ArcSwapOption::empty(),
                artifact_published,
                committed: Default::default(),
            }
        }))
    }

    /// Connects each transaction's resources into chains, or makes resource-free txs available.
    pub(crate) fn connect(&self) {
        for tx in self.txs() {
            if tx.resources().is_empty() {
                // Transactions with no resource dependencies are immediately available
                // for execution - no data to load, no chains to join.
                self.push_available_tx(tx);
            } else {
                for resource in tx.resources() {
                    resource.connect(self.state.storage());
                }
            }
        }
    }

    /// Restores the committed batch from disk and marks it done, skipping execution.
    pub(crate) fn restore_committed<RS: ReadStore>(&self, store: &RS) {
        // Each resource's tail access holds the batch's final state; restore it from disk.
        let index = self.checkpoint.index();
        for tx in self.txs() {
            for access in tx.resources() {
                if access.is_batch_tail() {
                    access.restore_committed_data(store, index);
                }
            }
        }

        // Nothing executed, so drive the counters down and open the done-latches directly.
        self.pending_txs.store(0, Ordering::Release);
        self.processed.open();
    }

    /// Pushes a transaction onto the work-stealing queue of ready transactions.
    #[inline(always)]
    pub(crate) fn push_available_tx(&self, tx: &ScheduledTransaction<S, P>) {
        self.available_txs.push(ManagerTask::ExecuteTransaction(tx.clone()));
    }

    /// Marks one transaction executed; opens `processed` when the last finishes.
    pub(crate) fn decrease_pending_txs(&self) {
        if self.pending_txs.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.processed.open();

            // Canceled batches may never receive artifacts - open the latches immediately.
            if self.canceled() {
                self.tx_artifacts_published.open();
                self.artifact_published.open();
            }
        }
    }

    /// Marks one tx artifact published; opens `tx_artifacts_published` when the last finishes.
    pub(crate) fn decrease_pending_tx_artifacts(&self) {
        if self.pending_tx_artifacts.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tx_artifacts_published.open();
        }
    }

    /// Submits a write to the storage manager. No-op if canceled.
    pub(crate) fn submit_write(&self, write: Write<S, P>) {
        if !self.canceled() {
            self.state.storage().submit_write(write);
        }
    }

    /// Writes the batch's latest pointers, SMT update, state root, and metadata. No-op if canceled.
    pub(crate) fn commit<ST: Store>(&self, store: &ST, wb: &mut ST::WriteBatch) {
        if !self.canceled() {
            // Write the latest ptr entries for all updated resources.
            let updated = self.updated_state_diffs();
            for state_diff in &updated {
                state_diff.written_state().write_latest_ptr(wb);
            }

            // A fresh batch updates the SMT and persists its metadata.
            if !self.restored {
                StoredBatchMetadata::set(wb, self.checkpoint.index(), self.checkpoint.metadata());
                store.update(
                    wb,
                    updated.into_iter().map(Commitment::from).collect(),
                    self.checkpoint.index(),
                );
            }

            // Record the last-committed pointer.
            StateMetadata::set_last_committed(wb, &self.checkpoint);

            // Persist root on first commit for crash-fault tolerance. Root was already set
            // in-memory when this batch was scheduled (see next_checkpoint).
            if self.checkpoint.index() == self.state.root().index() {
                StateMetadata::set_root(wb, &self.checkpoint);
            }
        }
    }

    /// Marks the batch committed, updates shared state, and queues its resources for eviction.
    pub(crate) fn commit_done(self) {
        if !self.canceled() {
            // Eagerly update last_committed in the shared state.
            self.state.set_last_committed(Arc::new(self.checkpoint.clone()));
        }

        // Mark the batch as committed.
        self.committed.open();

        // Register all resources accessed by this batch for potential eviction. The scheduler will
        // check if each resource's last access still belongs to a committed batch before actually
        // evicting it.
        for state_diff in self.state_diffs() {
            self.state.eviction_queue().push(*state_diff.resource_id());
        }
    }
}

impl<S: Store, P: Processor<S>> Batch<ManagerTask<S, P>> for ScheduledBatch<S, P> {
    /// Steals a ready transaction task for the worker, or None if the queue is empty.
    fn steal_available_tasks(
        &self,
        worker: &Worker<ManagerTask<S, P>>,
    ) -> Option<ManagerTask<S, P>> {
        loop {
            match self.available_txs.steal_batch_and_pop(worker) {
                Steal::Success(task) => return Some(task),
                Steal::Retry => continue,
                Steal::Empty => return None,
            }
        }
    }

    /// Returns true when no transactions are pending or available.
    #[inline(always)]
    fn is_depleted(&self) -> bool {
        self.num_pending() == 0 && self.available_txs.is_empty()
    }
}
