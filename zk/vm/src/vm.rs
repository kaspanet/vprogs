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

        // Decode and apply storage operations.
        Outputs::decode(&output_bytes, ctx.resources().len()).map(|output| {
            for (resource, op) in ctx.resources_mut().iter_mut().zip(output.storage_ops) {
                if let Some(new_data) = op {
                    resource.set_data(new_data);
                    log::trace!(
                        "executed: resource_index={} version={} data={}",
                        resource.resource_index(),
                        resource.version(),
                        faster_hex::hex_string(resource.data()),
                    );
                }
            }
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

    fn tx_image_id(&self) -> [u8; 32] {
        *self.backend.image_id()
    }

    fn batch_image_id(&self) -> [u8; 32] {
        *self.backend.batch_image_id()
    }

    /// Restore is safe only without proving; any proving mode needs per-tx pre-images.
    fn supports_restore(&self) -> bool {
        matches!(*self.proving_pipeline, ProvingPipeline::None)
    }

    type Transaction = L1Transaction;
    type TransactionArtifact = B::Receipt;
    type BatchArtifact = B::Receipt;
    type AggregatorArtifact = B::Receipt;
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
