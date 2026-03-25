use std::sync::Arc;

use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    transaction_processor::{Inputs, Outputs, StorageOp},
};

use crate::{Backend, ProvingPipeline};

/// ZK processor that executes programs and optionally coordinates proving via [`ProvingPipeline`].
#[derive(Clone)]
pub struct Vm<B: Backend, S: Store> {
    /// The ZK backend used for execution and proving.
    backend: B,
    /// Proving strategy (None, Transaction-only, or full Batch).
    proving: Arc<ProvingPipeline<S, Self>>,
}

impl<B: Backend, S: Store> Vm<B, S> {
    /// Creates a new ZK VM with the given backend and proving pipeline.
    pub fn new(backend: B, proving: ProvingPipeline<S, Self>) -> Self {
        Self { backend, proving: Arc::new(proving) }
    }

    /// Processes a single transaction against the ZK backend.
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>) -> Result<()> {
        // Encode into ABI wire format.
        let input_bytes = Inputs::encode(&*ctx);

        // Execute via backend.
        let output_bytes = self.backend.execute_transaction(&input_bytes);

        // Submit to proving pipeline (no-op if ProvingPipeline::None).
        self.proving.submit(ctx.scheduled_tx(), input_bytes);

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

    /// Signals the proving pipeline to shut down.
    pub fn shutdown(&self) {
        self.proving.shutdown();
    }
}

impl<B: Backend, S: Store> Processor<S> for Vm<B, S> {
    type Transaction = L1Transaction;
    type TransactionEffects = B::Receipt;
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;

    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>) -> Result<()> {
        Self::process_transaction(self, ctx)
    }
}
