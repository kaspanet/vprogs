use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    transaction_processor::{Inputs, Outputs, StorageOp},
};

use crate::{Backend, ProvingOrchestrator};

/// ZK processor that executes programs via a [`Backend`] and optionally coordinates proving.
///
/// `B` is the ZK backend. `S` is the store type (inferred from the scheduler context).
/// When proving is disabled, the orchestrator is `None` and no background threads are spawned.
#[derive(Clone)]
pub struct Vm<B: Backend, S: Store> {
    /// The ZK backend used for execution.
    backend: B,
    /// Proving orchestrator (present when proving is enabled).
    proving: Option<ProvingOrchestrator<Self, B::Receipt, S>>,
}

impl<B: Backend, S: Store> Vm<B, S> {
    /// Creates a new ZK VM with the given backend and optional proving orchestrator.
    pub fn new(backend: B, proving: Option<ProvingOrchestrator<Self, B::Receipt, S>>) -> Self {
        Self { backend, proving }
    }
}

impl<B: Backend, S: Store> Processor<S> for Vm<B, S> {
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

        // 4. Optionally register transaction for proving.
        if let Some(ref proving) = self.proving {
            proving.register_transaction(ctx.batch(), input_bytes);
        }

        return_value
    }

    type Transaction = L1Transaction;
    type TransactionEffects = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
