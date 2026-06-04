use std::sync::Arc;

use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    transaction_processor::{Inputs, Outputs},
};

use crate::{Backend, ProvingPipeline};

/// ZK processor that executes programs and optionally coordinates proving via [`ProvingPipeline`].
#[derive(Clone)]
pub struct Vm<B: Backend, S: Store> {
    /// The ZK backend used for execution and proving.
    backend: B,
    /// Proving strategy (None, Transaction-only, or full Batch).
    proving_pipeline: Arc<ProvingPipeline<S, Self>>,
}

impl<B: Backend, S: Store> Vm<B, S> {
    /// Creates a new ZK VM with the given backend and proving pipeline.
    pub fn new(backend: B, proving_pipeline: ProvingPipeline<S, Self>) -> Self {
        Self { backend, proving_pipeline: Arc::new(proving_pipeline) }
    }
}

impl<B: Backend, S: Store> Processor<S> for Vm<B, S> {
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>) -> Result<()> {
        // Encode into ABI wire format.
        let input_bytes = Inputs::encode(&*ctx);

        // Execute via backend.
        let output_bytes = self.backend.execute_transaction(&input_bytes);

        // Submit to proving pipeline (no-op if ProvingPipeline::None).
        self.proving_pipeline.submit_transaction(ctx.scheduled_tx(), input_bytes);

        // Decode and apply storage operations, one per accessed resource.
        Outputs::decode(&output_bytes, ctx.resources().len()).map(|output| {
            output
                .storage_ops
                .into_iter()
                .zip(ctx.resources_mut())
                .filter_map(|(new_data, resource)| new_data.map(|new_data| (new_data, resource)))
                // TODO: is this fine to skip rewriting?
                .filter(|(new_data, resource)| resource.data() != new_data)
                .for_each(|(new_data, resource)| resource.set_data(new_data));
        })
    }

    fn on_batch_scheduled(&self, batch: &ScheduledBatch<S, Self>) {
        self.proving_pipeline.submit_batch(batch);
    }

    fn on_rollback(&self, target_index: u64) {
        self.proving_pipeline.rollback(target_index);
    }

    fn on_shutdown(&self) {
        self.proving_pipeline.shutdown();
    }

    type Transaction = L1Transaction;
    type TransactionArtifact = B::Receipt;
    type BatchArtifact = B::Receipt;
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
