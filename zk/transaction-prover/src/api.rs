use std::sync::Arc;

use vprogs_core_atomics::AsyncQueue;
use vprogs_core_macros::smart_pointer;
use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;

use crate::{TransactionBackend, input::Input};

/// Shared state between the [`TransactionProver`](crate::TransactionProver) handle and its
/// background worker.
///
/// This is the communication surface: the prover pushes to `inbox`, the worker reads from it
/// and proves transactions.
#[smart_pointer]
pub struct Api<P: Processor<S>, B: TransactionBackend, S: Store> {
    /// The backend used for proving.
    pub backend: B,
    /// Queue of transactions awaiting proving.
    pub inbox: AsyncQueue<Input<P, S>>,
}

impl<P: Processor<S>, B: TransactionBackend, S: Store> Api<P, B, S> {
    pub(crate) fn new(backend: B) -> Self {
        Self(Arc::new(ApiData { backend, inbox: AsyncQueue::new() }))
    }

    /// Returns true if this is the only remaining reference to the API.
    pub(crate) fn is_sole_owner(&self) -> bool {
        Arc::strong_count(&self.0) == 1
    }
}
