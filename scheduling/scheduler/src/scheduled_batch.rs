use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
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
use vprogs_state_proof_receipt::{AggregatorKey, BatchKey, Prefix};
use vprogs_storage_types::Store;

use crate::{
    CancellationContext, Read, ReadReceipt, ReceiptRead, ScheduledTransaction, Scheduler,
    StateDiff, StoreReceipt, Write, cpu_task::ManagerTask, processor::Processor,
    state::SchedulerState, storage_cmd::ReceiptLookup,
};

/// A batch of transactions progressing through the scheduler's lifecycle.
///
/// Each batch moves through three stages: processed (all transactions executed), persisted (all
/// state diffs written to disk), and committed (batch metadata finalized). When proving is active,
/// additional latches track asynchronous transaction and batch artifact publication. Callers can
/// observe progress via the query / `wait_*` methods. A batch may be canceled by a rollback, in
/// which case the wait methods return immediately.
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
    /// All transactions in this batch.
    txs: Vec<ScheduledTransaction<S, P>>,
    /// One state diff per unique resource accessed by this batch.
    state_diffs: Vec<StateDiff<S, P>>,
    /// Batch artifact (e.g. batch proof receipt), set via
    /// [`publish_artifact`](Self::publish_artifact).
    artifact: ArcSwapOption<P::BatchArtifact>,
    /// Work-stealing queue of transactions ready for execution.
    available_txs: Injector<ManagerTask<S, P>>,
    /// Number of transactions not yet fully executed.
    pending_txs: AtomicU64,
    /// Number of transactions whose artifacts haven't been published yet.
    pending_tx_artifacts: AtomicU64,
    /// Number of state diff writes not yet persisted to disk.
    pending_writes: AtomicU64,
    /// Opens when all transactions have been executed.
    processed: AtomicAsyncLatch,
    /// Opens when all transaction artifacts have been published.
    tx_artifacts_published: AtomicAsyncLatch,
    /// Opens when the batch artifact has been published via
    /// [`publish_artifact`](Self::publish_artifact).
    artifact_published: AtomicAsyncLatch,
    /// Opens when all state diffs have been written to disk.
    persisted: AtomicAsyncLatch,
    /// Opens when batch metadata has been committed.
    committed: AtomicAsyncLatch,
}

impl<S: Store, P: Processor<S>> ScheduledBatch<S, P> {
    /// Returns the checkpoint (index + metadata) identifying this batch.
    pub fn checkpoint(&self) -> &Checkpoint<P::BatchMetadata> {
        &self.checkpoint
    }

    /// Returns the transactions in this batch.
    pub fn txs(&self) -> &[ScheduledTransaction<S, P>] {
        &self.txs
    }

    /// Returns the state diffs produced by this batch (one per unique resource).
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
    pub fn num_available(&self) -> u64 {
        self.available_txs.len() as u64
    }

    /// Returns the number of transactions not yet fully executed.
    pub fn num_pending(&self) -> u64 {
        self.pending_txs.load(Ordering::Acquire)
    }

    /// Returns true if this batch was canceled by a rollback.
    pub fn canceled(&self) -> bool {
        self.checkpoint.index() > self.cancellation.threshold()
    }

    /// Returns true if all transactions have been executed.
    pub fn processed(&self) -> bool {
        self.processed.is_open()
    }

    /// Waits until all transactions have been executed, or returns immediately if canceled.
    pub async fn wait_processed(&self) {
        if !self.canceled() {
            self.processed.wait().await
        }
    }

    /// Blocking version of [`wait_processed`](Self::wait_processed).
    pub fn wait_processed_blocking(&self) -> &Self {
        if !self.canceled() {
            self.processed.wait_blocking();
        }
        self
    }

    /// Returns true if all state diffs have been written to disk.
    pub fn persisted(&self) -> bool {
        self.persisted.is_open()
    }

    /// Waits until all state diffs have been written to disk, or returns immediately if canceled.
    pub async fn wait_persisted(&self) {
        if !self.canceled() {
            self.persisted.wait().await
        }
    }

    /// Blocking version of [`wait_persisted`](Self::wait_persisted).
    pub fn wait_persisted_blocking(&self) -> &Self {
        if !self.canceled() {
            self.persisted.wait_blocking();
        }
        self
    }

    /// Returns true if the batch metadata has been committed to disk.
    pub fn committed(&self) -> bool {
        self.committed.is_open()
    }

