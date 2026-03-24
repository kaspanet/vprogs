use std::sync::Arc;

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_zk_transaction_prover::{Output, TransactionBackend};

/// Shared state between the [`BatchProver`](crate::BatchProver) handle and its background worker.
#[smart_pointer]
pub struct Api<P: Processor<S>, TB: TransactionBackend, S: Store> {
    /// The backend used for journal extraction, image IDs, and batch proving.
    pub backend: TB,
    /// Store for reading SMT state proofs at the pre-batch version.
    pub store: S,
    /// Proved transaction receipts from the transaction prover.
    pub inbox: AsyncQueue<Output<P, TB, S>>,
    /// Batch proof receipts.
    pub outbox: AsyncQueue<TB::Receipt>,
}

impl<P: Processor<S>, TB: TransactionBackend, S: Store> Api<P, TB, S> {
    pub(crate) fn new(
        tx_api: &vprogs_zk_transaction_prover::Api<P, TB, S>,
        store: S,
        outbox: AsyncQueue<TB::Receipt>,
    ) -> Self {
        Self(Arc::new(ApiData {
            store,
            backend: tx_api.backend.clone(),
            inbox: tx_api.outbox.clone(),
            outbox,
        }))
    }
}
