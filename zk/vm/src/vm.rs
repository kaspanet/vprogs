use tokio::sync::mpsc;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_abi::{AccessMetadata, ResourceId, StateOp, Transaction, Witness};

use crate::{Backend, Error, ProofRequest};

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
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        ctx: &mut TransactionContext<S, Self>,
    ) -> Result<(), Error> {
        // 1. Serialize witness to rkyv bytes.
        let witness = Witness::from(&*ctx);
        let witness_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&witness).unwrap().to_vec();

        // 2. Execute via backend — returns one optional op per account.
        let ops = self
            .backend
            .execute(&witness_bytes)
            .map_err(|e| Error::ExecutorFailed(e.to_string()))?;

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
            let _ = proof_tx.send(ProofRequest {
                block_hash: ctx.batch_metadata().hash().as_bytes(),
                tx_index: ctx.tx_index(),
                witness_bytes,
                ops,
            });
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
