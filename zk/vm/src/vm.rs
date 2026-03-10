use tokio::sync::mpsc;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    Error, Result,
    transaction_processor::{Input, Output, StorageOp},
};

use crate::{Backend, ProofRequest};

/// ZK processor that executes programs via a [`Backend`] and optionally sends proof requests
/// to a proving pipeline.
#[derive(Clone)]
pub struct Vm<B> {
    /// The ZK backend used for execution and proving.
    backend: B,
    /// Optional channel for sending proof requests to the proving pipeline.
    proof_tx: Option<mpsc::UnboundedSender<ProofRequest>>,
}

impl<B> Vm<B> {
    /// Creates a new ZK VM with the given backend.
    pub fn new(backend: B) -> Self {
        Self { backend, proof_tx: None }
    }

    /// Creates a new ZK VM that sends proof requests to the given channel after execution.
    pub fn with_proof_channel(backend: B, proof_tx: mpsc::UnboundedSender<ProofRequest>) -> Self {
        Self { backend, proof_tx: Some(proof_tx) }
    }
}

impl<B: Backend> Processor for Vm<B> {
    fn process_transaction<S: Store>(&self, ctx: &mut TransactionContext<S, Self>) -> Result<()> {
        // 1. Encode into ABI wire format.
        let wire_bytes = Input::encode(&*ctx);

        // 2. Execute via backend (returns raw bytes).
        let execution_result_bytes = self.backend.execute_transaction(&wire_bytes);

        // 3. Decode and apply storage operations on success.
        let return_value = Output::decode(&execution_result_bytes).map(|output| {
            for (i, op) in output.ops().iter().enumerate() {
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

        // 4. Optionally send a proof request to the proving pipeline.
        if let Some(ref proof_tx) = self.proof_tx {
            let resource_indices = ctx.resources().iter().map(|r| r.resource_index()).collect();
            let _ = proof_tx.send(ProofRequest {
                wire_bytes,
                block_hash: ctx.batch_metadata().block_hash().as_bytes(),
                tx_index: ctx.tx_index(),
                execution_result_bytes,
                resource_indices,
            });
        }

        return_value
    }

    type Transaction = L1Transaction;
    type TransactionEffects = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
