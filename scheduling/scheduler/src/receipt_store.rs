use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_state_proof_receipt::AggregatorKey;
use vprogs_storage_manager::StorageManager;
use vprogs_storage_types::Store;

use crate::{
    Read, ReadReceipt, ReceiptRead, StoreReceipt, Write, processor::Processor,
    storage_cmd::ReceiptLookup,
};

/// Handle to the proof-receipt cache: submits typed receipt reads and writes to the storage
/// manager's background workers, off the caller's async runtime.
///
/// Cloneable and shared, obtained from a [`SchedulerState`](crate::SchedulerState) via
/// [`receipt_store`](crate::SchedulerState::receipt_store). A read or write keys the
/// [`StateSpace::ProofReceipt`](vprogs_storage_types::StateSpace::ProofReceipt) column family by
/// the typed key alone, so receipt I/O is independent of any batch's lifecycle: a component holding
/// no batch reaches the cache through this handle rather than borrowing one as a storage gateway.
#[derive(Clone)]
pub struct ReceiptStore<S: Store, P: Processor<S>> {
    /// Storage manager whose read/write workers serve the proof-receipt column family.
    storage: StorageManager<S, Read<S, P>, Write<S, P>>,
}

impl<S: Store, P: Processor<S>> ReceiptStore<S, P> {
    /// Wraps a storage manager as a receipt-store handle.
    pub(crate) fn new(storage: StorageManager<S, Read<S, P>, Write<S, P>>) -> Self {
        Self { storage }
    }

    /// Submits a proof-receipt lookup for the typed `key` to the read worker, returning the typed
    /// [`ReceiptRead`] handle the caller awaits. The key type determines the stored value's kind
    /// and how the served value projects back to the concrete receipt.
    pub(crate) fn read<K: ReceiptLookup<S, P>>(&self, key: K) -> ReceiptRead<S, P, K::Artifact> {
        let (cmd, handle) = ReadReceipt::new(key.into_key(), K::extract);
        self.storage.submit_read(Read::ReadReceipt(cmd));
        handle
    }

    /// Submits `receipt` under the typed `key` to the write worker, returning a latch that opens
    /// once it commits. The key type pins the receipt to its matching stored variant.
    pub(crate) fn write<K: ReceiptLookup<S, P>>(
        &self,
        key: K,
        receipt: K::Artifact,
    ) -> AtomicAsyncLatch {
        let committed = AtomicAsyncLatch::new();
        self.storage.submit_write(Write::StoreReceipt(StoreReceipt::new(
            key.into_key(),
            K::wrap(receipt),
            committed.clone(),
        )));
        committed
    }

    /// Looks up the aggregate (settlement) receipt at `key`, resolving to the receipt or `None` on
    /// a cache miss.
    pub fn read_agg_receipt(&self, key: AggregatorKey) -> ReceiptRead<S, P, P::AggregatorArtifact> {
        self.read(key)
    }

    /// Stores the aggregate (settlement) receipt at `key` through the write worker, returning a
    /// latch that opens once it commits.
    pub fn write_agg_receipt(
        &self,
        key: AggregatorKey,
        receipt: P::AggregatorArtifact,
    ) -> AtomicAsyncLatch {
        self.write(key, receipt)
    }
}
