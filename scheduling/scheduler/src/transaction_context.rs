use vprogs_storage_types::Store;

use crate::{AccessHandle, processor::Processor};

/// Context passed to [`Processor::process_transaction`] providing the transaction, its
/// position within the batch, the batch's opaque metadata, and the resource access handles.
pub struct TransactionContext<'a, S: Store, P: Processor> {
    tx: &'a P::Transaction,
    tx_index: u32,
    batch_metadata: &'a P::BatchMetadata,
    resources: Vec<AccessHandle<'a, S, P>>,
}

impl<'a, S: Store, P: Processor> TransactionContext<'a, S, P> {
    pub(crate) fn new(
        tx: &'a P::Transaction,
        tx_index: u32,
        batch_metadata: &'a P::BatchMetadata,
        resources: Vec<AccessHandle<'a, S, P>>,
    ) -> Self {
        Self { tx, tx_index, batch_metadata, resources }
    }

    /// Returns the transaction being processed.
    pub fn transaction(&self) -> &P::Transaction {
        self.tx
    }

    /// Returns the zero-based index of the transaction within its batch.
    pub fn tx_index(&self) -> u32 {
        self.tx_index
    }

    /// Returns the batch metadata associated with this execution context.
    pub fn batch_metadata(&self) -> &P::BatchMetadata {
        self.batch_metadata
    }

    /// Returns the resource access handles.
    pub fn resources(&self) -> &[AccessHandle<'a, S, P>] {
        &self.resources
    }

    /// Returns mutable resource access handles.
    pub fn resources_mut(&mut self) -> &mut [AccessHandle<'a, S, P>] {
        &mut self.resources
    }

    /// Split borrow: returns (&Transaction, &mut [AccessHandle]) simultaneously.
    pub fn parts_mut(&mut self) -> (&P::Transaction, &mut [AccessHandle<'a, S, P>]) {
        (self.tx, &mut self.resources)
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
