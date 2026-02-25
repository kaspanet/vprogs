use vprogs_core_types::{AccessMetadata, BatchMetadata, ResourceId, Transaction};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::TransactionContext;

/// Abstract transaction processor that the scheduler invokes for each transaction.
pub trait TransactionProcessor: Clone + Sized + Send + Sync + 'static {
    /// Executes a single transaction against its local [`TransactionContext`].
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<Self::TransactionEffects, Self::Error>;

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
