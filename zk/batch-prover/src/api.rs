use std::sync::Arc;

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::TransactionBackend;

/// Shared state between the [`BatchProver`](crate::BatchProver) handle and its background worker.
#[smart_pointer]
pub struct Api<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// The backend used for journal extraction, image IDs, and batch proving.
    pub backend: TB,
    /// Store for reading SMT state proofs at the pre-batch version.
    pub store: S,
    /// Scheduled batches awaiting proving (pushed once per batch by the pipeline).
    pub inbox: AsyncQueue<ScheduledBatch<S, P>>,
    /// Batch proof receipts.
    pub outbox: AsyncQueue<TB::Receipt>,
}

impl<P: Processor<S>, TB: TransactionBackend, S: Store> Api<P, TB, S> {
    pub(crate) fn new(backend: TB, store: S, outbox: AsyncQueue<TB::Receipt>) -> Self {
        Self(Arc::new(ApiData { backend, store, inbox: AsyncQueue::new(), outbox }))
    }
}