    /// Waits until the batch has been committed, or returns immediately if canceled.
    pub async fn wait_committed(&self) {
        if !self.canceled() {
            self.committed.wait().await
        }
    }

    /// Blocking version of [`wait_committed`](Self::wait_committed).
    pub fn wait_committed_blocking(&self) -> &Self {
        if !self.canceled() {
            self.committed.wait_blocking();
        }
        self
    }

    /// Returns true if all transaction artifacts have been published.
    pub fn tx_artifacts_published(&self) -> bool {
        self.tx_artifacts_published.is_open()
    }

    /// Waits until all transaction artifacts have been published, or returns immediately if
    /// canceled.
    pub async fn wait_tx_artifacts_published(&self) {
        if !self.canceled() {
            self.tx_artifacts_published.wait().await
        }
    }

    /// Blocking version of [`wait_tx_artifacts_published`](Self::wait_tx_artifacts_published).
    pub fn wait_tx_artifacts_published_blocking(&self) -> &Self {
        if !self.canceled() {
            self.tx_artifacts_published.wait_blocking();
        }
        self
    }

    /// Returns the batch artifact.
    ///
    /// # Panics
    /// Panics if called before [`publish_artifact`](Self::publish_artifact).
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
    pub fn artifact_published(&self) -> bool {
        self.artifact_published.is_open()
    }

    /// Waits until the batch artifact has been published, or returns immediately if canceled.
    pub async fn wait_artifact_published(&self) {
        if !self.canceled() {
            self.artifact_published.wait().await
        }
    }

