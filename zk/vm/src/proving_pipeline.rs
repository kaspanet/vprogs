use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_aggregate_prover::{AggregateProver, AggregateProverConfig};
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig, LaneProofSource};
use vprogs_zk_transaction_prover::TransactionProver;

use crate::Backend;

/// Proving strategy for the [`Vm`](crate::Vm). Backend implementations are the extension point.
pub enum ProvingPipeline<S: Store, P: Processor<S>> {
    /// No proving -- execution only.
    None,
    /// Transaction-only proving.
    Transaction(TransactionProver<S, P>),
    /// Full batch proving (transaction prover + batch prover).
    Batch(TransactionProver<S, P>, BatchProver<S, P>),
    /// Full settlement proving: transaction prover + batch prover + aggregate prover. The aggregate
    /// prover consumes the per-batch receipts the batch prover publishes onto each scheduled batch.
    Aggregate(TransactionProver<S, P>, BatchProver<S, P>, AggregateProver<S, P>),
}

impl<S: Store, P: Processor<S>> ProvingPipeline<S, P> {
    /// Creates a transaction-only proving pipeline.
    pub fn transaction<B: Backend<Receipt = P::TransactionArtifact>>(backend: B) -> Self {
        Self::Transaction(TransactionProver::new(backend))
    }

    /// Creates a full batch proving pipeline.
    pub fn batch<B: Backend>(backend: B, store: S, config: BatchProverConfig) -> Self
    where
        P: Processor<
                S,
                TransactionArtifact = B::Receipt,
                BatchArtifact = B::Receipt,
                BatchMetadata = ChainBlockMetadata,
            >,
    {
        Self::Batch(
            TransactionProver::new(backend.clone()),
            BatchProver::new(backend, store, config),
        )
    }

    /// Creates a full settlement proving pipeline: transaction + batch + aggregate provers. The
    /// aggregate prover bundles the per-batch receipts the batch prover produces and proves one
    /// settlement receipt per bundle, fetching each bundle's final-block lane proof from
    /// `lane_source`.
    pub fn aggregate<B, L>(
        backend: B,
        store: S,
        batch_config: BatchProverConfig,
        agg_config: AggregateProverConfig<L, B::Receipt>,
    ) -> Self
    where
        B: Backend + vprogs_zk_aggregate_prover::Backend,
        L: LaneProofSource,
        P: Processor<
                S,
                TransactionArtifact = B::Receipt,
                BatchArtifact = B::Receipt,
                BatchMetadata = ChainBlockMetadata,
            >,
    {
        Self::Aggregate(
            TransactionProver::new(backend.clone()),
            BatchProver::new(backend.clone(), store, batch_config),
            AggregateProver::new(backend, agg_config),
        )
    }

    /// Submits a transaction for proving. No-op for `None`.
    pub(crate) fn submit_transaction(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        if let Self::Transaction(tx_prover)
        | Self::Batch(tx_prover, _)
        | Self::Aggregate(tx_prover, _, _) = self
        {
            tx_prover.submit(tx, tx_inputs);
        }
    }

    /// Submits a batch for proving. No-op unless in `Batch` or `Aggregate` mode. In `Aggregate`
    /// mode the same batch handle goes to both provers: the batch prover publishes the
    /// per-batch receipt onto it, which the aggregate prover then waits on.
    pub fn submit_batch(&self, batch: &ScheduledBatch<S, P>) {
        match self {
            Self::Batch(_, batch_prover) => batch_prover.submit(batch),
            Self::Aggregate(_, batch_prover, agg_prover) => {
                batch_prover.submit(batch);
                agg_prover.submit(batch);
            }
            _ => {}
        }
    }

    /// Rolls the batch and aggregate provers back past `target_index`. No-op unless in `Batch` or
    /// `Aggregate` mode.
    pub fn rollback(&self, target_index: u64) {
        match self {
            Self::Batch(_, batch_prover) => batch_prover.rollback(target_index),
            Self::Aggregate(_, batch_prover, agg_prover) => {
                batch_prover.rollback(target_index);
                agg_prover.rollback(target_index);
            }
            _ => {}
        }
    }

    /// Shuts down the prover worker threads. No-op for `None`. Consumers are torn down before
    /// producers (aggregate before batch before transaction), so a downstream prover can't deadlock
    /// waiting on a receipt an already-stopped upstream will never publish.
    pub fn shutdown(&self) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) => tx_prover.shutdown(),
            Self::Batch(tx_prover, batch_prover) => {
                batch_prover.shutdown();
                tx_prover.shutdown();
            }
            Self::Aggregate(tx_prover, batch_prover, agg_prover) => {
                agg_prover.shutdown();
                batch_prover.shutdown();
                tx_prover.shutdown();
            }
        }
    }
}
