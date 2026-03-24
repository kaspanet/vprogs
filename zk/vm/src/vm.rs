use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    transaction_processor::{Inputs, Outputs, StorageOp},
};
use vprogs_zk_transaction_prover::TransactionBackend;

use crate::{ExecutionBackend, ProvingPipeline};

/// ZK processor that executes programs via an [`ExecutionBackend`] and optionally coordinates
/// proving.
///
/// `EB` is the execution backend (synchronous). `TB` is the transaction/batch proving backend
/// (async). `S` is the store type (inferred from the scheduler context). The proving strategy
/// is selected via [`ProvingPipeline`].
#[derive(Clone)]
pub struct Vm<EB: ExecutionBackend, TB: TransactionBackend, S: Store> {
    /// The ZK backend used for execution.
    backend: EB,
    /// Proving strategy (None, Transaction-only, or full Batch).
    proving_pipeline: ProvingPipeline<Self, TB, S>,
}

impl<EB: ExecutionBackend, TB: TransactionBackend, S: Store> Vm<EB, TB, S> {
    /// Creates a new ZK VM with the given execution backend and proving pipeline.
    pub fn new(backend: EB, proving_pipeline: ProvingPipeline<Self, TB, S>) -> Self {
        Self { backend, proving_pipeline }
    }
}

impl<EB: ExecutionBackend, TB: TransactionBackend, S: Store> Processor<S> for Vm<EB, TB, S> {
    fn process_transaction(&self, ctx: &mut TransactionContext<S, Self>) -> Result<()> {
        // 1. Encode into ABI wire format.
        let input_bytes = Inputs::encode(&*ctx);

        // 2. Execute via backend (returns raw bytes).
        let output_bytes = self.backend.execute_transaction(&input_bytes);

        // 3. Decode and apply storage operations on success.
        let return_value = Outputs::decode(&output_bytes).map(|output| {
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
        });

        // 4. Submit transaction to proving pipeline (no-op if ProvingPipeline::None).
        self.proving_pipeline.submit(ctx.batch(), input_bytes);

        return_value
    }

    type Transaction = L1Transaction;
    type TransactionEffects = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
