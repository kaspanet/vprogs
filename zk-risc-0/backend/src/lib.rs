use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_scheduling_scheduler::{AccessHandle, TransactionContext};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_vm::{BackendError, ExecutionResult, ZkBackend, ZkVm, witness_builder};

/// RISC-0 backend for execution and proving.
///
/// Owns the transaction and batch ELF binaries. In dev mode (`RISC0_DEV_MODE=1`),
/// `prove_transaction()` generates fake receipts suitable for testing.
#[derive(Clone)]
pub struct Risc0Backend {
    transaction_elf: Arc<Vec<u8>>,
    batch_elf: Arc<Vec<u8>>,
}

impl Risc0Backend {
    pub fn new(transaction_elf: Vec<u8>, batch_elf: Vec<u8>) -> Self {
        Self { transaction_elf: Arc::new(transaction_elf), batch_elf: Arc::new(batch_elf) }
    }
}

impl ZkBackend for Risc0Backend {
    type Receipt = Receipt;

    fn execute_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        resources: &[AccessHandle<S, ZkVm<Self>>],
        ctx: &TransactionContext<ZkVm<Self>>,
    ) -> Result<ExecutionResult, BackendError> {
        let witness_bytes = witness_builder::build_witness(resources, ctx);
        let env = build_env(&witness_bytes)?;
        let session = default_executor()
            .execute(env, &self.transaction_elf)
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        Ok(ExecutionResult { journal_bytes: session.journal.bytes, witness_bytes })
    }

    fn prove_transaction(&self, witness_bytes: &[u8]) -> Result<Receipt, BackendError> {
        let env = build_env(witness_bytes)?;
        default_prover()
            .prove_with_opts(env, &self.transaction_elf, &ProverOpts::succinct())
            .map(|info| info.receipt)
            .map_err(|e| BackendError::Failed(e.to_string()))
    }

    fn prove_batch(
        &self,
        _batch_index: u64,
        _journals: &[Vec<u8>],
    ) -> Result<Receipt, BackendError> {
        Err(BackendError::Failed("batch proving not yet implemented".into()))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}

fn build_env(witness_bytes: &[u8]) -> Result<ExecutorEnv<'static>, BackendError> {
    ExecutorEnv::builder()
        .write(&witness_bytes.to_vec())
        .map_err(|e| BackendError::Failed(e.to_string()))?
        .build()
        .map_err(|e| BackendError::Failed(e.to_string()))
}
