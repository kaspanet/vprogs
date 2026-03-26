use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::SchedulerTransaction;
use vprogs_storage_types::Store;

use crate::{
    AccessHandle, ResourceAccess, ScheduledBatchRef, Scheduler, StateDiff, TransactionContext,
    processor::Processor,
};

/// A transaction progressing through the scheduler's execution pipeline.
///
/// Wraps a user-submitted transaction with its resource access handles, execution state, and a
/// back-reference to the owning batch. Derefs to the inner `SchedulerTransaction`.
#[smart_pointer(deref(tx))]
pub struct ScheduledTransaction<S: Store, P: Processor<S>> {
    /// Processor used to execute this transaction.
    processor: P,
    /// Weak reference to the owning batch.
    batch: ScheduledBatchRef<S, P>,
    /// Resources accessed by this transaction, one per declared access.
    resources: Vec<ResourceAccess<S, P>>,
    /// Number of resources whose data hasn't been resolved yet.
    pending_resources: AtomicU64,
    /// Transaction artifact (e.g. proof receipt), set after processing.
    artifact: ArcSwapOption<P::TransactionArtifact>,
    /// Zero-based position of this transaction within its batch.
    tx_index: u32,
    /// The user-submitted transaction (deref target).
    tx: SchedulerTransaction<P::Transaction>,
}

impl<S: Store, P: Processor<S>> ScheduledTransaction<S, P> {
    /// Returns the resources accessed by this transaction.
    pub fn resources(&self) -> &[ResourceAccess<S, P>] {
        &self.resources
    }

    /// Returns the artifact produced by this transaction.
    ///
    /// # Panics
    /// Panics if called before [`publish_artifact`](Self::publish_artifact).
    pub fn artifact(&self) -> Arc<P::TransactionArtifact> {
        self.artifact.load_full().expect("artifact not ready")
    }

    /// Publishes this transaction's artifact. `None` skips without storing an artifact.
    pub fn publish_artifact(&self, artifact: Option<P::TransactionArtifact>) {
        if let Some(artifact) = artifact {
            self.artifact.store(Some(Arc::new(artifact)));
        }

        // Always advance the batch's pending artifact counter to ensure the latch will open.
        if let Some(batch) = self.batch.upgrade() {
            batch.decrease_pending_tx_artifacts();
        }
    }

    pub(crate) fn new(
        scheduler: &mut Scheduler<S, P>,
        state_diffs: &mut Vec<StateDiff<S, P>>,
        batch: ScheduledBatchRef<S, P>,
        tx_index: u32,
        tx: SchedulerTransaction<P::Transaction>,
        resource_index: &mut u32,
    ) -> Self {
        Self(Arc::new_cyclic(|this: &Weak<ScheduledTransactionData<S, P>>| {
            let this = ScheduledTransactionRef(this.clone());
            let resources = scheduler.resources(&tx, this, &batch, state_diffs, resource_index);
            ScheduledTransactionData {
                processor: scheduler.processor().clone(),
                pending_resources: AtomicU64::new(resources.len() as u64),
                artifact: ArcSwapOption::empty(),
                batch,
                tx_index,
                tx,
                resources,
            }
        }))
    }

    pub(crate) fn decrease_pending_resources(self) {
        if self.pending_resources.fetch_sub(1, Ordering::Relaxed) == 1 {
            if let Some(batch) = self.batch.upgrade() {
                batch.push_available_tx(&self)
            }
        }
    }

    pub(crate) fn execute(&self) {
        if let Some(batch) = self.batch.upgrade() {
            let mut ctx = TransactionContext::new(
                &self.tx,
                self.tx_index,
                &batch,
                self.resources.iter().map(AccessHandle::new).collect(),
            );

            // If the batch was canceled, roll back all changes and exit early.
            if batch.canceled() {
                ctx.rollback_all();
                batch.decrease_pending_txs();
                return;
            }

            // Process the transaction using the processor.
            match self.processor.process_transaction(&mut ctx) {
                Ok(()) => ctx.commit_all(),
                // TODO: Handle errors (e.g. store with transaction)
                Err(_) => ctx.rollback_all(),
            }

            // Notify the batch that this transaction has been processed.
            batch.decrease_pending_txs();
        }
    }

    /// Returns the weak reference to the owning batch.
    pub fn batch(&self) -> &ScheduledBatchRef<S, P> {
        &self.batch
    }
}

impl<S: Store, P: Processor<S>> ScheduledTransactionRef<S, P> {
    pub(crate) fn belongs_to_batch(&self, batch: &ScheduledBatchRef<S, P>) -> bool {
        self.upgrade().is_some_and(|tx| tx.batch() == batch)
    }
}
