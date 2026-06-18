use kaspa_grpc_client::GrpcClient;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, ScheduledTransaction};
use vprogs_storage_types::Store;
use vprogs_zk_batch_prover::{BatchProver, BatchProverConfig};
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
}

impl<S: Store, P: Processor<S>> ProvingPipeline<S, P> {
    /// Creates a transaction-only proving pipeline.
    pub fn transaction<B: Backend<Receipt = P::TransactionArtifact>>(backend: B) -> Self {
        Self::Transaction(TransactionProver::new(backend))
    }

    /// Creates a full batch proving pipeline.
    pub fn batch<B: Backend>(
        backend: B,
        store: S,
        grpc_client: GrpcClient,
        config: BatchProverConfig,
    ) -> Self
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
            BatchProver::new(backend, store, grpc_client, config),
        )
    }

    /// Submits a transaction for proving. No-op for `None`.
    pub(crate) fn submit_transaction(&self, tx: &ScheduledTransaction<S, P>, tx_inputs: Vec<u8>) {
        if let Self::Transaction(tx_prover) | Self::Batch(tx_prover, _) = self {
            tx_prover.submit(tx, tx_inputs);
        }
    }

    /// Submits a batch for proving. No-op unless in `Batch` mode.
    pub fn submit_batch(&self, batch: &ScheduledBatch<S, P>) {
        if let Self::Batch(_, batch_prover) = self {
            batch_prover.submit(batch);
        }
    }

    /// Rolls back the batch prover past `target_index`. No-op unless in `Batch` mode.
    pub fn rollback(&self, target_index: u64) {
        if let Self::Batch(_, batch_prover) = self {
            batch_prover.rollback(target_index);
        }
    }

    /// Shuts down the prover worker threads. No-op for `None`.
    pub fn shutdown(&self) {
        match self {
            Self::None => {}
            Self::Transaction(tx_prover) => tx_prover.shutdown(),
            Self::Batch(tx_prover, batch_prover) => {
                batch_prover.shutdown();
                tx_prover.shutdown();
            }
        }
    }
}
