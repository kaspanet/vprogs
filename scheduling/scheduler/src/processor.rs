use borsh::{BorshDeserialize, BorshSerialize};
use vprogs_core_types::BatchMetadata;
use vprogs_storage_types::Store;

use crate::{ScheduledBatch, TransactionContext};

/// Abstract transaction processor that the scheduler invokes for each transaction.
pub trait Processor<S: Store>: Clone + Sized + Send + Sync + 'static {
    /// Executes a single transaction against its local [`TransactionContext`].
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>)
    -> Result<(), Self::Error>;

    /// Called before a new batch gets scheduled (runs in the scheduler's single-threaded context).
    fn on_batch_scheduled(&self, _batch: &ScheduledBatch<S, Self>) {
        // Default implementation does nothing (override if needed).
    }

    /// Called after a rollback to `target_index` (runs in the scheduler's single-threaded context).
    fn on_rollback(&self, _target_index: u64) {
        // Default implementation does nothing (override if needed).
    }

    /// Called when the scheduler is shutting down, after all execution workers have stopped.
    fn on_shutdown(&self) {
        // Default implementation does nothing (override if needed).
    }

    /// Program identifier keying a per-transaction receipt in the proof-receipt store.
    fn tx_image_id(&self) -> [u8; 32];

    /// Program identifier keying a per-batch receipt in the proof-receipt store.
    fn batch_image_id(&self) -> [u8; 32];

    /// Program identifier keying an aggregate (settlement) receipt in the proof-receipt store.
    fn aggregator_image_id(&self) -> [u8; 32];

    /// The transaction payload type (e.g. kaspa `L1Transaction`, `usize` in tests).
    /// The scheduler wraps it in `SchedulerTransaction<Self::Transaction>`.
    type Transaction: Send + Sync + 'static;
    /// Artifact produced by a processed transaction ([`ScheduledTransaction::publish_artifact`]).
    /// The `Borsh` bounds let the scheduler cache it in (and restore it from) the proof-receipt
    /// store; `Clone` lets a served lookup hand the cached value back out of its shared slot.
    type TransactionArtifact: Clone + BorshSerialize + BorshDeserialize + Send + Sync + 'static;
    /// Artifact produced by a processed batch ([`ScheduledBatch::publish_artifact`]). Bounded for
    /// the same proof-receipt caching as [`TransactionArtifact`](Self::TransactionArtifact).
    type BatchArtifact: Clone + BorshSerialize + BorshDeserialize + Send + Sync + 'static;
    /// Receipt produced by aggregating a run of per-batch receipts into one settlement receipt.
    /// Bounded for the same proof-receipt caching as the other artifacts.
    type AggregatorArtifact: Clone + BorshSerialize + BorshDeserialize + Send + Sync + 'static;
    /// Opaque metadata attached to each batch for persistence.
    type BatchMetadata: BatchMetadata;
    /// Error type returned when transaction processing fails.
    type Error;
}
