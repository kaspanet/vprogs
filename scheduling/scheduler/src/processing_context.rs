use crate::vm_interface::VmInterface;

/// Context passed to [`VmInterface::process_transaction`] providing the transaction, its position
/// within the batch, and the batch's opaque metadata.
pub struct ProcessingContext<'a, V: VmInterface> {
    tx: &'a V::Transaction,
    tx_index: u32,
    batch_metadata: &'a V::BatchMetadata,
}

impl<'a, V: VmInterface> ProcessingContext<'a, V> {
    pub(crate) fn new(
        tx: &'a V::Transaction,
        tx_index: u32,
        batch_metadata: &'a V::BatchMetadata,
    ) -> Self {
        Self { tx, tx_index, batch_metadata }
    }

    /// Returns the transaction being processed.
    pub fn transaction(&self) -> &V::Transaction {
        self.tx
    }

    /// Returns the zero-based index of the transaction within its batch.
    pub fn tx_index(&self) -> u32 {
        self.tx_index
    }

    /// Returns the batch metadata associated with this execution context.
    pub fn batch_metadata(&self) -> &V::BatchMetadata {
        self.batch_metadata
    }
}
