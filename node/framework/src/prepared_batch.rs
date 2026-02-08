use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_atomics::AtomicAsyncLatch;

use crate::NodeVm;

/// One-shot container for pre-processing results.
///
/// A `PreparedBatch` is created when a chain block arrives and handed to an execution worker for
/// pre-processing. The worker fills in the resulting transactions and metadata via
/// [`set`](Self::set) and opens the latch, signaling the event loop that this batch is ready.
///
/// Fully lock-free: results are published through an [`ArcSwapOption`] and coordination goes
/// through the [`AtomicAsyncLatch`].
pub struct PreparedBatch<V: NodeVm> {
    index: u64,
    result: ArcSwapOption<(Vec<V::Transaction>, V::BatchMetadata)>,
    ready: AtomicAsyncLatch,
}

impl<V: NodeVm> PreparedBatch<V> {
    /// Creates a new empty batch for the given block index.
    pub fn new(index: u64) -> Arc<Self> {
        Arc::new(Self { index, result: ArcSwapOption::empty(), ready: AtomicAsyncLatch::new() })
    }

    /// Returns the block index this batch corresponds to.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns `true` if pre-processing has completed.
    pub fn is_ready(&self) -> bool {
        self.ready.is_open()
    }

    /// Asynchronously waits until pre-processing completes.
    pub async fn wait(&self) {
        self.ready.wait().await;
    }

    /// Blocks the current thread until pre-processing completes.
    pub fn wait_blocking(&self) {
        self.ready.wait_blocking();
    }

    /// Stores the pre-processed transactions and metadata, and signals readiness.
    ///
    /// Called by the execution worker after [`NodeVm::pre_process_block`] returns.
    pub fn set(&self, txs: Vec<V::Transaction>, metadata: V::BatchMetadata) {
        self.result.store(Some(Arc::new((txs, metadata))));
        self.ready.open();
    }

    /// Takes the pre-processed transactions and metadata out of the batch.
    ///
    /// Must only be called after [`wait`](Self::wait) or [`is_ready`](Self::is_ready) returns
    /// `true`. Panics if the results have already been taken.
    pub fn take(&self) -> (Vec<V::Transaction>, V::BatchMetadata) {
        let arc = self.result.swap(None).expect("results already taken");
        Arc::into_inner(arc).expect("outstanding references to results")
    }
}