    /// Blocking version of [`wait_artifact_published`](Self::wait_artifact_published).
    pub fn wait_artifact_published_blocking(&self) -> &Self {
        if !self.canceled() {
            self.artifact_published.wait_blocking();
        }
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

    /// Looks up the aggregate (settlement) receipt at the start coordinate (`checkpoint_index` /
    /// `from_block`) and claimed tip `seq_commit`, with this batch as the storage gateway: the
    /// aggregate prover holds no store of its own, so it reaches the receipt cache through a batch's
    /// storage handle. Resolves to the receipt, or `None` on a miss.
    pub fn read_agg_receipt(
        &self,
        checkpoint_index: u64,
        from_block: [u8; 32],
        seq_commit: [u8; 32],
    ) -> ReceiptRead<S, P, P::AggregatorArtifact> {
        self.submit_read_receipt(self.agg_key(checkpoint_index, from_block, seq_commit))
    }

    /// Stores the aggregate (settlement) receipt at the start coordinate through the write worker,
    /// returning a latch that opens once it commits. This batch is the storage gateway, as for
    /// [`read_agg_receipt`](Self::read_agg_receipt).
    pub fn write_agg_receipt(
        &self,
        checkpoint_index: u64,
        from_block: [u8; 32],
        seq_commit: [u8; 32],
        receipt: P::AggregatorArtifact,
    ) -> AtomicAsyncLatch {
        self.submit_store_receipt(self.agg_key(checkpoint_index, from_block, seq_commit), receipt)
    }

    /// The aggregate receipt key at the start coordinate (`checkpoint_index` + `from_block`) and
    /// claimed tip `seq_commit`; this batch supplies the aggregator image id.
    fn agg_key(
        &self,
        checkpoint_index: u64,
        from_block: [u8; 32],
        seq_commit: [u8; 32],
    ) -> AggregatorKey {
        AggregatorKey {
            prefix: Prefix { checkpoint_index: checkpoint_index.into() },
            block_hash: from_block,
            image_id: self.processor.aggregator_image_id(),
            seq_commit,
        }
    }

    /// Submits a proof-receipt lookup for the typed `key` to the read worker, returning the typed
    /// [`ReceiptRead`] handle the caller awaits. The key type determines the stored value's kind
    /// and how the served value projects back to the concrete receipt.
    pub(crate) fn submit_read_receipt<K: ReceiptLookup<S, P>>(
        &self,
        key: K,
    ) -> ReceiptRead<S, P, K::Artifact> {
        let (cmd, handle) = ReadReceipt::new(key.into_key(), K::extract);
        self.state.storage().submit_read(Read::ReadReceipt(cmd));
        handle
    }

    /// Submits `receipt` under the typed `key` to the write worker, returning a latch that opens
    /// once it commits. The key type pins the receipt to its matching stored variant.
    pub(crate) fn submit_store_receipt<K: ReceiptLookup<S, P>>(
        &self,
        key: K,
        receipt: K::Artifact,
    ) -> AtomicAsyncLatch {
        let committed = AtomicAsyncLatch::new();
        self.state.storage().submit_write(Write::StoreReceipt(StoreReceipt::new(
            key.into_key(),
            K::wrap(receipt),
            committed.clone(),
        )));
        committed
    }

    /// Submits this batch for commit on the write worker. No-op if canceled.
    pub fn schedule_commit(&self) {
        if !self.canceled() {
            self.state.storage().submit_write(Write::CommitBatch(self.clone()));
        }
    }

    pub(crate) fn new(
        scheduler: &mut Scheduler<S, P>,
        txs: Vec<SchedulerTransaction<P::Transaction>>,
        checkpoint: Checkpoint<P::BatchMetadata>,
    ) -> Self {
        Self(Arc::new_cyclic(|this| {
            let processed = AtomicAsyncLatch::default();
            let persisted = AtomicAsyncLatch::default();
            let tx_artifacts_published = AtomicAsyncLatch::default();
            let artifact_published = AtomicAsyncLatch::default();

            // An empty batch has nothing to process, persist, or prove - open the latches
            // immediately so the lifecycle worker can commit it right away.
            if txs.is_empty() {
                processed.open();
                persisted.open();
                tx_artifacts_published.open();
                artifact_published.open();
            }

            let mut state_diffs = Vec::new();
            let mut resource_index = 0u32;

            ScheduledBatchData {
                cancellation: scheduler.cancellation().clone(),
                processor: scheduler.processor().clone(),
                state: scheduler.state().clone(),
                checkpoint,
                pending_txs: AtomicU64::new(txs.len() as u64),
                pending_tx_artifacts: AtomicU64::new(txs.len() as u64),
                txs: txs
                    .into_iter()
                    .enumerate()
                    .map(|(i, tx)| {
                        ScheduledTransaction::new(
                            scheduler,
                            &mut state_diffs,
                            ScheduledBatchRef(this.clone()),
                            i as u32,
                            tx,
                            &mut resource_index,
                        )
                    })
                    .collect(),
                state_diffs,
                available_txs: Injector::new(),
                pending_writes: AtomicU64::new(0),
                processed,
                tx_artifacts_published,
                artifact: ArcSwapOption::empty(),
                artifact_published,
                persisted,
                committed: Default::default(),
            }
        }))
    }

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

    pub(crate) fn push_available_tx(&self, tx: &ScheduledTransaction<S, P>) {
        self.available_txs.push(ManagerTask::ExecuteTransaction(tx.clone()));
    }

    pub(crate) fn decrease_pending_txs(&self) {
        if self.pending_txs.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.processed.open();

            // Canceled batches may never receive artifacts - open the latches immediately.
            if self.canceled() {
                self.tx_artifacts_published.open();
                self.artifact_published.open();
            }

            // Also check if persisted should open (handles case where last TX has no writes)
            if self.pending_writes.load(Ordering::Acquire) == 0 {
                self.persisted.open();
            }
        }
    }

    pub(crate) fn decrease_pending_tx_artifacts(&self) {
        if self.pending_tx_artifacts.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tx_artifacts_published.open();
        }
    }

    pub(crate) fn submit_write(&self, write: Write<S, P>) {
        if !self.canceled() {
            self.pending_writes.fetch_add(1, Ordering::AcqRel);
            self.state.storage().submit_write(write);
        }
    }

    pub(crate) fn decrease_pending_writes(&self) {
        if self.pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Double-check: once pending_txs == 0, no new writes can be submitted, so if
            // pending_writes is still 0, it will stay 0.
            if self.num_pending() == 0 && self.pending_writes.load(Ordering::Acquire) == 0 {
                self.persisted.open();
            }
        }
    }

    pub(crate) fn commit<ST: Store>(&self, store: &ST, wb: &mut ST::WriteBatch) {
        if !self.canceled() {
            // Write the latest ptr entries for all updated resources.
            let updated = self.updated_state_diffs();
            for state_diff in &updated {
                state_diff.written_state().write_latest_ptr(wb);
            }

            // Update the authenticated state tree with all resource state diffs from this batch.
            let new_root = store.update(
                wb,
                updated.into_iter().map(Commitment::from).collect(),
                self.checkpoint.index(),
            );

            StateMetadata::set_state_root(wb, &new_root);
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

    fn is_depleted(&self) -> bool {
        self.num_pending() == 0 && self.available_txs.is_empty()
    }
}
