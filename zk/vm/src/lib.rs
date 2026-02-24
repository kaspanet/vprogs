mod types;
mod witness_builder;

use std::sync::Arc;

use tokio::sync::mpsc;
pub use types::*;
use vprogs_scheduling_scheduler::{AccessHandle, ProcessingContext, RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_proof_provider::ZkBackend;
use vprogs_zk_types::ProofRequest;

/// ZK VM that executes programs via a [`ZkBackend`] and optionally sends proof requests
/// to a proving pipeline.
#[derive(Clone)]
pub struct ZkVm<B: ZkBackend> {
    backend: B,
    elf: Arc<Vec<u8>>,
    proof_tx: Option<mpsc::UnboundedSender<ProofRequest>>,
}

impl<B: ZkBackend> ZkVm<B> {
    /// Creates a new ZK VM with the given backend and ELF binary.
    pub fn new(backend: B, elf: Vec<u8>) -> Self {
        Self { backend, elf: Arc::new(elf), proof_tx: None }
    }

    /// Creates a new ZK VM that sends proof requests to the given channel after execution.
    pub fn with_proof_channel(
        backend: B,
        elf: Vec<u8>,
        proof_tx: mpsc::UnboundedSender<ProofRequest>,
    ) -> Self {
        Self { backend, elf: Arc::new(elf), proof_tx: Some(proof_tx) }
    }
}

impl<B: ZkBackend> VmInterface for ZkVm<B> {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        resources: &mut [AccessHandle<S, Self>],
        ctx: &ProcessingContext<Self>,
    ) -> Result<ZkTransactionEffects, ZkVmError> {
        // 1. Build the witness from transaction data and resource state.
        let witness_bytes = witness_builder::build_witness(resources, ctx);

        // 2. Execute the program (no proof generation, just execution).
        let journal_bytes = self
            .backend
            .execute(&self.elf, &witness_bytes)
            .map_err(|e| ZkVmError::ExecutorFailed(e.to_string()))?;

        // 3. Optionally send a proof request to the proving pipeline.
        if let Some(ref proof_tx) = self.proof_tx {
            let _ = proof_tx.send(ProofRequest {
                batch_index: ctx.batch_metadata().batch_index,
                tx_index: ctx.tx_index(),
                journal_bytes: journal_bytes.clone(),
                witness_bytes,
            });
        }

        Ok(ZkTransactionEffects { journal_bytes })
    }

    fn post_process_batch<S: Store<StateSpace = StateSpace>>(
        &self,
        _batch: &RuntimeBatch<S, Self>,
    ) {
    }

    type Transaction = ZkTransaction;
    type TransactionEffects = ZkTransactionEffects;
    type ResourceId = ZkResourceId;
    type AccessMetadata = ZkAccessMetadata;
    type BatchMetadata = ZkBatchMetadata;
    type Error = ZkVmError;
}
