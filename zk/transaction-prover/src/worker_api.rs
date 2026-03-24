use std::sync::Arc;

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{
    TransactionBackend, pending_transaction::PendingTransaction,
    proved_transaction::ProvedTransaction,
};

/// Shared state between the [`TransactionProver`](crate::TransactionProver) handle and its
/// background worker.
///
/// This is the communication surface: the prover pushes to `inbox`, the worker reads from it,
/// proves transactions, and pushes results to `outbox`. Also the natural place for shutdown
/// flags or coordination state.
#[smart_pointer]
pub struct WorkerApi<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The backend used for proving.
    pub backend: B,
    /// Queue of transactions awaiting proving.
    pub inbox: AsyncQueue<PendingTransaction<P, S>>,
    /// Proved transaction receipts (consumed by caller or batch prover).
    pub outbox: AsyncQueue<ProvedTransaction<P, B, S>>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> WorkerApi<P, B, S> {
    pub(crate) fn new(backend: B, outbox: AsyncQueue<ProvedTransaction<P, B, S>>) -> Self {
        Self(Arc::new(WorkerApiData { backend, inbox: AsyncQueue::new(), outbox }))
    }

    /// Returns true if this is the only remaining reference to the worker API.
    pub(crate) fn is_sole_owner(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}
