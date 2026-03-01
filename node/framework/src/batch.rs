use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_atomics::AtomicAsyncLatch;

use crate::NodeVm;

/// One-shot container for pre-processing results.
///
/// A `Batch` is created when a chain block arrives and handed to an execution worker for
/// pre-processing. The worker fills in the resulting transactions and metadata via
/// [`set`](Self::set) and opens the latch, signaling the event loop that this batch is prepared.
///
/// Fully lock-free: results are published through an [`ArcSwapOption`] and coordination goes
/// through the [`AtomicAsyncLatch`].
pub(crate) struct Batch<V: NodeVm> {
    index: u64,
    result: ArcSwapOption<(Vec<V::Transaction>, V::BatchMetadata)>,
    prepared: AtomicAsyncLatch,
}

impl<V: NodeVm> Batch<V> {
    /// Creates a new empty batch for the given block index.
    pub(crate) fn new(index: u64) -> Arc<Self> {
        Arc::new(Self { index, result: ArcSwapOption::empty(), prepared: AtomicAsyncLatch::new() })
    }

    /// Returns the block index this batch corresponds to.
    pub(crate) fn index(&self) -> u64 {
        self.index
    }

    /// Returns `true` if pre-processing has completed.
    pub(crate) fn is_prepared(&self) -> bool {
        self.prepared.is_open()
    }

    /// Asynchronously waits until pre-processing completes.
    pub(crate) async fn wait_until_prepared(&self) {
        self.prepared.wait().await;
    }

    /// Stores the pre-processed transactions and metadata, marking the batch as prepared.
    ///
    /// Called by the execution worker after [`NodeVm::pre_process_block`] returns.
    pub(crate) fn set(&self, txs: Vec<V::Transaction>, metadata: V::BatchMetadata) {
        self.result.store(Some(Arc::new((txs, metadata))));
        self.prepared.open();
    }

    /// Takes the pre-processed transactions and metadata out of the batch.
    ///
    /// Must only be called after [`wait_until_prepared`](Self::wait_until_prepared) or
    /// [`is_prepared`](Self::is_prepared) returns `true`. Panics if the results have already been
    /// taken.
    pub(crate) fn take(&self) -> (Vec<V::Transaction>, V::BatchMetadata) {
        let arc = self.result.swap(None).expect("results already taken");
        Arc::into_inner(arc).expect("outstanding references to results")
    }
}
