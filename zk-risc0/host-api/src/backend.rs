use std::sync::Arc;

use risc0_zkvm::{ExecutorEnv, ProverOpts, Receipt, default_executor, default_prover};
use vprogs_zk_abi::StateOp;
use vprogs_zk_vm::BackendError;

use crate::read_ops::read_ops;

/// RISC-0 backend for execution and proving.
///
/// Owns the transaction and batch ELF binaries. In dev mode (`RISC0_DEV_MODE=1`),
/// `prove_transaction()` generates fake receipts suitable for testing.
#[derive(Clone)]
pub struct Backend {
    transaction_elf: Arc<Vec<u8>>,
    batch_elf: Arc<Vec<u8>>,
}

impl Backend {
    pub fn new(transaction_elf: Vec<u8>, batch_elf: Vec<u8>) -> Self {
        Self { transaction_elf: Arc::new(transaction_elf), batch_elf: Arc::new(batch_elf) }
    }
}

impl vprogs_zk_vm::Backend for Backend {
    type Receipt = Receipt;

    fn execute(&self, witness_bytes: &[u8]) -> Result<Vec<Option<StateOp>>, BackendError> {
        let mut ops_stdout = Vec::new();
        let env = ExecutorEnv::builder()
            .write_slice(&[witness_bytes.len() as u32])
            .write_slice(witness_bytes)
            .stdout(&mut ops_stdout)
            .build()
            .map_err(|e| BackendError::Failed(e.to_string()))?;

        default_executor()
            .execute(env, &self.transaction_elf)
            .map_err(|e| BackendError::Failed(e.to_string()))?;

        Ok(read_ops(&ops_stdout))
    }

    fn prove_transaction(&self, witness_bytes: &[u8]) -> Result<Receipt, BackendError> {
        let env = ExecutorEnv::builder()
            .write_slice(&[witness_bytes.len() as u32])
            .write_slice(witness_bytes)
            .build()
            .map_err(|e| BackendError::Failed(e.to_string()))?;

        default_prover()
            .prove_with_opts(env, &self.transaction_elf, &ProverOpts::succinct())
            .map(|info| info.receipt)
            .map_err(|e| BackendError::Failed(e.to_string()))
    }

    fn prove_batch(
        &self,
        _block_hash: [u8; 32],
        _journals: &[Vec<u8>],
    ) -> Result<Receipt, BackendError> {
        let _elf = &self.batch_elf;
        Err(BackendError::Failed("batch proving not yet implemented".into()))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}
