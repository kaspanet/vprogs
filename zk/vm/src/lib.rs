mod types;
mod witness_builder;

use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, default_executor};
use tokio::sync::mpsc;
pub use types::*;
use vprogs_scheduling_scheduler::{AccessHandle, ProcessingContext, RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_types::ProofRequest;

/// ZK VM that executes guest programs via the RISC-0 executor and optionally sends proof requests
/// to a proving pipeline.
#[derive(Clone)]
pub struct ZkVm {
    elf: Arc<Vec<u8>>,
    proof_tx: Option<mpsc::UnboundedSender<ProofRequest>>,
}

impl ZkVm {
    /// Creates a new ZK VM with the given guest ELF binary.
    pub fn new(elf: Vec<u8>) -> Self {
        Self { elf: Arc::new(elf), proof_tx: None }
    }

    /// Creates a new ZK VM that sends proof requests to the given channel after execution.
    pub fn with_proof_channel(elf: Vec<u8>, proof_tx: mpsc::UnboundedSender<ProofRequest>) -> Self {
        Self { elf: Arc::new(elf), proof_tx: Some(proof_tx) }
    }
}

impl VmInterface for ZkVm {
    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        resources: &mut [AccessHandle<S, Self>],
        ctx: &ProcessingContext<Self>,
    ) -> Result<ZkTransactionEffects, ZkVmError> {
        // 1. Build the witness from transaction data and resource state.
        let witness_bytes = witness_builder::build_witness(resources, ctx);

        // 2. Set up the RISC-0 executor environment with the witness as stdin.
        let env = ExecutorEnv::builder()
            .write_slice(&witness_bytes)
            .build()
            .map_err(|e| ZkVmError::ExecutorFailed(e.to_string()))?;

        // 3. Execute the guest (no proof generation, just execution).
        let session = default_executor()
            .execute(env, &self.elf)
            .map_err(|e| ZkVmError::ExecutorFailed(e.to_string()))?;

        // 4. Extract the journal bytes from the session.
        let journal_bytes = session.journal.bytes;

        // 5. Optionally send a proof request to the proving pipeline.
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
