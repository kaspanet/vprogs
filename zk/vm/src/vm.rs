use tokio::sync::mpsc;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_storage_types::Store;
use vprogs_zk_abi::{self as abi, AccessMetadata, ResourceId, StateOp, Transaction};

use crate::{Backend, Error, ProofRequest, Result};

/// ZK processor that executes programs via a [`Backend`] and optionally sends proof requests
/// to a proving pipeline.
#[derive(Clone)]
pub struct Vm<B: Backend> {
    backend: B,
    proof_tx: Option<mpsc::UnboundedSender<ProofRequest>>,
}

impl<B: Backend> Vm<B> {
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
        // 1. Build ABI transaction context.
        let abi_ctx = abi::TransactionContext::from(&*ctx);

        // 2. Execute via backend — returns one optional op per account.
        let ops = self.backend.execute(&abi_ctx)?;

        // 3. Apply ops to resource handles.
        for (i, op) in ops.iter().enumerate() {
            if let Some(op) = op {
                let data = ctx.resources_mut()[i].data_mut();
                match op {
                    StateOp::Create(new_data) | StateOp::Update(new_data) => {
                        data.clear();
                        data.extend_from_slice(new_data);
                    }
                    StateOp::Delete => data.clear(),
                }
            }
        }

        // 4. Optionally send a proof request to the proving pipeline.
        if let Some(ref proof_tx) = self.proof_tx {
            let _ = proof_tx.send(ProofRequest { abi_ctx, ops });
        }

        Ok(())
    }

    type Transaction = Transaction;
    type TransactionEffects = ();
    type ResourceId = ResourceId;
    type AccessMetadata = AccessMetadata;
    type BatchMetadata = ChainBlockMetadata;
    type Error = Error;
}
