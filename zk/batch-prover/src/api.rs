use std::sync::Arc;

use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// Shared state between the [`BatchProver`](crate::BatchProver) and its worker.
#[smart_pointer]
pub struct Api<S: Store, P: Processor<S>> {
    /// Batches awaiting proving.
    pub inbox: AsyncQueue<ScheduledBatch<S, P>>,
    /// Opened to signal worker shutdown.
    pub shutdown: AtomicAsyncLatch,
}

impl<S: Store, P: Processor<S>> Api<S, P> {
    pub(crate) fn new() -> Self {
        Self(Arc::new(ApiData { inbox: AsyncQueue::new(), shutdown: AtomicAsyncLatch::new() }))
    }
}
