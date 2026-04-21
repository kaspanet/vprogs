use std::sync::Arc;

use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, TransactionContext};
use vprogs_state_lane_tip::LaneTip;
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    batch_processor::StateTransition,
    transaction_processor::{Inputs, Outputs, StorageOp},
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
        Outputs::decode(&output_bytes).map(|output| {
            for (i, op) in output.storage_ops().iter().enumerate() {
                if let Some(op) = op {
                    let data = ctx.resources_mut()[i].data_mut();
                    match op {
                        StorageOp::Create(new_data) | StorageOp::Update(new_data) => {
                            data.clear();
                            data.extend_from_slice(new_data);
                        }
                        StorageOp::Delete => data.clear(),
                    }
                }
            }
        })
    }

    fn on_batch_scheduled(&self, batch: &ScheduledBatch<S, Self>) {
        self.proving_pipeline.submit_batch(batch);
    }

    fn on_batch_commit<ST: Store>(
        &self,
        _store: &ST,
        wb: &mut ST::WriteBatch,
        batch: &ScheduledBatch<S, Self>,
    ) {
        // Extract the new lane tip from the batch proof receipt's journal and persist it atomic
        // with the rest of the commit. If proving is inactive (ProvingPipeline::None) there is no
        // artifact, so we have nothing to record - the covenant path is moot in that mode.
        let Some(artifact) = batch.try_artifact() else { return };
        let journal = B::journal_bytes(&artifact);
        match StateTransition::decode(&journal) {
            Ok(StateTransition::Success { new_lane_tip, .. }) => {
                LaneTip::set(wb, batch.checkpoint().index(), &new_lane_tip);
            }
            Ok(StateTransition::Error(_)) | Err(_) => {
                // A guest-reported error or a malformed journal means the batch never produced a
                // valid lane-tip transition; nothing to persist.
            }
        }
    }

    fn on_batch_rollback<ST: Store>(&self, wb: &mut ST::WriteBatch, batch_index: u64) {
        LaneTip::delete(wb, batch_index);
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
