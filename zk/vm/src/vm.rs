use tokio::sync::mpsc;
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{StorageOp, host};

use crate::{Backend, Error, ProofRequest, Result};

/// ZK processor that executes programs via a [`Backend`] and optionally sends proof requests
/// to a proving pipeline.
#[derive(Clone)]
pub struct Vm<B> {
    backend: B,
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
        let wire_bytes = host::encode_transaction_context(&*ctx);

        // 2. Execute via backend — returns one optional storage operation per account.
        let storage_ops = self.backend.execute_transaction(&wire_bytes)?;

        // 3. Apply storage operations to resource handles.
        for (i, storage_op) in storage_ops.iter().enumerate() {
            if let Some(op) = storage_op {
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

        // 4. Optionally send a proof request to the proving pipeline.
        if let Some(ref proof_tx) = self.proof_tx {
            let _ = proof_tx.send(ProofRequest {
                wire_bytes,
                block_hash: ctx.batch_metadata().block_hash().as_bytes(),
                tx_index: ctx.tx_index(),
                storage_ops,
            });
        }

        Ok(())
    }

    type Transaction = L1Transaction;
    type TransactionEffects = ();
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
