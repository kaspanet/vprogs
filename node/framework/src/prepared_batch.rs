use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_atomics::AtomicAsyncLatch;

use crate::NodeVm;

/// One-shot container for pre-processing results.
///
/// A `PreparedBatch` is created when a chain block arrives and handed to an execution worker for
/// pre-processing. The worker fills in the resulting transactions via [`set_txs`](Self::set_txs)
/// and opens the latch, signaling the event loop that this batch is ready.
///
/// Fully lock-free: transactions are published through an [`ArcSwapOption`] and coordination goes
/// through the [`AtomicAsyncLatch`].
pub struct PreparedBatch<V: NodeVm> {
    index: u64,
    txs: ArcSwapOption<Vec<V::Transaction>>,
    ready: AtomicAsyncLatch,
}

impl<V: NodeVm> PreparedBatch<V> {
    /// Creates a new empty batch for the given block index.
    pub fn new(index: u64) -> Arc<Self> {
        Arc::new(Self { index, txs: ArcSwapOption::empty(), ready: AtomicAsyncLatch::new() })
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

    /// Stores the pre-processed transactions and signals readiness.
    ///
    /// Called by the execution worker after [`NodeVm::pre_process_block`] returns.
    pub fn set_txs(&self, txs: Vec<V::Transaction>) {
        self.txs.store(Some(Arc::new(txs)));
        self.ready.open();
    }

    /// Takes the pre-processed transactions out of the batch.
    ///
    /// Must only be called after [`wait`](Self::wait) or [`is_ready`](Self::is_ready) returns
    /// `true`. Panics if the transactions have already been taken.
    pub fn take_txs(&self) -> Vec<V::Transaction> {
        let arc = self.txs.swap(None).expect("transactions already taken");
        Arc::into_inner(arc).expect("outstanding references to transactions")
    }
}
