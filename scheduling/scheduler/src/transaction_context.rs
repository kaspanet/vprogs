use vprogs_core_types::SchedulerTransaction;
use vprogs_storage_types::Store;

use crate::{AccessHandle, ScheduledBatch, ScheduledTransaction, processor::Processor};

/// Context passed to [`Processor::process_transaction`] providing the transaction, its
/// position within the batch, the batch reference, and the resource access handles.
pub struct TransactionContext<'a, S: Store, P: Processor<S>> {
    scheduler_tx: &'a SchedulerTransaction<P::Transaction>,
    tx_index: u32,
    batch: &'a ScheduledBatch<S, P>,
    resources: Vec<AccessHandle<'a, S, P>>,
}

impl<'a, S: Store, P: Processor<S>> TransactionContext<'a, S, P> {
    pub(crate) fn new(
        scheduler_tx: &'a SchedulerTransaction<P::Transaction>,
        tx_index: u32,
        batch: &'a ScheduledBatch<S, P>,
        resources: Vec<AccessHandle<'a, S, P>>,
    ) -> Self {
        Self { scheduler_tx, tx_index, batch, resources }
    }

    /// Returns the transaction being processed.
    pub fn tx(&self) -> &P::Transaction {
        &self.scheduler_tx.tx
    }

    /// Returns the zero-based index of the transaction within its batch.
    pub fn tx_index(&self) -> u32 {
        self.tx_index
    }

    /// Returns the batch metadata associated with this execution context.
    pub fn batch_metadata(&self) -> &P::BatchMetadata {
        self.batch.checkpoint().metadata()
    }

    /// Returns a reference to the scheduled batch this transaction belongs to.
    pub fn batch(&self) -> &ScheduledBatch<S, P> {
        self.batch
    }

    /// Returns the scheduled transaction this context belongs to.
    pub fn scheduled_tx(&self) -> &ScheduledTransaction<S, P> {
        &self.batch.txs()[self.tx_index as usize]
    }

    /// Returns the resource access handles.
    pub fn resources(&self) -> &[AccessHandle<'a, S, P>] {
        &self.resources
    }

    /// Returns mutable resource access handles.
    pub fn resources_mut(&mut self) -> &mut [AccessHandle<'a, S, P>] {
        &mut self.resources
    }

    /// Returns the L2 payload bytes from the scheduler transaction.
    pub fn l2_payload(&self) -> &[u8] {
        &self.scheduler_tx.l2_payload
    }

    /// Split borrow: returns (&P::Transaction, &mut [AccessHandle]) simultaneously.
    pub fn parts_mut(&mut self) -> (&P::Transaction, &mut [AccessHandle<'a, S, P>]) {
        (&self.scheduler_tx.tx, &mut self.resources)
    }

    /// Commits changes for all resource handles. Called after successful execution.
    pub(crate) fn commit_all(self) {
        for handle in self.resources {
            handle.commit_changes();
        }
    }

    /// Rolls back changes for all resource handles. Called on failure or cancellation.
    pub(crate) fn rollback_all(self) {
        for handle in self.resources {
            handle.rollback_changes();
        }
    }
}
