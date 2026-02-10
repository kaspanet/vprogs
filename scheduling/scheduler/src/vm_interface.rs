use vprogs_core_types::{AccessMetadata, BatchMetadata, ResourceId, Transaction};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{AccessHandle, RuntimeBatch};

/// Abstract transaction processor that the scheduler invokes.
///
/// Implementations define how individual transactions are executed and may
/// optionally hook into batch lifecycle events via [`Self::post_process_batch`].
pub trait VmInterface: Clone + Sized + Send + Sync + 'static {
    /// Executes a single transaction against its accessed resources.
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        tx: &Self::Transaction,
        resources: &mut [AccessHandle<S, Self>],
    ) -> Result<Self::TransactionEffects, Self::Error>;

    /// Optional hook called after all transactions in a batch have been processed.
    /// The default implementation is a no-op.
    ///
    /// Implementations should check [`RuntimeBatch::was_canceled`] before performing
    /// any work, as canceled batches still pass through this method:
    /// ```ignore
    /// fn post_process_batch<S: Store<StateSpace = StateSpace>>(&self, batch: &RuntimeBatch<S, Self>) {
    ///     if !batch.was_canceled() {
    ///         // ...
    ///     }
    /// }
    /// ```
    fn post_process_batch<S: Store<StateSpace = StateSpace>>(&self, _: &RuntimeBatch<S, Self>) {}

    /// The transaction type submitted to the scheduler.
    type Transaction: Transaction<Self::ResourceId, Self::AccessMetadata>;
    /// The effects produced by a successfully processed transaction.
    type TransactionEffects: Send + Sync + 'static;
    /// Identifies a single resource a transaction may access.
    type ResourceId: ResourceId;
    /// Per-access metadata describing how a resource is used.
    type AccessMetadata: AccessMetadata<Self::ResourceId>;
    /// Opaque metadata attached to each batch for persistence.
    type BatchMetadata: BatchMetadata;
    /// Error type returned when transaction processing fails.
    type Error;
}
